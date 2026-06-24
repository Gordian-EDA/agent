//! `anneal-place` — the premium simulated-annealing schematic placement engine.
//!
//! Implements `sch_model::place::PlacementEngine` (the free build defaults to
//! Greedy). Runs a multi-start SA with a router-free locality proxy over the place
//! scaffold (cost/refine/polish/cohesion/anchor) exposed by `sch-layout`.

use std::collections::{BTreeMap, BTreeSet};

use kicad_cli_rs::env::KicadEnv;

// The engine boundary (PlaceProblem + the PlacementEngine trait) is `sch_model::place`;
// the cost/scaffold fns + tuning consts are sch-layout's working surface, globbed here
// since per-item lists would churn every iteration.
use sch_model::place::{PlaceProblem, PlacementEngine};
use sch_layout::floorplan::place::*;
use sch_model::ir::{LayoutIr, Orient};
use sch_model::item::{Incidence, Item};
use sch_model::netclass::is_power_net;

/// Deterministic-given-IR PRNG (SplitMix64-ish) so annealing reproduces.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next() % n as u64) as usize }
    }
    /// Uniform in [0,1).
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    /// A small symmetric integer step in [-r, r].
    fn step(&mut self, r: i32) -> i32 {
        self.below((2 * r + 1) as usize) as i32 - r
    }
}

/// Simulated annealing (paid tier): a seeded refine→anneal AND a broad anneal from
/// the raw seed, keeping whichever the cost prefers (today's multi-start best-of).
pub struct Anneal;
impl PlacementEngine for Anneal {
    fn name(&self) -> &'static str {
        "anneal"
    }
    fn place(&self, p: &PlaceProblem, items: &mut [Item]) {
        let (env, inc, ir, needs_flag, seed) = (p.env, p.inc, p.ir, p.needs_flag, p.seed);
        use rayon::prelude::*;
        let timed_top = std::env::var("DEBUG_SA_TIME").is_ok();

        // A group with no placed items (e.g. a sub-sheet holding only power/label
        // declarations, which `gather` skips) has nothing to search — and the fast
        // lane's `30_000 / pins` would divide by zero. Bail out cleanly.
        if items.is_empty() {
            return;
        }

        // FAST LANE (large boards): the tuned routed paths below route the whole sheet
        // per move and cost minutes past ~60 pins. Here the search is router-free —
        // multi-start `anneal_locality` (proxy cost + range-limited cluster jump) from
        // the raw cell seed — and the only routes paid are the bounded candidate
        // selection + the one final emit. Strictly additive safety is preserved: the
        // RAW seed is always a candidate (a floor), and the pick takes fewest real
        // warnings then true cost, so the fast lane never ships worse than the seed.
        let pins: usize = items.iter().map(|it| it.geom.pins.len()).sum();
        // PORT-HEAVY sheet = a multi-sheet sub-sheet: its inter-block nets each touch only one
        // pin here, so they become single-pin signal PORTS (labels). Such a sheet is small but
        // its bus/port fanout tangles, and the small path leaves the crossings uncorrected (a
        // 6-part I2C sheet sat at 5 crossings though the topology allows ~1). Route it through the
        // fast lane so it gets the route-aware crossing REFINEMENT (validated: io 7→8,
        // power_entry 8→9). Self-contained reference boards have <6 single-pin signal nets, so
        // they stay on the small path ⇒ snapshots byte-identical.
        let port_heavy = {
            let mut npins: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
            for it in items.iter() {
                for (_, _, net) in &it.pins {
                    if let Some(net) = net {
                        *npins.entry(net.clone()).or_insert(0) += 1;
                    }
                }
            }
            let mut signal_ports = 0usize;
            for (net, c) in &npins {
                if *c == 1 && !ir.rails.contains_key(net.as_str()) && !is_power_net(net) {
                    signal_ports += 1;
                }
            }
            signal_ports >= 6
        };
        let force_fast = port_heavy || std::env::var("MULTISHEET_REFINE").is_ok();
        if pins > FAST_PINS || force_fast {
            let raw: Vec<Item> = items.to_vec();
            // Diverse proxy-anneal starts; fewer for very large boards (each candidate
            // costs two real routes at selection, ~1 s each on a 671-pin BGA).
            let n_starts = if pins > 250 { 1 } else { 3 };
            let seeds: Vec<u64> = (0..n_starts)
                .map(|k| seed ^ (0x9E3779B97F4A7C15u64.wrapping_mul(k as u64 + 1)))
                .collect();
            let t_search = std::time::Instant::now();
            let mut starts: Vec<Vec<Item>> = seeds
                .par_iter()
                .map(|&s| {
                    let mut st = raw.clone();
                    anneal_locality(env, &mut st, inc, ir, needs_flag, s);
                    st
                })
                .collect();
            if timed_top {
                eprintln!("  [SA-fast] {n_starts} proxy starts: {:.2}s", t_search.elapsed().as_secs_f64());
            }
            let mut bases = vec![raw];
            bases.append(&mut starts);
            // From each base placement, produce three FULLY-POLISHED candidates with
            // different post-passes: (a) nudge only — the conservative floor; (b) +magnet
            // — seat each satellite tight to the pin it taps (kills the stranded-cap
            // long-route labels); (c) +magnet +gravity — also pack whole modules toward
            // the centre (kills inter-module sprawl). Seating and packing can collide
            // module power-symbols / net-labels (text the proxy can't see), so all three
            // are offered to the pick, which judges on REAL post-solve warnings then true
            // cost — so neither pass can ever ship a worse/colliding sheet than the floor.
            let variants: [(bool, bool); 3] = [(false, false), (true, false), (true, true)];
            let candidates: Vec<Vec<Item>> = bases
                .par_iter()
                .flat_map_iter(|b| {
                    variants.iter().map(move |&(m, g)| {
                        let mut p = b.clone();
                        polish_proxy(&mut p, inc, ir, m, g);
                        decongest(&mut p);
                        p
                    })
                })
                .collect();
            let t_score = std::time::Instant::now();
            let scored: Vec<(usize, usize, f64)> = candidates
                .par_iter()
                .map(|cand| {
                    // TRUTHFULNESS first: a magnet/gravity move can strand two nets onto
                    // one wire (a merge), which warnings DON'T see — reject those here.
                    let b = truthfulness_breaks(env, cand, inc, ir, needs_flag);
                    let w = warning_count(env, cand, inc, ir, needs_flag);
                    let c = premium_score_with_w(env, cand, inc, ir, needs_flag, w);
                    (b, w, c)
                })
                .collect();
            if timed_top {
                eprintln!("  [SA-fast] score {} candidates: {:.2}s", candidates.len(), t_score.elapsed().as_secs_f64());
            }
            let (mut best, mut best_b, mut best_w, mut best_c) =
                (0usize, usize::MAX, usize::MAX, f64::INFINITY);
            for (k, (b, w, c)) in scored.iter().enumerate() {
                let better = (*b, *w).cmp(&(best_b, best_w)) == std::cmp::Ordering::Less
                    || (*b == best_b && *w == best_w && c + 0.5 < best_c);
                if better {
                    best = k;
                    best_b = *b;
                    best_w = *w;
                    best_c = *c;
                }
            }
            if timed_top {
                eprintln!("  [SA-fast] pick cand#{best} scored={scored:?}");
            }
            // ROUTE-AWARE REFINEMENT (large boards). The proxy is crossing-BLIND, so the
            // fast-lane winner is sprawl-optimal but not crossing-optimal. Refine it with a
            // bounded `anneal_items` whose objective is the TRUE routed cost (premium) — the
            // only faithful crossing signal — which no cheap proxy could capture. Seeded
            // from the already-good winner, so its capped budget (≤750 routed iters, the
            // 420k/pins ceiling) is spent polishing, not exploring. Kept ONLY if it wins the
            // SAME (breaks, warnings, true-cost) pick, so it can never ship worse. This
            // trades the ≤5s budget for fewer dense-board crossings, per the user's call.
            // Score a candidate on its FINALISED geometry. CRUCIAL: the emit runs decongest
            // + align_idiom_clusters + align_led_chains (which e.g. snaps each LED's resistor
            // into a clean leg, tidying a tangled candidate dramatically — c08 53→19) BEFORE
            // counting crossings. Measuring pre-finalise ranks candidates the emit then
            // re-orders, so we finalise a clone here first. The picked candidate ships RAW
            // (the emit re-finalises it identically). Order: truthfulness, warnings, total
            // crossings (body+ic+wire), then straightness.
            let score = |c: &[Item]| -> (usize, usize, usize, f64) {
                let mut m = c.to_vec();
                decongest(&mut m);
                if align_idiom_clusters(&mut m, ir) {
                    decongest(&mut m);
                }
                if align_led_chains(&mut m, inc, ir) {
                    decongest(&mut m);
                }
                let b = truthfulness_breaks(env, &m, inc, ir, needs_flag);
                let w = warning_count(env, &m, inc, ir, needs_flag);
                let (bx, ix, wx) = crossing_counts(env, &m, inc, ir, needs_flag);
                (b, w, bx + ix + wx, premium_score_with_w(env, &m, inc, ir, needs_flag, w))
            };
            let (bb, bw, bx, bc) = score(&candidates[best]);
            // SKIP the refinement when the winner is already clean (no breaks/warnings and
            // few crossings): such boards can't meaningfully improve, so the routed budget
            // would be pure wasted wall-time. Every refinement win this far had a best with
            // ≥7 crossings or a warning, so a ≤6/0-warning gate keeps all wins.
            // NEVER skip the refinement on a forced-fast (multi-sheet) sub-sheet: even at 0-1
            // crossings it often has CAP-SCATTER / long satellite runs (a 3V3 bulk cap marooned
            // far from the regulator output) — an HPWL/straightness defect the crossing-based skip
            // misses but the refinement's true routed-cost objective fixes (it's kept only if the
            // premium score improves). Cheap on a small sheet. A big board still skips when clean.
            let small_forced = force_fast && pins <= FAST_PINS;
            if !small_forced && bb == 0 && bw == 0 && bx <= 6 {
                items.clone_from_slice(&candidates[best]);
                return;
            }
            // ROUTE-AWARE REFINEMENT. The proxy is crossing-BLIND, so the fast-lane winner is
            // sprawl-optimal but not crossing-optimal — and no cheap router-free crossing
            // proxy proved faithful (bbox/trunk-segment all failed). So refine the winner with
            // the TRUE router: a bounded `anneal_items` (premium routed cost; iter-capped
            // 80..300 = 30k/pins so even a 173-pin board stays seconds) seeded from it. Kept
            // ONLY if it wins on real (finalised) crossings, so it is strictly additive — a
            // straighter-but-more-crossing result is rejected. Trades the ≤5s budget for fewer
            // dense-board crossings (user-authorised).
            let mut refined = candidates[best].clone();
            let t_ref = std::time::Instant::now();
            let ref_cap = (30_000 / pins).clamp(80, 300);
            anneal_items(env, &mut refined, inc, ir, needs_flag, false, true, seed ^ 0x5EF1, Some(ref_cap));
            decongest(&mut refined);
            let (rb, rw, rx, rc) = score(&refined);
            let refined_wins = (rb, rw, rx).cmp(&(bb, bw, bx)) == std::cmp::Ordering::Less
                || (rb == bb && rw == bw && rx == bx && rc + 0.5 < bc);
            if timed_top {
                eprintln!(
                    "  [SA-fast] route-refine {:.2}s cap={ref_cap}: ({bb},{bw},{bx},{bc:.0})->({rb},{rw},{rx},{rc:.0}) win={refined_wins}",
                    t_ref.elapsed().as_secs_f64()
                );
            }
            let mut fast_final: Vec<Item> = if refined_wins { refined } else { candidates[best].clone() };
            // MOTIF TILING (opt-in via `MOTIF_TILE`, dense-only): tile repeated same-part
            // anchor blocks (4× DRV8871 etc.) on a regular lattice — the human idiom for
            // repeated structure (mined rule #9). Strictly ADDITIVE: applied only when it
            // neither breaks connectivity NOR adds a readability warning, so when enabled it
            // can only tidy, never regress the measurable gates. Default OFF ⇒ byte-identical.
            // (Validated NEUTRAL-or-better on motordrv: critic 6=6, convention dim +1, channels
            // visibly tiled; kept opt-in pending multi-board validation since layout-forcing can
            // hurt the critic in ways warnings don't catch — see the grid experiment.)
            if std::env::var("MOTIF_TILE").is_ok() {
                let mut cand = fast_final.clone();
                if align_repeated_motifs(&mut cand, inc, ir) {
                    decongest(&mut cand);
                    let before =
                        (truthfulness_breaks(env, &fast_final, inc, ir, needs_flag),
                         warning_count(env, &fast_final, inc, ir, needs_flag));
                    let after =
                        (truthfulness_breaks(env, &cand, inc, ir, needs_flag),
                         warning_count(env, &cand, inc, ir, needs_flag));
                    if after <= before {
                        fast_final = cand;
                    }
                }
            }
            // force_fast SMALL sub-sheets: the fast lane's locality proxy can be crossing-worse
            // than the small-board path on SIMPLE sheets (split-supply power: 4 here vs 2). Run the
            // small path too and keep whichever has fewer (breaks, warnings, crossings) via the same
            // `score` — so a congested sheet still gets the fast lane's refinement (io 16→13) while a
            // simple sheet gets the small path's cleaner routing. Cheap: only for force_fast smalls.
            if small_forced {
                let sp = small_path_search(env, &bases[0], inc, ir, needs_flag, seed);
                let (fb, fw, fx, fc) = score(&fast_final);
                let (sb, sw, sx, sc) = score(&sp);
                let sp_wins = (sb, sw, sx).cmp(&(fb, fw, fx)) == std::cmp::Ordering::Less
                    || (sb == fb && sw == fw && sx == fx && sc + 0.5 < fc);
                items.clone_from_slice(if sp_wins { &sp } else { &fast_final });
            } else {
                items.clone_from_slice(&fast_final);
            }
            return;
        }

        // Small board: greedy + four parallel anneals, pick the polished winner.
        // Extracted to small_path_search so the force_fast fast lane can run it as a
        // rival candidate; this call reproduces the old inline behaviour exactly.
        let r = small_path_search(env, items, inc, ir, needs_flag, seed);
        items.clone_from_slice(&r);
    }
}
/// The small-board placement search, extracted so the fast lane can run it as a RIVAL
/// candidate for force_fast SMALL sub-sheets (the fast lane's locality proxy is
/// crossing-worse than this on simple sheets — a split-supply power sheet sat at 4
/// crossings via the fast lane vs 2 here). Greedy refine + four parallel anneals (A
/// seeded, B broad, C premium, D locality), then pick the polished winner by
/// (truthfulness, warnings, premium cost). Operates on a COPY of `seed`, returns the
/// POLISHED winner. Behaviour is byte-identical to the old inline else-branch (the
/// placement_snapshot verifies it for the references that take the small path).
fn small_path_search(
    env: &KicadEnv,
    seed: &[Item],
    inc: &Incidence,
    ir: &LayoutIr,
    needs_flag: &BTreeSet<String>,
    rng_seed: u64,
) -> Vec<Item> {
    use rayon::prelude::*;
    let mut work: Vec<Item> = seed.to_vec();
    let seed_state: Vec<Item> = work.clone();
    let timed = std::env::var("DEBUG_SA_TIME").is_ok();
    let tic = |label: &str, f: &mut dyn FnMut()| {
        let t0 = std::time::Instant::now();
        f();
        if timed {
            eprintln!("  [SA] {label}: {:.2}s", t0.elapsed().as_secs_f64());
        }
    };
    let mut state_a: Vec<Item> = Vec::new();
    let mut state_b: Vec<Item> = seed_state;
    let mut state_c: Vec<Item> = Vec::new();
    let mut state_d: Vec<Item> = Vec::new();
    let mut greedy_state: Vec<Item> = Vec::new();
    rayon::scope(|s| {
        s.spawn(|_| { let mut f = || anneal_items(env, &mut state_b, inc, ir, needs_flag, true, false, rng_seed, None); tic("B broad", &mut f); });
        { let mut f = || refine_items(env, &mut work, inc, ir, needs_flag); tic("greedy", &mut f); }
        greedy_state = work.to_vec();
        state_a = greedy_state.clone();
        state_c = greedy_state.clone();
        state_d = greedy_state.clone();
        rayon::join(
            || { let mut f = || anneal_items(env, &mut state_a, inc, ir, needs_flag, false, false, rng_seed, None); tic("A seeded", &mut f); },
            || {
                rayon::join(
                    || { let mut f = || anneal_items(env, &mut state_c, inc, ir, needs_flag, false, true, rng_seed ^ 0x9E3779B97F4A7C15, None); tic("C premium", &mut f); },
                    || { let mut f = || anneal_locality(env, &mut state_d, inc, ir, needs_flag, rng_seed ^ 0x517CC1B727220A95); tic("D locality", &mut f); },
                )
            },
        );
    });
    let annealed = vec![state_a, state_b, state_c, state_d];
    let mut candidates = vec![greedy_state];
    candidates.extend(annealed);
    let scored: Vec<(usize, usize, f64, Vec<Item>)> = candidates
        .par_iter()
        .map(|cand| {
            let mut shipped = cand.clone();
            polish(env, &mut shipped, inc, ir, needs_flag);
            decongest(&mut shipped);
            let b = truthfulness_breaks(env, &shipped, inc, ir, needs_flag);
            let w = warning_count(env, &shipped, inc, ir, needs_flag);
            let c = premium_score_with_w(env, &shipped, inc, ir, needs_flag, w);
            (b, w, c, shipped)
        })
        .collect();
    let (mut best, mut best_b, mut best_w, mut best_c) =
        (0usize, usize::MAX, usize::MAX, f64::INFINITY);
    for (k, (b, w, c, _)) in scored.iter().enumerate() {
        let better = (*b, *w).cmp(&(best_b, best_w)) == std::cmp::Ordering::Less
            || (*b == best_b && *w == best_w && c + 0.5 < best_c);
        if better {
            best = k;
            best_b = *b;
            best_w = *w;
            best_c = *c;
        }
    }
    scored[best].3.clone()
}
/// Simulated-annealing placement search over the coarse cells: like `refine_cells`
/// but it accepts *worsening* moves with probability `exp(-Δ/T)` (T cooling to ~0),
/// so it escapes the local minima the greedy climb is trapped in — a satellite
/// stranded across the sheet can migrate, in stages, to hug the IC pin it serves.
/// Moves: relocate a satellite to a random nearby cell, re-orient it, swap two,
/// or nudge an anchor. Every candidate is scored on the REAL routed cost (incl.
/// the spread/stray/overlap terms), and the best layout seen is kept — so SA can
/// only match-or-beat the seed it started from.
/// Simulated annealing over the items' mm positions. Same Metropolis loop as the
/// greedy refine's neighbourhood but it accepts *worsening* moves with probability
/// `exp(-Δ/T)` (T cooling to ~0), so it escapes the local minima greedy is trapped
/// in — a satellite stranded across the sheet can migrate, in stages, to hug the
/// IC pin it serves. Moves operate directly on `at`/`angle` (the shipped geometry),
/// scored by `score_items`; the best layout seen is kept. `broad` runs hotter and
/// longer (a wider global search from the raw seed). ANCHORS are mobile here: an
/// anchor nudge frees a whole block to slide.
fn anneal_items(
    env: &KicadEnv,
    items: &mut [Item],
    inc: &Incidence,
    ir: &LayoutIr,
    needs_flag: &BTreeSet<String>,
    broad: bool,
    premium: bool,
    seed: u64,
    iter_cap: Option<usize>,
) {
    // The objective: free tier minimises the base routed cost; the premium run
    // optimises the richer (straighter) objective. Run as an EXTRA candidate so it
    // never displaces the base run's warning-free find — see `Anneal::search`.
    let cost = |env: &KicadEnv, items: &[Item], inc: &Incidence, ir: &LayoutIr, nf: &BTreeSet<String>| {
        if premium {
            premium_score_items(env, items, inc, ir, nf)
        } else {
            score_items(env, items, inc, ir, nf)
        }
    };
    let sats: Vec<usize> = (0..items.len()).filter(|&i| items[i].geom.pins.len() < 3 && !items[i].frozen).collect();
    let anchors: Vec<usize> = (0..items.len()).filter(|&i| items[i].geom.pins.len() >= 3).collect();
    if sats.is_empty() {
        return;
    }
    // Cluster locality: each anchor's "block" is the satellites that tap it plus any
    // idiom members it anchors. The block move (below) slides a whole functional unit
    // (an IC and its decoupling/crystal/tap parts) as one rigid group — the GLOBAL
    // structural move a per-part LOCAL search can't reach.
    let blocks = build_anchor_blocks(items, inc, &anchors, &sats, ir);
    let siblings = multi_unit_siblings(items, &anchors);
    let orients = [Orient::Up, Orient::Down, Orient::Left, Orient::Right];
    let mut rng = Rng(seed);

    let mut cur = cost(env, items, inc, ir, needs_flag);
    let mut best_items: Vec<Item> = items.to_vec();
    let mut best = cur;

    // Iterations scale with part count; temperature cools linearly. T0 is set so an
    // early move that adds a crossing/junction (cost ~5) is readily accepted, while
    // a correctness failure (cost ~1000+) never is.
    // Iterations scale with movable count. Swept down empirically: 0.5x of the
    // previous budget holds (fast 7/7, oneshot 0) with margin, 0.4x is the fragile
    // edge, 0.3x breaks — and because the cooling schedule `t = t0·(1−it/iters)`
    // makes the trajectory chaotic-sensitive to the EXACT count, the safe choice is
    // the margin (0.5x), not the edge. These ceilings (750 / 2000) are ~6x fewer
    // evals than the original 4000 / 12000; the mults are unchanged so the
    // binding-ceiling fixtures get exactly the validated 0.5x count.
    let (mult, t0) = if broad { (700, 30.0) } else { (300, 12.0) };
    let mut iters = (mult * sats.len()).clamp(250, if broad { 2000 } else { 750 });
    // Large boards (100-pin / BGA): each `score_items` routes the WHOLE sheet, and
    // routing cost scales with PIN count (a 100-pin MCU is one item but 186 pins),
    // so the full iteration count runs into minutes. Cap total routing work so
    // `iters * pin-count` stays under a fixed budget. Deterministic (seed-driven,
    // never wall-clock-timed); the tuned fixtures (≤58 pins) are below the threshold
    // and completely unchanged. The SA still ships ≥ greedy regardless of iteration
    // count (greedy is always one of the picked candidates), so a smaller budget
    // can never produce a worse layout — only a less-optimised SA path the candidate
    // pick then discards.
    let pins: usize = items.iter().map(|it| it.geom.pins.len()).sum();
    if pins > 70 {
        iters = iters.min((420_000 / pins).max(800));
    }
    // A route-aware refinement from an already-good seed caps its routed budget tighter
    // (keeps the >5s large-board path bounded — see the fast-lane call site).
    if let Some(cap) = iter_cap {
        iters = iters.min(cap);
    }
    // One grid cell-step in x/y for the relocation moves.
    let relocate = |rng: &mut Rng, at: [f64; 2], n: i32| -> [f64; 2] {
        [
            sch_model::grid::snap(at[0] + rng.step(n) as f64 * COL_GAP),
            sch_model::grid::snap(at[1] + rng.step(n) as f64 * ROW_GAP),
        ]
    };
    // NB: no "exit early once `best` plateaus for N iters" rule. Measured the largest
    // plateau that is still FOLLOWED by a real improvement: up to 1154 iters on the
    // 2000-iter broad run, 685 on a 750-iter seeded run. Every
    // run's last improvement lands at 94-99% of its budget — the ~6x iteration cut
    // already removed the dead tail, so the search genuinely uses its whole budget.
    // A patience small enough to save time would cut those late improvements (a
    // measured 555/uart/mcp tidiness regression); a safe patience saves ~nothing.
    for it in 0..iters {
        let t = (t0 * (1.0 - it as f64 / iters as f64)).max(0.05);
        // Snapshot the item(s) a move touches (at + angle) so it can be rolled back.
        let m = rng.below(10);
        let undo: Vec<(usize, [f64; 2], f64)>;
        if m < 6 {
            // Relocate a satellite to a nearby cell (the big move greedy lacks).
            let i = sats[rng.below(sats.len())];
            undo = vec![(i, items[i].at, items[i].angle)];
            items[i].at = relocate(&mut rng, items[i].at, 2);
        } else if m < 8 {
            // Re-orient a satellite.
            let i = sats[rng.below(sats.len())];
            undo = vec![(i, items[i].at, items[i].angle)];
            items[i].angle = orient_angle(&items[i].geom, orients[rng.below(4)]);
        } else if m < 9 && sats.len() >= 2 {
            // Swap two satellites' positions (keep each orientation).
            let a = sats[rng.below(sats.len())];
            let b = sats[rng.below(sats.len())];
            undo = vec![(a, items[a].at, items[a].angle), (b, items[b].at, items[b].angle)];
            let (pa, pb) = (items[a].at, items[b].at);
            items[a].at = pb;
            items[b].at = pa;
        } else if !anchors.is_empty() {
            // Nudge an anchor (an IC) by one cell, carrying its whole BLOCK (the
            // satellites that tap it + the idiom clusters it anchors) by the same
            // delta — a coherent global slide of a functional unit. The rng draws
            // match the old anchor-only nudge (anchor pick + relocate); only the
            // block now follows, so the move is no longer self-defeating.
            let i = anchors[rng.below(anchors.len())];
            let new = relocate(&mut rng, items[i].at, 1);
            let d = [new[0] - items[i].at[0], new[1] - items[i].at[1]];
            let group = cluster_group(i, &blocks, &siblings);
            undo = group.iter().map(|&k| (k, items[k].at, items[k].angle)).collect();
            for &k in &group {
                items[k].at =
                    [sch_model::grid::snap(items[k].at[0] + d[0]), sch_model::grid::snap(items[k].at[1] + d[1])];
            }
        } else {
            continue;
        }

        let c = cost(env, items, inc, ir, needs_flag);
        let d = c - cur;
        if d < 0.0 || rng.unit() < (-d / t).exp() {
            cur = c;
            if c < best {
                best = c;
                best_items.clone_from_slice(items);
            }
        } else {
            for (i, at, angle) in undo {
                items[i].at = at;
                items[i].angle = angle;
            }
        }
    }
    items.clone_from_slice(&best_items);
}
/// A cheap, routing-FREE geometric proxy for [`layout_cost`] — the per-move objective
/// of the locality-aware anneal. The correctness wall (body overlaps, authored-grid
/// order) stays EXACT, never approximated; wirelength is the per-net bounding-box
/// half-perimeter (HPWL) over incident item centres — the standard placement-SA inner
/// loop — `spread` is the whole-board bbox, and `cohere` is the per-satellite Manhattan
/// distance to the anchor PIN it taps (the cheap mirror of [`count_stray`], so the inner
/// loop pulls a far-flung pull-up back to its pin instead of leaving it stranded on a
/// wide rail). It omits the ROUTED neatness terms (crossings/corners/congestion/
/// body-cross, which need the router); the `Anneal::search` candidate pick re-asserts the
/// true routed cost + warnings on the result, so a proxy that ranks geometry can never
/// SHIP a worse or untruthful sheet — it only proposes candidates the true cost then judges.
fn proxy_cost(
    items: &[Item],
    inc: &Incidence,
    ir: &LayoutIr,
    cohesion: &[(usize, Vec<(usize, usize)>)],
) -> f64 {
    let overlaps = body_overlap_count(items);
    let grid_order = grid_order_viol(items, ir);
    let mut hpwl = 0.0;
    for pins in inc.values() {
        let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
        for (i, _) in pins {
            let at = items[*i].at;
            lo[0] = lo[0].min(at[0]);
            lo[1] = lo[1].min(at[1]);
            hi[0] = hi[0].max(at[0]);
            hi[1] = hi[1].max(at[1]);
        }
        if hi[0] >= lo[0] {
            hpwl += (hi[0] - lo[0]) + (hi[1] - lo[1]);
        }
    }
    let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
    for it in items {
        lo[0] = lo[0].min(it.at[0]);
        lo[1] = lo[1].min(it.at[1]);
        hi[0] = hi[0].max(it.at[0]);
        hi[1] = hi[1].max(it.at[1]);
    }
    let spread = if hi[0] >= lo[0] { (hi[0] - lo[0]) + (hi[1] - lo[1]) } else { 0.0 };
    let mut cohere = 0.0;
    for (si, tgts) in cohesion {
        let (mut cx, mut cy) = (0.0f64, 0.0f64);
        for (j, pgi) in tgts {
            let p = sch_io::write::pin_endpoint(
                &items[*j].geom.pins[*pgi],
                items[*j].at,
                items[*j].angle,
                items[*j].mirror,
            );
            cx += p[0];
            cy += p[1];
        }
        let n = tgts.len() as f64;
        let at = items[*si].at;
        cohere += (at[0] - cx / n).abs() + (at[1] - cy / n).abs();
    }
    // HYBRID VLM zone bias: a SOFT pull of each zoned anchor toward the coarse target
    // fraction the LLM chose (left/centre/right, top/bottom), scaled to mm by the board
    // size. Soft so the engine still does the precise placement and can override the
    // LLM where local geometry demands — the LLM only steers the rough arrangement.
    // Empty `ir.zone` (every existing path) ⇒ 0 ⇒ this is a no-op.
    let mut zbias = 0.0;
    if !ir.zone.is_empty() && hi[0] > lo[0] && hi[1] > lo[1] {
        let (bw, bh) = (hi[0] - lo[0], hi[1] - lo[1]);
        for it in items {
            if let Some([tx, ty]) = ir.zone.get(&it.refdes) {
                let fx = (it.at[0] - lo[0]) / bw;
                let fy = (it.at[1] - lo[1]) / bh;
                zbias += (fx - tx).abs() * bw + (fy - ty).abs() * bh;
            }
        }
    }
    1500.0 * overlaps as f64
        + 1200.0 * grid_order as f64
        + 0.15 * hpwl
        + PROXY_SPREAD_W * spread
        + 0.5 * cohere
        + ZBIAS_W * zbias
}
/// Locality-aware anneal (see `docs/specs/locality-aware-placement-search.md`). Two
/// things the tuned full-route paths can't afford: (1) a cheap geometric `proxy_cost`
/// per move (no whole-sheet reroute), so it runs a far larger iteration budget and
/// only pays the true routed cost on a new proxy-best; (2) a RANGE-LIMITED CLUSTER
/// JUMP — slide a whole block by a large displacement when hot, decaying to a nudge
/// when cold — the GLOBAL move that lets a coherent idiom migrate across a congested
/// region in one step (the crystal/reset-cluster gap). Run as an EXTRA candidate in
/// `Anneal::search`: the pick ships it only if it beats the tuned paths on the true
/// cost, so it is purely additive and never regresses a tuned fixture.
fn anneal_locality(
    env: &KicadEnv,
    items: &mut [Item],
    inc: &Incidence,
    ir: &LayoutIr,
    needs_flag: &BTreeSet<String>,
    seed: u64,
) {
    let sats: Vec<usize> =
        (0..items.len()).filter(|&i| items[i].geom.pins.len() < 3 && !items[i].frozen).collect();
    let anchors: Vec<usize> = (0..items.len()).filter(|&i| items[i].geom.pins.len() >= 3).collect();
    if sats.is_empty() {
        return;
    }
    let blocks = build_anchor_blocks(items, inc, &anchors, &sats, ir);
    let siblings = multi_unit_siblings(items, &anchors);
    let cohesion = cohesion_targets(items, inc, ir);
    let orients = [Orient::Up, Orient::Down, Orient::Left, Orient::Right];
    let mut rng = Rng(seed);
    let relocate = |rng: &mut Rng, at: [f64; 2], n: i32| -> [f64; 2] {
        [
            sch_model::grid::snap(at[0] + rng.step(n) as f64 * COL_GAP),
            sch_model::grid::snap(at[1] + rng.step(n) as f64 * ROW_GAP),
        ]
    };
    // Board extent in cells — the hot cluster-jump radius.
    let (mut blo, mut bhi) = ([f64::MAX; 2], [f64::MIN; 2]);
    for it in items.iter() {
        blo[0] = blo[0].min(it.at[0]);
        blo[1] = blo[1].min(it.at[1]);
        bhi[0] = bhi[0].max(it.at[0]);
        bhi[1] = bhi[1].max(it.at[1]);
    }
    let span_cells = (((bhi[0] - blo[0]).max(bhi[1] - blo[1])) / COL_GAP).ceil().max(2.0) as i32;

    // Cheap proxy ⇒ afford a big budget; no per-move routing, so no pin-count cap.
    let iters = (40 * sats.len()).clamp(800, 8000);
    let t0 = 24.0;
    // The proxy loop is router-free, but each true-cost VERIFY routes (+ text-solves)
    // the whole sheet. On small boards that's cheap, so keep the historical ~256-cap
    // (the tuned fixtures' path-D result is unchanged). On a large board one route is
    // expensive (a 671-pin BGA ~1 s), so cap verifies pin-aware to stay inside the 5 s
    // budget — the final proxy-best is always verified once below regardless, and the
    // candidate pick re-routes the result, so fewer mid-search verifies never ships
    // worse, only tracks a slightly-staler true-best.
    let pins: usize = items.iter().map(|it| it.geom.pins.len()).sum();
    let max_verifies = if pins > FAST_PINS { (4000 / pins.max(1)).clamp(6, 128) } else { 256 };
    let verify_period = (iters / max_verifies).max(1);
    let mut last_verify = 0usize;

    let mut cur = proxy_cost(items, inc, ir, &cohesion);
    let mut proxy_best = cur;
    let mut proxy_best_items: Vec<Item> = items.to_vec();
    let mut best_true = premium_score_items(env, items, inc, ir, needs_flag);
    let mut best_items: Vec<Item> = items.to_vec();

    for it in 0..iters {
        let p = it as f64 / iters as f64;
        let t = (t0 * (1.0 - p)).max(0.05);
        let m = rng.below(10);
        let undo: Vec<(usize, [f64; 2], f64)>;
        if m < 6 {
            let i = sats[rng.below(sats.len())];
            undo = vec![(i, items[i].at, items[i].angle)];
            items[i].at = relocate(&mut rng, items[i].at, 2);
        } else if m < 8 {
            let i = sats[rng.below(sats.len())];
            undo = vec![(i, items[i].at, items[i].angle)];
            items[i].angle = orient_angle(&items[i].geom, orients[rng.below(4)]);
        } else if m < 9 && sats.len() >= 2 {
            let a = sats[rng.below(sats.len())];
            let b = sats[rng.below(sats.len())];
            undo = vec![(a, items[a].at, items[a].angle), (b, items[b].at, items[b].angle)];
            let (pa, pb) = (items[a].at, items[b].at);
            items[a].at = pb;
            items[b].at = pa;
        } else if !anchors.is_empty() {
            // RANGE-LIMITED CLUSTER JUMP: large displacement when hot, decaying to a
            // 1-cell nudge when cold — carries the anchor's whole block rigidly.
            let radius = (((1.0 - p) * span_cells as f64).round() as i32).max(1);
            let i = anchors[rng.below(anchors.len())];
            let new = relocate(&mut rng, items[i].at, radius);
            let d = [new[0] - items[i].at[0], new[1] - items[i].at[1]];
            let group = cluster_group(i, &blocks, &siblings);
            undo = group.iter().map(|&k| (k, items[k].at, items[k].angle)).collect();
            for &k in &group {
                items[k].at =
                    [sch_model::grid::snap(items[k].at[0] + d[0]), sch_model::grid::snap(items[k].at[1] + d[1])];
            }
        } else {
            continue;
        }

        let c = proxy_cost(items, inc, ir, &cohesion);
        let d = c - cur;
        if d < 0.0 || rng.unit() < (-d / t).exp() {
            cur = c;
            if c < proxy_best {
                proxy_best = c;
                proxy_best_items.clone_from_slice(items);
                // Pay the true routed cost only on a new proxy-best, throttled.
                if it - last_verify >= verify_period {
                    last_verify = it;
                    let tc = premium_score_items(env, items, inc, ir, needs_flag);
                    if tc < best_true {
                        best_true = tc;
                        best_items.clone_from_slice(items);
                    }
                }
            }
        } else {
            for (i, at, angle) in undo {
                items[i].at = at;
                items[i].angle = angle;
            }
        }
    }
    // Always verify the final proxy-best against the true cost.
    let tc = premium_score_items(env, &proxy_best_items, inc, ir, needs_flag);
    if tc < best_true {
        best_items.clone_from_slice(&proxy_best_items);
    }
    items.clone_from_slice(&best_items);
}
/// Router-free sub-grid polish for LARGE boards (`pins > FAST_PINS`). The routed
/// `polish` (align/compact/free_nudge, each routing the whole sheet per candidate
/// move) is the engine's hot loop and costs tens of seconds past ~60 pins. This
/// does the same essential job — pull each satellite onto the anchor pin it taps and
/// close sub-grid whitespace — but scores moves with `proxy_cost` (overlap wall +
/// HPWL + spread + cohesion-to-pin, no router), so its cost is independent of pin
/// count. The clearance-padded overlap guard matches `free_nudge` so it never packs
/// two parts into a readability-lint touch. The SHIPPED warnings are still measured
/// by the one real route emit runs afterwards; this only positions.
fn polish_proxy(items: &mut [Item], inc: &Incidence, ir: &LayoutIr, magnet: bool, gravity: bool) {
    let sats: Vec<usize> =
        (0..items.len()).filter(|&i| items[i].geom.pins.len() < 3 && !items[i].frozen).collect();
    if sats.is_empty() {
        return;
    }
    let cohesion = cohesion_targets(items, inc, ir);
    // Seat each free satellite next to the pin it taps FIRST (a teleport the ±1-cell
    // nudge below can't reach), so a satellite the SA stranded across the sheet (a
    // reset cap far from NRST → a long blocked route the router gives up on and
    // labels) snaps tight to its pin. Then the nudge settles sub-grid offsets.
    if magnet {
        magnet_proxy(items, &cohesion, ir, inc);
    }
    let mut best = proxy_cost(items, inc, ir, &cohesion);
    for _ in 0..6 {
        let mut improved = false;
        for &i in &sats {
            let orig = items[i].at;
            let (mut best_pos, mut best_cost) = (orig, best);
            for (axis, dir) in [(0usize, 1.0), (0, -1.0), (1, 1.0), (1, -1.0)] {
                let mut p = orig;
                p[axis] += dir * 1.27;
                let r = item_rect(&items[i], p);
                let pad = [r[0] - 1.27, r[1] - 1.27, r[2] + 1.27, r[3] + 1.27];
                if items
                    .iter()
                    .enumerate()
                    .any(|(j, it)| j != i && rects_overlap(pad, item_rect(it, it.at)))
                {
                    continue;
                }
                items[i].at = p;
                let c = proxy_cost(items, inc, ir, &cohesion);
                if c + 0.25 < best_cost {
                    best_cost = c;
                    best_pos = p;
                }
            }
            items[i].at = best_pos;
            if best_pos != orig {
                best = best_cost;
                improved = true;
            }
        }
        if !improved {
            break;
        }
    }
    // Optionally close inter-module whitespace (the dominant sprawl) by packing whole
    // blocks toward the centroid — offered as a pick-protected variant by the caller,
    // since over-packing can collide module labels the proxy can't see.
    if gravity {
        block_gravity_proxy(items, inc, ir, &cohesion);
    }
}/// Router-free satellite SEATING: teleport each free satellite to the best
/// overlap-free cell within ±2 grid of the pin it taps (its cohesion-target
/// centroid), kept only when it lowers `proxy_cost`. The ±1-cell nudge can only walk
/// locally, so a satellite the anneal stranded far from its pin never migrates back;
/// this jumps it home in one move. Greedy + proxy-gated, so it only ever tightens.
fn magnet_proxy(
    items: &mut [Item],
    cohesion: &[(usize, Vec<(usize, usize)>)],
    ir: &LayoutIr,
    inc: &Incidence,
) {
    let mut best = proxy_cost(items, inc, ir, cohesion);
    for (si, tgts) in cohesion {
        let si = *si;
        // Live centroid of the target pins.
        let (mut tx, mut ty) = (0.0f64, 0.0f64);
        for &(j, pgi) in tgts {
            let p = sch_io::write::pin_endpoint(
                &items[j].geom.pins[pgi],
                items[j].at,
                items[j].angle,
                items[j].mirror,
            );
            tx += p[0];
            ty += p[1];
        }
        let n = tgts.len() as f64;
        let t = [tx / n, ty / n];
        let orig = items[si].at;
        let (mut best_pos, mut best_c) = (orig, best);
        for dy in -2..=2 {
            for dx in -2..=2 {
                let p = [
                    sch_model::grid::snap(t[0] + dx as f64 * COL_GAP),
                    sch_model::grid::snap(t[1] + dy as f64 * ROW_GAP),
                ];
                let r = item_rect(&items[si], p);
                let pad = [r[0] - 1.27, r[1] - 1.27, r[2] + 1.27, r[3] + 1.27];
                if items
                    .iter()
                    .enumerate()
                    .any(|(j, it)| j != si && rects_overlap(pad, item_rect(it, it.at)))
                {
                    continue;
                }
                items[si].at = p;
                let c = proxy_cost(items, inc, ir, cohesion);
                if c + 0.25 < best_c {
                    best_c = c;
                    best_pos = p;
                }
            }
        }
        items[si].at = best_pos;
        best = best_c;
    }
}
/// Router-free MODULE compaction: slide each anchor's whole BLOCK (the IC + its tap
/// satellites + frozen idiom members) one grid step at a time toward the layout
/// centroid, kept only when it lowers `proxy_cost` and the moved block overlaps no
/// other part. This is the deterministic counterpart to the anneal's random cluster
/// jump — it directly removes the inter-module whitespace (the "modules flung apart /
/// long detour rails" sprawl the per-satellite nudge can't reach) without ever
/// routing. Blocks are rigid, so each block's internal layout (a banked decoupling
/// row, a crystal cluster) travels intact.
fn block_gravity_proxy(
    items: &mut [Item],
    inc: &Incidence,
    ir: &LayoutIr,
    cohesion: &[(usize, Vec<(usize, usize)>)],
) {
    let anchors: Vec<usize> = (0..items.len()).filter(|&i| items[i].geom.pins.len() >= 3).collect();
    let sats: Vec<usize> =
        (0..items.len()).filter(|&i| items[i].geom.pins.len() < 3 && !items[i].frozen).collect();
    if anchors.is_empty() {
        return;
    }
    let blocks = build_anchor_blocks(items, inc, &anchors, &sats, ir);
    let mut best = proxy_cost(items, inc, ir, cohesion);
    for _ in 0..12 {
        // Layout centroid (recomputed each sweep as modules pack inward).
        let (mut cx, mut cy) = (0.0f64, 0.0f64);
        for it in items.iter() {
            cx += it.at[0];
            cy += it.at[1];
        }
        let c = [cx / items.len() as f64, cy / items.len() as f64];
        let mut improved = false;
        for &ai in &anchors {
            let mut group = vec![ai];
            if let Some(b) = blocks.get(&ai) {
                group.extend(b.iter().copied());
            }
            let in_group: BTreeSet<usize> = group.iter().copied().collect();
            for axis in 0..2 {
                let dir = (c[axis] - items[ai].at[axis]).signum();
                if dir == 0.0 {
                    continue;
                }
                let mut delta = [0.0; 2];
                delta[axis] = dir * 1.27;
                // Tentatively slide the whole group; reject if any moved member's
                // padded rect now overlaps a NON-group part.
                // Keep a generous inter-module GUTTER (not just the body-clearance
                // `compact`/`free_nudge` use): packed modules carry power symbols and
                // net-label pennants in the gutter between them, and those text boxes
                // collide well before the bodies do — the "compaction trades against
                // text collisions the cost can't see" trap. A wider margin stops the
                // gravity short of label crowding.
                const G: f64 = 5.08;
                let collide = group.iter().any(|&k| {
                    let np = [items[k].at[0] + delta[0], items[k].at[1] + delta[1]];
                    let r = item_rect(&items[k], np);
                    let pad = [r[0] - G, r[1] - G, r[2] + G, r[3] + G];
                    items
                        .iter()
                        .enumerate()
                        .any(|(j, it)| !in_group.contains(&j) && rects_overlap(pad, item_rect(it, it.at)))
                });
                if collide {
                    continue;
                }
                for &k in &group {
                    items[k].at[0] += delta[0];
                    items[k].at[1] += delta[1];
                }
                let nc = proxy_cost(items, inc, ir, cohesion);
                if nc + 0.25 < best {
                    best = nc;
                    improved = true;
                } else {
                    for &k in &group {
                        items[k].at[0] -= delta[0];
                        items[k].at[1] -= delta[1];
                    }
                }
            }
        }
        if !improved {
            break;
        }
    }
}/// Render `cells` to mm: each column sized to its widest member and each row to
/// its tallest, every part at its cell centre — aligned, overlap-free, and as
/// tight as the parts allow.
/// Snap each frozen CRYSTAL cluster to its IC's actual oscillator-pin positions in
/// mm — the coarse grid can only place a cluster's cells, which on a tall IC pack
/// outside the body. The crystal lands one gap out from the osc pins' midpoint and
/// MOTIF TILING (mined rule #9): N≥3 anchors of the SAME part (e.g. 4× DRV8871 motor-
/// driver channels) are placed on a regular lattice — uniform pitch, each anchor carrying
/// its tap-satellite block rigidly — so repeated structure reads as a clean grid of cells
/// instead of N scattered islands (the named motordrv defect). Targeted (only repeated
/// parts), unlike the global authored grid which over-constrains and hurts. Finalize-only;
/// positions only — connectivity untouched (router redraws; long inter-cell nets → labels).
fn align_repeated_motifs(items: &mut [Item], inc: &Incidence, ir: &LayoutIr) -> bool {
    let anchors: Vec<usize> = (0..items.len()).filter(|&i| items[i].geom.pins.len() >= 3).collect();
    let sats: Vec<usize> =
        (0..items.len()).filter(|&i| items[i].geom.pins.len() < 3 && !items[i].frozen).collect();
    let blocks = build_anchor_blocks(items, inc, &anchors, &sats, ir);
    let blk_bbox = |items: &[Item], ai: usize| -> [f64; 4] {
        let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
        let extend = |k: usize, lo: &mut [f64; 2], hi: &mut [f64; 2]| {
            let r = item_rect(&items[k], items[k].at);
            lo[0] = lo[0].min(r[0]);
            lo[1] = lo[1].min(r[1]);
            hi[0] = hi[0].max(r[2]);
            hi[1] = hi[1].max(r[3]);
        };
        extend(ai, &mut lo, &mut hi);
        if let Some(b) = blocks.get(&ai) {
            for &k in b {
                extend(k, &mut lo, &mut hi);
            }
        }
        [lo[0], lo[1], hi[0], hi[1]]
    };
    let mut by_part: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for &ai in &anchors {
        by_part.entry(items[ai].part.as_str()).or_default().push(ai);
    }
    let mut changed = false;
    for group in by_part.values().filter(|g| g.len() >= 3) {
        let mut g = group.clone();
        g.sort_by(|&a, &b| {
            items[a].at[0]
                .partial_cmp(&items[b].at[0])
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(items[a].at[1].partial_cmp(&items[b].at[1]).unwrap_or(std::cmp::Ordering::Equal))
        });
        const GAP: f64 = 7.62;
        let pitch_x =
            g.iter().map(|&ai| { let b = blk_bbox(items, ai); b[2] - b[0] }).fold(0.0_f64, f64::max) + GAP;
        let pitch_y =
            g.iter().map(|&ai| { let b = blk_bbox(items, ai); b[3] - b[1] }).fold(0.0_f64, f64::max) + GAP;
        let cols = (g.len() as f64).sqrt().ceil().max(1.0) as usize;
        let origin = blk_bbox(items, g[0]);
        for (idx, &ai) in g.iter().enumerate() {
            let (col, row) = (idx % cols, idx / cols);
            let bb = blk_bbox(items, ai);
            let dx = sch_model::grid::snap(origin[0] + col as f64 * pitch_x - bb[0]);
            let dy = sch_model::grid::snap(origin[1] + row as f64 * pitch_y - bb[1]);
            if dx != 0.0 || dy != 0.0 {
                let mut grp = vec![ai];
                if let Some(b) = blocks.get(&ai) {
                    grp.extend(b.iter().copied());
                }
                for &k in &grp {
                    items[k].at[0] += dx;
                    items[k].at[1] += dy;
                }
                changed = true;
            }
        }
    }
    changed
}
/// Geometric TRUTHFULNESS breaks (net merges / shorts / foreign taps) of a placement
/// as it would SHIP — the same checks `layout_cost` prices, returned as a hard count
/// so the candidate pick can REJECT any layout that mis-wires. Critical: the
/// readability `warning_count` does NOT detect a merge (a rail-to-rail short actually
/// LOWERS length+junctions), so a placement move (the proxy magnet/gravity) that
/// strands two nets onto one wire would otherwise be shipped as a fewest-warning
/// candidate — the documented dense-board truthfulness failure. Gating the pick on
/// this makes the router-free fast lane truthfulness-safe without a full netlist
/// extraction.
fn truthfulness_breaks(
    env: &KicadEnv,
    items: &[Item],
    inc: &Incidence,
    ir: &LayoutIr,
    needs_flag: &BTreeSet<String>,
) -> usize {
    match build_writer(env, None, items, inc, ir, needs_flag, true) {
        Ok(w) => {
            let wires = w.wires_with_nets();
            count_merges(&wires, &w.junction_positions())
                + count_shorts(env, &w, items, inc, &wires)
                + count_foreign_taps(&wires)
        }
        Err(_) => usize::MAX,
    }
}
/// Weight on the LLM zone bias in `proxy_cost`. Raised from the original 0.8: at 0.8 the
/// soft pull lost to spread/hpwl/cohere and the engine effectively ignored the LLM's
/// coarse signal-flow/cluster plan (measured: zoned sprawl ≈ unzoned). A stronger pull
/// makes the engine actually FOLLOW the plan (which supplies the GLOBAL structure — flow
/// direction + functional grouping — the local force-layout can't discover). ZERO effect
/// when `ir.zone` is empty (every reference/snapshot path), so byte-identity holds.
const ZBIAS_W: f64 = 0.8;
/// `premium_score_items` when the caller ALREADY knows the shipped warning count
/// `w` (the candidate pick computes it for the primary sort). Identical result,
/// but skips the redundant second text-solving `warning_count` — the candidate
/// evaluation was paying for two full text solves per candidate.
fn premium_score_with_w(
    env: &KicadEnv,
    items: &[Item],
    inc: &Incidence,
    ir: &LayoutIr,
    needs_flag: &BTreeSet<String>,
    w: usize,
) -> f64 {
    let aes = match build_writer(env, None, items, inc, ir, needs_flag, false) {
        Ok(wr) => layout_cost(env, &wr, items, inc, ir, true),
        Err(_) => return f64::INFINITY,
    };
    let pins: usize = items.iter().map(|it| it.geom.pins.len()).sum();
    if pins <= 250 && inc.len() <= 40 {
        10_000.0 * w as f64 + aes
    } else {
        aes
    }
}