//! `anneal-place` — the amplified simulated-annealing schematic placement engine. It OWNS
//! its objective (the amplified energy: the 18 terms under the straightness-amplified
//! weights + the compaction/orientation boosts + the real-warning gate) and its
//! search (the SA move-set + proxy costs + multi-start + route-aware refinement). It is a
//! MEASUREMENT-based engine: it builds + routes candidates to score them, so it searches
//! over [`SchematicPlaceProblem`] plus caller-supplied layout intent and the shared
//! geometry/idiom primitives from `sch-floorplan`'s contract.
//!
//! The amplified objective + the SA + the greedy-descent SEED candidate all live here. The
//! The author's [`sch_place::ir::Relation`] intent is HARD here: the cell seed is projected
//! onto the constraints before the search, every SA move that would break more of them is
//! rejected outright, a heavy `RELATION_W` term keeps the routed objective honest, and the
//! finalize re-projects after the relation-blind align passes. `Group` cohesion is the one
//! SOFT part — a bbox pull, not a constraint.
//!
//! The greedy refine/polish below is a COPY of the free engine's descent (the SA uses a
//! greedy hill-climb as one multi-start candidate) — duplicated, not shared, so the two
//! engines evolve independently. The weights/constants likewise are anneal's own copies.

use std::collections::{BTreeMap, BTreeSet};

use circuit_graph::netclass::is_power_net;
use geom::{EPS, Point2, Rect};
use kicad::KicadInstallation;
use sch_check::model::Design;
use sch_place::ir::{LayoutIr, Orient};
use sch_place::item::{Incidence, Item};
use sch_place::place::{Crossings, PlaceResult};

use sch_floorplan::contract::{
    PlacementEngine, PlacementOutput, RawMetrics, RouteRealization, RoutedEvaluator,
    RoutedSheetRealizer, SchematicPlaceProblem,
};
use sch_floorplan::engine_support::{
    COL_GAP, FAST_PINS, GRID_KEY, ROW_GAP, align_idiom_clusters, align_led_chains,
    align_rail_cap_rows, apply_cells, assign_cells, body_overlap_count, build_anchor_blocks,
    cluster_group, cohesion_targets, decongest, grid_order_viol, item_rect, multi_unit_siblings,
    normalize, orient_angle, overlaps_any, pin_endpoint, relation_group_spread, relation_viol,
    repair_relations, signal_anchor_centroid, supply_pin_target,
};

/// Simulated annealing: a seeded refine→anneal AND a broad anneal from the
/// raw seed, keeping whichever the objective prefers (today's multi-start best-of).
pub struct Anneal;

impl PlacementEngine for Anneal {
    fn name(&self) -> &'static str {
        "anneal"
    }

    fn place(
        &self,
        env: &KicadInstallation,
        design: &Design,
        problem: &mut SchematicPlaceProblem,
        ir: Option<LayoutIr>,
    ) -> PlacementOutput {
        let ir = ir.unwrap_or_else(|| {
            sch_floorplan::floorplan::infer_ir_with_options(env, design, problem.options)
        });
        for it in &mut problem.items {
            it.mirror = ir.mirror.contains(&it.refdes);
        }
        let cells = assign_cells(&problem.items, &ir);
        // Every item is seeded from its cell; only a PRESEEDED item (the region adapter's
        // fixed neighbours) keeps the live pose it arrived with. `frozen` then just forbids
        // the search from moving it.
        apply_cells(&mut problem.items, &cells);
        for it in &mut problem.items {
            it.frozen |= ir.frozen.contains(&it.refdes);
        }
        // Project the cell seed onto the author's relations BEFORE the search, so the SA
        // starts inside the constraint set and its hard rejection rule can keep it there.
        if repair_relations(&mut problem.items, &ir) {
            decongest(&mut problem.items);
        }
        normalize(&mut problem.items);

        let _ = anneal_place(env, problem, &ir, self.name());

        decongest(&mut problem.items);
        // The finalize align passes below are relation-blind, as is `decongest`; re-project
        // and re-relax so what SHIPS satisfies the relations the search held.
        if repair_relations(&mut problem.items, &ir) {
            decongest(&mut problem.items);
        }
        if align_idiom_clusters(&mut problem.items, &ir) {
            decongest(&mut problem.items);
        }
        if align_led_chains(&mut problem.items, &problem.inc, &ir) {
            decongest(&mut problem.items);
        }
        if align_rail_cap_rows(&mut problem.items, &ir) {
            decongest(&mut problem.items);
        }
        if repair_relations(&mut problem.items, &ir) {
            decongest(&mut problem.items);
        }

        let realizer = RoutedSheetRealizer::new(env, &problem.inc, &ir, problem.options);
        let eval = RoutedEvaluator::new(&realizer);
        PlacementOutput {
            result: report(self.name(), problem, &eval),
            ir,
        }
    }
}

// ---------------------------------------------------------------------------
// The amplified OBJECTIVE — anneal's own energy: the 18 raw terms (from the shared
// measurement library) under the amplified weights. The base-tier energy (`base_cost`)
// is the SA's free-base path; it coincides with the free engine's objective today but
// is duplicated, not shared.
// ---------------------------------------------------------------------------

/// Cohesion pull on a multi-unit part's units (same refdes, no shared net). Anneal's own
/// copy of the constant — the free engine carries its own; they are not shared.
const SIB_COHESION: f64 = 3.0;
const GRID_STEP: f64 = geom::GRID_50_MIL.pitch();
/// Extra AMPLIFIED weight on a 1-rail leg orientation violation, on top of the base 12.
const ORIENT_BOOST: f64 = 50.0;
/// Extra AMPLIFIED weight on compactness (length+spread) so straightness can't win by
/// spreading parts into open space.
const COMPACT_BOOST: f64 = 2.0;
/// Weight on the whole-board bbox half-perimeter in `proxy_cost` (the dense fast-lane SA
/// inner loop).
const PROXY_SPREAD_W: f64 = 0.45;
/// Weight on the LLM zone bias in `proxy_cost`.
const ZBIAS_W: f64 = 0.8;
/// Weight on an unsatisfied [`sch_place::ir::Relation`]. Above the authored-grid weight:
/// a relation is an EXPLICIT statement, the grid an authored convenience.
const RELATION_W: f64 = 1500.0;
/// Pull on a `Relation::Group`'s bounding box — the same weight the whole-board `spread`
/// carries, since it is the same quantity restricted to the group.
const GROUP_COHESION: f64 = 0.45;

/// The base routed energy of the 18 raw terms — anneal's own copy of the
/// non-amplified weighted combo. Used by the SA's `amplified=false` path. Build failure
/// (saturated length) ⇒ ∞.
fn base_cost(m: &RawMetrics) -> f64 {
    if !m.length.is_finite() {
        return f64::INFINITY;
    }
    let correctness = 2000.0 * m.merges as f64
        + RELATION_W * m.relation as f64
        + 1500.0 * m.overlaps as f64
        + 1000.0 * m.fallbacks as f64
        + 1200.0 * m.grid_order as f64
        + 30.0 * m.body_cross as f64
        + 12.0 * m.orient_viol as f64
        + 10.0 * m.spine_viol as f64;
    let neat = 1.0;
    let base = correctness
        + neat * (5.0 * m.crossings as f64 + 7.0 * m.congestion as f64 + 7.0 * m.corners as f64)
        + 1.0 * m.junctions as f64
        + 0.7 * m.stray
        + 0.15 * m.length
        + 0.45 * m.spread;
    let multiunit = SIB_COHESION * m.sib_spread;
    base + multiunit + GROUP_COHESION * m.group_spread
}

/// The AMPLIFIED straightness energy of the 18 raw terms. The `neat` multiplier is 3.0
/// (straighter wires), and a matching compaction boost + an orientation boost are ADDED
/// outside the base sum. Build failure ⇒ ∞.
fn amplified_energy(m: &RawMetrics) -> f64 {
    if !m.length.is_finite() {
        return f64::INFINITY;
    }
    let correctness = 2000.0 * m.merges as f64
        + RELATION_W * m.relation as f64
        + 1500.0 * m.overlaps as f64
        + 1000.0 * m.fallbacks as f64
        + 1200.0 * m.grid_order as f64
        + 30.0 * m.body_cross as f64
        + 12.0 * m.orient_viol as f64
        + 10.0 * m.spine_viol as f64;
    let neat = 3.0;
    let base = correctness
        + neat * (5.0 * m.crossings as f64 + 7.0 * m.congestion as f64 + 7.0 * m.corners as f64)
        + 1.0 * m.junctions as f64
        + 0.7 * m.stray
        + 0.15 * m.length
        + 0.45 * m.spread;
    let multiunit = SIB_COHESION * m.sib_spread;
    base + multiunit
        + GROUP_COHESION * m.group_spread
        + COMPACT_BOOST * (0.15 * m.length + 0.45 * m.spread)
        + ORIENT_BOOST * m.leg_viol as f64
}

/// The full amplified objective: the straightness energy [`amplified_energy`] plus — on boards
/// small enough to afford the accurate per-move text solve (`pins <= 250 && nets <= 40`)
/// — a heavy weight on the REAL post-solve warning count, so the SA directly minimises
/// shipped warnings. Anneal's own copy of `amplified_score_items`.
fn amplified_score(problem: &SchematicPlaceProblem, eval: &RoutedEvaluator, items: &[Item]) -> f64 {
    let aes = amplified_energy(&eval.measure(items));
    if !aes.is_finite() {
        return f64::INFINITY;
    }
    let pins: usize = items.iter().map(|it| it.geom.pins.len()).sum();
    if pins <= 250 && problem.inc.len() <= 40 {
        10_000.0 * eval.warnings(items) as f64 + aes
    } else {
        aes
    }
}

/// [`amplified_score`] when the caller ALREADY knows the shipped warning count `w` (the
/// candidate pick computes it for the primary sort). Identical result, but skips the
/// redundant second text-solving warning count. Anneal's own copy of
/// `amplified_score_with_w`.
fn amplified_score_with_w(
    problem: &SchematicPlaceProblem,
    eval: &RoutedEvaluator,
    items: &[Item],
    w: usize,
) -> f64 {
    let aes = amplified_energy(&eval.measure(items));
    if !aes.is_finite() {
        return f64::INFINITY;
    }
    let pins: usize = items.iter().map(|it| it.geom.pins.len()).sum();
    if pins <= 250 && problem.inc.len() <= 40 {
        10_000.0 * w as f64 + aes
    } else {
        aes
    }
}

// ---------------------------------------------------------------------------
// The descent SEED candidate — a local routed refine/polish hill-climb used as one
// multi-start candidate of the SA (`small_path_search`'s "greedy" path + the
// per-candidate `polish`). It lives inside anneal now; there is no separate greedy engine.
// ---------------------------------------------------------------------------

/// HARD relational feasibility: a move that would break MORE of the author's
/// [`sch_place::ir::Relation`] statements than the incumbent is rejected outright, so a
/// search seeded inside the constraint set never leaves it. The heavy `RELATION_W` cost
/// term still applies, and repairs any infeasibility the seed could not project away.
fn relation_regressed(items: &[Item], ir: &LayoutIr, incumbent: usize) -> bool {
    !ir.relations.is_empty() && relation_viol(items, ir) > incumbent
}

/// Score `items` under the base energy by measuring the routed sheet (the greedy
/// descent's objective).
fn greedy_score(eval: &RoutedEvaluator, items: &[Item]) -> f64 {
    base_cost(&eval.measure(items))
}

/// Greedy hill-climb over the satellites' mm positions/orientation (the SA's seeded
/// descent candidate). Local moves kept only on strict improvement of the base routed
/// cost. Anchors hold.
fn refine_items(problem: &SchematicPlaceProblem, eval: &RoutedEvaluator, items: &mut [Item]) {
    let satellites: Vec<usize> = (0..items.len())
        .filter(|&i| items[i].geom.pins.len() < 3 && !items[i].frozen)
        .collect();
    if satellites.is_empty() {
        return;
    }
    let mut best = greedy_score(eval, items);
    const MAX_ROUNDS: usize = 6;
    for _ in 0..MAX_ROUNDS {
        let mut improved = false;
        for &i in &satellites {
            for d in [
                [COL_GAP, 0.0],
                [-COL_GAP, 0.0],
                [0.0, ROW_GAP],
                [0.0, -ROW_GAP],
            ] {
                let prev = items[i].at;
                items[i].at = [
                    geom::GRID_50_MIL.snap(prev[0] + d[0]),
                    geom::GRID_50_MIL.snap(prev[1] + d[1]),
                ]
                .into();
                let c = greedy_score(eval, items);
                if c + 0.5 < best {
                    best = c;
                    improved = true;
                } else {
                    items[i].at = prev;
                }
            }
        }
        for a in 0..satellites.len() {
            for b in (a + 1)..satellites.len() {
                let (i, j) = (satellites[a], satellites[b]);
                let (pi, pj) = (items[i].at, items[j].at);
                items[i].at = pj;
                items[j].at = pi;
                let c = greedy_score(eval, items);
                if c + 0.5 < best {
                    best = c;
                    improved = true;
                } else {
                    items[i].at = pi;
                    items[j].at = pj;
                }
            }
        }
        for &i in &satellites {
            let mut best_a = items[i].angle;
            for o in [Orient::Up, Orient::Down, Orient::Left, Orient::Right] {
                let a = orient_angle(&items[i].geom, o);
                if (a - best_a).abs() < EPS {
                    continue;
                }
                items[i].angle = a;
                let c = greedy_score(eval, items);
                if c + 0.5 < best {
                    best = c;
                    best_a = a;
                    improved = true;
                }
            }
            items[i].angle = best_a;
        }
        for &i in &satellites {
            if let Some(ax) = anchor_x(items, &problem.inc, i) {
                let nx = geom::GRID_50_MIL.snap(2.0 * ax - items[i].at[0]);
                if (nx - items[i].at[0]).abs() > EPS {
                    let prev = items[i].at;
                    items[i].at = [nx, prev[1]].into();
                    let c = greedy_score(eval, items);
                    if c + 0.5 < best {
                        best = c;
                        improved = true;
                    } else {
                        items[i].at = prev;
                    }
                }
            }
        }
        if !improved {
            break;
        }
    }
}

/// The mm x of the single IC anchor a satellite serves (None if it taps zero or several
/// distinct anchor x's), for the side-flip move.
fn anchor_x(items: &[Item], inc: &Incidence, i: usize) -> Option<f64> {
    let mut xs: BTreeSet<i64> = BTreeSet::new();
    let mut x = 0.0;
    for (_, _, net) in &items[i].pins {
        let Some(net) = net else { continue };
        for (j, _) in inc.get(net).into_iter().flatten() {
            if items[*j].geom.pins.len() >= 3 {
                xs.insert((items[*j].at[0] / GRID_KEY).round() as i64);
                x = items[*j].at[0];
            }
        }
    }
    (xs.len() == 1).then_some(x)
}

/// Sub-grid compaction: slide each satellite one grid step toward the centroid where it
/// does not raise the base routed cost and creates no overlap.
fn compact(eval: &RoutedEvaluator, items: &mut [Item]) {
    let sats: Vec<usize> = (0..items.len())
        .filter(|&i| items[i].geom.pins.len() < 3 && !items[i].frozen)
        .collect();
    if sats.is_empty() {
        return;
    }
    let mut best = greedy_score(eval, items);
    for _ in 0..8 {
        let (mut cx, mut cy) = (0.0, 0.0);
        for it in items.iter() {
            cx += it.at[0];
            cy += it.at[1];
        }
        let c = [cx / items.len() as f64, cy / items.len() as f64];
        let mut improved = false;
        for &i in &sats {
            for axis in 0..2 {
                let dir = (c[axis] - items[i].at[axis]).signum();
                if dir == 0.0 {
                    continue;
                }
                let orig = items[i].at;
                let mut p = orig;
                p[axis] += dir * GRID_STEP;
                let a = item_rect(&items[i], p).inflate(GRID_STEP);
                if items
                    .iter()
                    .enumerate()
                    .any(|(j, it)| j != i && a.overlaps(&item_rect(it, it.at)))
                {
                    continue;
                }
                items[i].at = p;
                let sc = greedy_score(eval, items);
                if sc + 0.25 < best {
                    best = sc;
                    improved = true;
                } else {
                    items[i].at = orig;
                }
            }
        }
        if !improved {
            break;
        }
    }
}

/// Continuous-placement polish: iterate {align → compact → free-nudge} to a fixpoint,
/// gated by the base routed cost (the SA ships each candidate through this).
fn polish(
    problem: &SchematicPlaceProblem,
    realizer: &RoutedSheetRealizer,
    eval: &RoutedEvaluator,
    items: &mut [Item],
    ir: &LayoutIr,
) {
    let mut prev = greedy_score(eval, items);
    for _ in 0..3 {
        align_to_pins(problem, realizer, eval, items, ir);
        compact(eval, items);
        free_nudge(eval, items);
        let now = greedy_score(eval, items);
        if prev - now < 1.0 {
            break;
        }
        prev = now;
    }
}

/// Free per-axis nudge: slide each satellite ±1 grid in x and y, keeping any move that
/// lowers the base routed cost without creating a clearance-padded overlap.
fn free_nudge(eval: &RoutedEvaluator, items: &mut [Item]) {
    let sats: Vec<usize> = (0..items.len())
        .filter(|&i| items[i].geom.pins.len() < 3 && !items[i].frozen)
        .collect();
    if sats.is_empty() {
        return;
    }
    let mut best = greedy_score(eval, items);
    for _ in 0..2 {
        let mut improved = false;
        for &i in &sats {
            let orig = items[i].at;
            let (mut best_pos, mut best_cost) = (orig, best);
            for (axis, dir) in [(0usize, 1.0), (0, -1.0), (1, 1.0), (1, -1.0)] {
                let mut p = orig;
                p[axis] += dir * GRID_STEP;
                let pad = item_rect(&items[i], p).inflate(GRID_STEP);
                if items
                    .iter()
                    .enumerate()
                    .any(|(j, it)| j != i && pad.overlaps(&item_rect(it, it.at)))
                {
                    continue;
                }
                items[i].at = p;
                let c = greedy_score(eval, items);
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
}

/// Pin-alignment polish: slide each satellite onto the AXIS of the signal pin it wires
/// to, kept only when it lowers the base routed cost and overlaps nothing.
fn align_to_pins(
    problem: &SchematicPlaceProblem,
    realizer: &RoutedSheetRealizer,
    eval: &RoutedEvaluator,
    items: &mut [Item],
    ir: &LayoutIr,
) {
    let env = realizer.env();
    let inc = &problem.inc;
    let Ok(w0) = realizer.realize_writer(None, items, RouteRealization::CandidateScore) else {
        return;
    };
    let mut plans: Vec<(usize, bool, [f64; 2])> = Vec::new();
    for (si, s) in items.iter().enumerate() {
        if s.geom.pins.len() != 2 {
            continue;
        }
        let pos = |n: &str| {
            w0.pin_dirs(env, &s.refdes, n)
                .ok()
                .and_then(|v| v.first().map(|x| x.0))
        };
        let (Some(p0), Some(p1)) = (pos(&s.geom.pins[0].number), pos(&s.geom.pins[1].number))
        else {
            continue;
        };
        let vertical = (p0[1] - p1[1]).abs() >= (p0[0] - p1[0]).abs();
        if let Some(t) = signal_anchor_centroid(env, &w0, items, inc, ir, s, false) {
            plans.push((si, vertical, t));
        } else if let Some(t) = supply_pin_target(env, &w0, items, inc, ir, s) {
            plans.push((si, vertical, t));
        }
    }
    drop(w0);

    let mut best = greedy_score(eval, items);
    for (si, vertical, target) in plans {
        let axis = if vertical { 0 } else { 1 };
        let orig = items[si].at;
        let goal = geom::GRID_50_MIL.snap(target[axis]);
        let dir = (goal - orig[axis]).signum();
        if dir == 0.0 {
            continue;
        }
        let (mut best_pos, mut best_cost) = (orig, best);
        let mut p = orig;
        for _ in 0..24 {
            p[axis] += dir * GRID_STEP;
            if (p[axis] - goal) * dir > EPS || overlaps_any(items, si, p) {
                break;
            }
            items[si].at = p;
            let c = greedy_score(eval, items);
            if c + 0.5 < best_cost {
                best_cost = c;
                best_pos = p;
            }
        }
        items[si].at = best_pos;
        best = best_cost;
    }
}

// ===========================================================================
// The SA SEARCH — moved verbatim from the former sch-floorplan search module, with the
// injected-cost calls rebound to anneal's own objective + the shared measurement library.
// ===========================================================================

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
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
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

/// The amplified SA search: a seeded refine→anneal AND a broad anneal from
/// the raw seed, keeping whichever the cost prefers (today's multi-start best-of).
/// Writes the final placement into `items` and returns its diagnostics under `engine`
/// (the calling [`PlacementEngine`]'s name). Behaviour is byte-identical to the old
/// inline `Anneal::place` body — this is code-MOTION, not a re-tune.
pub fn anneal_place(
    env: &KicadInstallation,
    problem: &mut SchematicPlaceProblem,
    ir: &LayoutIr,
    engine: &'static str,
) -> PlaceResult {
    let inc = &problem.inc;
    let realizer =
        RoutedSheetRealizer::new(env, inc, ir, sch_place::place::PlaceOptions::default());
    let eval = RoutedEvaluator::new(&realizer);
    let seed = problem.seed;
    use rayon::prelude::*;
    let timed_top = problem.options.debug_timing;

    // A group with no placed problem.items (e.g. a sub-sheet holding only power/label
    // declarations, which `gather` skips) has nothing to search — and the fast
    // lane's `30_000 / pins` would divide by zero. Bail out cleanly.
    if problem.items.is_empty() {
        return report(engine, problem, &eval);
    }

    // FAST LANE (large boards): the tuned routed paths below route the whole sheet
    // per move and cost minutes past ~60 pins. Here the search is router-free —
    // multi-start `anneal_locality` (proxy cost + range-limited cluster jump) from
    // the raw cell seed — and the only routes paid are the bounded candidate
    // selection + the one final emit. Strictly additive safety is preserved: the
    // RAW seed is always a candidate (a floor), and the pick takes fewest real
    // warnings then true cost, so the fast lane never ships worse than the seed.
    let pins: usize = problem.items.iter().map(|it| it.geom.pins.len()).sum();
    // PORT-HEAVY sheet = a multi-sheet sub-sheet: its inter-block nets each touch only one
    // pin here, so they become single-pin signal PORTS (labels). Such a sheet is small but
    // its bus/port fanout tangles, and the small path leaves the crossings uncorrected (a
    // 6-part I2C sheet sat at 5 crossings though the topology allows ~1). Route it through the
    // fast lane so it gets the route-aware crossing REFINEMENT (validated: io 7→8,
    // power_entry 8→9). Self-contained reference boards have <6 single-pin signal nets, so
    // they stay on the small path ⇒ snapshots byte-identical.
    let signal_ports = {
        let mut npins: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        for it in problem.items.iter() {
            for (_, _, net) in &it.pins {
                if let Some(net) = net {
                    *npins.entry(net.clone()).or_insert(0) += 1;
                }
            }
        }
        npins
            .iter()
            .filter(|(net, count)| {
                **count == 1 && !ir.rails.contains_key(net.as_str()) && !is_power_net(net)
            })
            .count()
    };
    let port_heavy = signal_ports >= 6;
    let force_fast = port_heavy || problem.options.force_fast;

    // A tiny pair of two-pin passives with at most two shared nets has no third
    // body and only a single direct path per net, so it has no routing topology for
    // multi-start annealing to improve. Avoid hundreds of full route/text-solve
    // evaluations in both the apply preview and commit passes; explicit refinement
    // and port-heavy sheets always retain the full search.
    let shared_nets = problem.inc.values().filter(|pins| pins.len() > 1).count();
    let trivial_chain = problem.items.len() <= 2
        && pins <= 4
        && problem.items.iter().all(|item| item.geom.pins.len() <= 2)
        && shared_nets <= 2
        && signal_ports <= 2
        && !force_fast;
    if trivial_chain {
        let seed_items = problem.items.clone();
        decongest(&mut problem.items);
        let quick = report(engine, problem, &eval);
        if quick.truthfulness_breaks == 0 && quick.warnings == 0 && quick.crossings.total() == 0 {
            return quick;
        }
        problem.items = seed_items;
    }

    if pins > FAST_PINS || force_fast {
        let raw: Vec<Item> = problem.items.to_vec();
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
                anneal_locality(problem, &eval, &mut st, inc, ir, s);
                st
            })
            .collect();
        if timed_top {
            tracing::debug!(
                "  [SA-fast] {n_starts} proxy starts: {:.2}s",
                t_search.elapsed().as_secs_f64()
            );
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
                let b = eval.truthfulness_breaks(cand);
                let w = eval.warnings(cand);
                let c = amplified_score_with_w(problem, &eval, cand, w);
                (b, w, c)
            })
            .collect();
        if timed_top {
            tracing::debug!(
                "  [SA-fast] score {} candidates: {:.2}s",
                candidates.len(),
                t_score.elapsed().as_secs_f64()
            );
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
            tracing::debug!("  [SA-fast] pick cand#{best} scored={scored:?}");
        }
        // ROUTE-AWARE REFINEMENT (large boards). The proxy is crossing-BLIND, so the
        // fast-lane winner is sprawl-optimal but not crossing-optimal. Refine it with a
        // bounded `anneal_items` whose objective is the TRUE routed cost (amplified) — the
        // only faithful crossing signal — which no cheap proxy could capture. Seeded
        // from the already-good winner, so its 80 routed iterations are spent polishing,
        // not exploring. Kept ONLY if it wins the
        // SAME (breaks, warnings, true-cost) pick, so it can never ship worse. This
        // trades a bounded routed-search budget for fewer dense-board crossings.
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
            let b = eval.truthfulness_breaks(&m);
            let w = eval.warnings(&m);
            let cr = eval.crossings(&m);
            (
                b,
                w,
                cr.total(),
                amplified_score_with_w(problem, &eval, &m, w),
            )
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
        // amplified score improves). Cheap on a small sheet. A big board still skips when clean.
        let small_forced = force_fast && pins <= FAST_PINS;
        if !small_forced && bb == 0 && bw == 0 && bx <= 6 {
            problem.items.clone_from_slice(&candidates[best]);
            return report(engine, problem, &eval);
        }
        // ROUTE-AWARE REFINEMENT. The proxy is crossing-BLIND, so the fast-lane winner is
        // sprawl-optimal but not crossing-optimal — and no cheap router-free crossing
        // proxy proved faithful (bbox/trunk-segment all failed). So refine the winner with
        // the TRUE router: a bounded `anneal_items` (amplified routed cost; iter-capped
        // at 80 routed iterations) seeded from it. Kept
        // ONLY if it wins on real (finalised) crossings, so it is strictly additive — a
        // straighter-but-more-crossing result is rejected. Trades bounded time for fewer
        // dense-board crossings.
        let mut refined = candidates[best].clone();
        let t_ref = std::time::Instant::now();
        // Moderate fast-lane sheets are the most expensive routed-objective case:
        // 30k/pins used to clamp them to the maximum 300 iterations (a 43-pin
        // production sensor sheet spent 81 s here and timed out during compose).
        // The selected proxy candidate remains the strict quality floor, so a
        // shorter refinement can only win when its fully routed result improves.
        // Keep one bounded budget across the fast lane; reference fixtures never
        // enter this path because they are at or below FAST_PINS.
        let ref_cap = 80;
        anneal_items(
            problem,
            &eval,
            &mut refined,
            inc,
            ir,
            false,
            true,
            seed ^ 0x5EF1,
            Some(ref_cap),
        );
        decongest(&mut refined);
        let (rb, rw, rx, rc) = score(&refined);
        let refined_wins = (rb, rw, rx).cmp(&(bb, bw, bx)) == std::cmp::Ordering::Less
            || (rb == bb && rw == bw && rx == bx && rc + 0.5 < bc);
        if timed_top {
            tracing::debug!(
                "  [SA-fast] route-refine {:.2}s cap={ref_cap}: ({bb},{bw},{bx},{bc:.0})->({rb},{rw},{rx},{rc:.0}) win={refined_wins}",
                t_ref.elapsed().as_secs_f64()
            );
        }
        let mut fast_final: Vec<Item> = if refined_wins {
            refined
        } else {
            candidates[best].clone()
        };
        // MOTIF TILING (opt-in via `MOTIF_TILE`, dense-only): tile repeated same-part
        // anchor blocks (4× DRV8871 etc.) on a regular lattice — the human idiom for
        // repeated structure (mined rule #9). Strictly ADDITIVE: applied only when it
        // neither breaks connectivity NOR adds a readability warning, so when enabled it
        // can only tidy, never regress the measurable gates. Default OFF ⇒ byte-identical.
        // (Validated NEUTRAL-or-better on motordrv: critic 6=6, convention dim +1, channels
        // visibly tiled; kept opt-in pending multi-board validation since layout-forcing can
        // hurt the critic in ways warnings don't catch — see the grid experiment.)
        if problem.options.motif_tile {
            let mut cand = fast_final.clone();
            if align_repeated_motifs(&mut cand, inc, ir) {
                decongest(&mut cand);
                let before = (
                    eval.truthfulness_breaks(&fast_final),
                    eval.warnings(&fast_final),
                );
                let after = (eval.truthfulness_breaks(&cand), eval.warnings(&cand));
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
            let sp = small_path_search(
                problem, &realizer, &eval, &bases[0], inc, ir, seed, timed_top,
            );
            let (fb, fw, fx, fc) = score(&fast_final);
            let (sb, sw, sx, sc) = score(&sp);
            let sp_wins = (sb, sw, sx).cmp(&(fb, fw, fx)) == std::cmp::Ordering::Less
                || (sb == fb && sw == fw && sx == fx && sc + 0.5 < fc);
            problem
                .items
                .clone_from_slice(if sp_wins { &sp } else { &fast_final });
        } else {
            problem.items.clone_from_slice(&fast_final);
        }
        return report(engine, problem, &eval);
    }

    // Small board: greedy + four parallel anneals, pick the polished winner.
    // Extracted to small_path_search so the force_fast fast lane can run it as a
    // rival candidate; this call reproduces the old inline behaviour exactly.
    let placed = small_path_search(
        problem,
        &realizer,
        &eval,
        problem.items.as_slice(),
        inc,
        ir,
        seed,
        timed_top,
    );
    problem.items.clone_from_slice(&placed);
    report(engine, problem, &eval)
}

/// Measure the FINAL placement against the injected cost, for the diagnostic
/// [`PlaceResult`]. Empty placements report all-zero.
fn report(engine: &str, problem: &SchematicPlaceProblem, eval: &RoutedEvaluator) -> PlaceResult {
    let items = &problem.items;
    if items.is_empty() {
        return PlaceResult {
            engine: engine.to_string(),
            truthfulness_breaks: 0,
            warnings: 0,
            crossings: Crossings::default(),
            cost: 0.0,
        };
    }
    let warnings = eval.warnings(items);
    PlaceResult {
        engine: engine.to_string(),
        truthfulness_breaks: eval.truthfulness_breaks(items),
        warnings,
        crossings: eval.crossings(items),
        cost: amplified_score_with_w(problem, eval, items, warnings),
    }
}
/// The small-board placement search, extracted so the fast lane can run it as a RIVAL
/// candidate for force_fast SMALL sub-sheets (the fast lane's locality proxy is
/// crossing-worse than this on simple sheets — a split-supply power sheet sat at 4
/// crossings via the fast lane vs 2 here). Greedy refine + four parallel anneals (A
/// seeded, B broad, C amplified, D locality), then pick the polished winner by
/// (truthfulness, warnings, amplified cost). Operates on a COPY of `seed`, returns the
/// POLISHED winner. Behaviour is byte-identical to the old inline else-branch (the
/// placement_snapshot verifies it for the references that take the small path).
#[allow(clippy::too_many_arguments)]
fn small_path_search(
    problem: &SchematicPlaceProblem,
    realizer: &RoutedSheetRealizer,
    eval: &RoutedEvaluator,
    seed: &[Item],
    inc: &Incidence,
    ir: &LayoutIr,
    rng_seed: u64,
    timed: bool,
) -> Vec<Item> {
    use rayon::prelude::*;
    let mut work: Vec<Item> = seed.to_vec();
    let seed_state: Vec<Item> = work.clone();
    let tic = |label: &str, f: &mut dyn FnMut()| {
        let t0 = std::time::Instant::now();
        f();
        if timed {
            tracing::debug!("  [SA] {label}: {:.2}s", t0.elapsed().as_secs_f64());
        }
    };
    let mut state_a: Vec<Item> = Vec::new();
    let mut state_b: Vec<Item> = seed_state;
    let mut state_c: Vec<Item> = Vec::new();
    let mut state_d: Vec<Item> = Vec::new();
    let mut greedy_state: Vec<Item> = Vec::new();
    rayon::scope(|s| {
        s.spawn(|_| {
            let mut f = || {
                anneal_items(
                    problem,
                    eval,
                    &mut state_b,
                    inc,
                    ir,
                    true,
                    false,
                    rng_seed,
                    None,
                )
            };
            tic("B broad", &mut f);
        });
        {
            let mut f = || refine_items(problem, eval, &mut work);
            tic("greedy", &mut f);
        }
        greedy_state = work.to_vec();
        state_a = greedy_state.clone();
        state_c = greedy_state.clone();
        state_d = greedy_state.clone();
        rayon::join(
            || {
                let mut f = || {
                    anneal_items(
                        problem,
                        eval,
                        &mut state_a,
                        inc,
                        ir,
                        false,
                        false,
                        rng_seed,
                        None,
                    )
                };
                tic("A seeded", &mut f);
            },
            || {
                rayon::join(
                    || {
                        let mut f = || {
                            anneal_items(
                                problem,
                                eval,
                                &mut state_c,
                                inc,
                                ir,
                                false,
                                true,
                                rng_seed ^ 0x9E3779B97F4A7C15,
                                None,
                            )
                        };
                        tic("C amplified", &mut f);
                    },
                    || {
                        let mut f = || {
                            anneal_locality(
                                problem,
                                eval,
                                &mut state_d,
                                inc,
                                ir,
                                rng_seed ^ 0x517CC1B727220A95,
                            )
                        };
                        tic("D locality", &mut f);
                    },
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
            polish(problem, realizer, eval, &mut shipped, ir);
            decongest(&mut shipped);
            let b = eval.truthfulness_breaks(&shipped);
            let w = eval.warnings(&shipped);
            let c = amplified_score_with_w(problem, eval, &shipped, w);
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
#[allow(clippy::too_many_arguments)]
fn anneal_items(
    problem: &SchematicPlaceProblem,
    eval: &RoutedEvaluator,
    items: &mut [Item],
    inc: &Incidence,
    ir: &LayoutIr,
    broad: bool,
    amplified: bool,
    seed: u64,
    iter_cap: Option<usize>,
) {
    // The objective: the base run minimises the base routed cost; the amplified run
    // optimises the richer (straighter) objective. Run as an EXTRA candidate so it
    // never displaces the base run's warning-free find — see `Anneal::search`.
    let objective = |items: &[Item]| {
        if amplified {
            amplified_score(problem, eval, items)
        } else {
            base_cost(&eval.measure(items))
        }
    };
    let sats: Vec<usize> = (0..items.len())
        .filter(|&i| items[i].geom.pins.len() < 3 && !items[i].frozen)
        .collect();
    let anchors: Vec<usize> = (0..items.len())
        .filter(|&i| items[i].geom.pins.len() >= 3)
        .collect();
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

    let mut cur = objective(items);
    let mut cur_rv = relation_viol(items, ir);
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
            geom::GRID_50_MIL.snap(at[0] + rng.step(n) as f64 * COL_GAP),
            geom::GRID_50_MIL.snap(at[1] + rng.step(n) as f64 * ROW_GAP),
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
            undo = vec![(i, items[i].at.into(), items[i].angle)];
            items[i].at = relocate(&mut rng, items[i].at.into(), 2).into();
        } else if m < 8 {
            // Re-orient a satellite.
            let i = sats[rng.below(sats.len())];
            undo = vec![(i, items[i].at.into(), items[i].angle)];
            items[i].angle = orient_angle(&items[i].geom, orients[rng.below(4)]);
        } else if m < 9 && sats.len() >= 2 {
            // Swap two satellites' positions (keep each orientation).
            let a = sats[rng.below(sats.len())];
            let b = sats[rng.below(sats.len())];
            undo = vec![
                (a, items[a].at.into(), items[a].angle),
                (b, items[b].at.into(), items[b].angle),
            ];
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
            let new = relocate(&mut rng, items[i].at.into(), 1);
            let d = [new[0] - items[i].at[0], new[1] - items[i].at[1]];
            let group = cluster_group(i, &blocks, &siblings);
            undo = group
                .iter()
                .map(|&k| (k, items[k].at.into(), items[k].angle))
                .collect();
            for &k in &group {
                items[k].at = [
                    geom::GRID_50_MIL.snap(items[k].at[0] + d[0]),
                    geom::GRID_50_MIL.snap(items[k].at[1] + d[1]),
                ]
                .into();
            }
        } else {
            continue;
        }

        if relation_regressed(items, ir, cur_rv) {
            for (i, at, angle) in undo {
                items[i].at = at.into();
                items[i].angle = angle;
            }
            continue;
        }
        let c = objective(items);
        let d = c - cur;
        if d < 0.0 || rng.unit() < (-d / t).exp() {
            cur = c;
            cur_rv = relation_viol(items, ir);
            if c < best {
                best = c;
                best_items.clone_from_slice(items);
            }
        } else {
            for (i, at, angle) in undo {
                items[i].at = at.into();
                items[i].angle = angle;
            }
        }
    }
    items.clone_from_slice(&best_items);
}
/// A cheap, routing-FREE geometric proxy for the routed energy — the per-move objective
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
        let pts: Vec<Point2> = pins.iter().map(|(i, _)| items[*i].at).collect();
        hpwl += Rect::bounding(&pts).map_or(0.0, |r| r.half_perimeter());
    }
    let item_pts: Vec<Point2> = items.iter().map(|it| it.at).collect();
    let item_bbox = Rect::bounding(&item_pts);
    let spread = item_bbox.map_or(0.0, |r| r.half_perimeter());
    let mut cohere = 0.0;
    for (si, tgts) in cohesion {
        let (mut cx, mut cy) = (0.0f64, 0.0f64);
        for (j, pgi) in tgts {
            let p = pin_endpoint(
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
    if let Some(bbox) = item_bbox
        && !ir.zone.is_empty()
        && bbox.width() > 0.0
        && bbox.height() > 0.0
    {
        let (bw, bh) = (bbox.width(), bbox.height());
        for it in items {
            if let Some([tx, ty]) = ir.zone.get(&it.refdes) {
                let fx = (it.at[0] - bbox.min_x) / bw;
                let fy = (it.at[1] - bbox.min_y) / bh;
                zbias += (fx - tx).abs() * bw + (fy - ty).abs() * bh;
            }
        }
    }
    1500.0 * overlaps as f64
        + RELATION_W * relation_viol(items, ir) as f64
        + GROUP_COHESION * relation_group_spread(items, ir)
        + 1200.0 * grid_order as f64
        + 0.15 * hpwl
        + PROXY_SPREAD_W * spread
        + 0.7 * cohere
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
    problem: &SchematicPlaceProblem,
    eval: &RoutedEvaluator,
    items: &mut [Item],
    inc: &Incidence,
    ir: &LayoutIr,
    seed: u64,
) {
    let sats: Vec<usize> = (0..items.len())
        .filter(|&i| items[i].geom.pins.len() < 3 && !items[i].frozen)
        .collect();
    let anchors: Vec<usize> = (0..items.len())
        .filter(|&i| items[i].geom.pins.len() >= 3)
        .collect();
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
            geom::GRID_50_MIL.snap(at[0] + rng.step(n) as f64 * COL_GAP),
            geom::GRID_50_MIL.snap(at[1] + rng.step(n) as f64 * ROW_GAP),
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
    let span_cells = (((bhi[0] - blo[0]).max(bhi[1] - blo[1])) / COL_GAP)
        .ceil()
        .max(2.0) as i32;

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
    let max_verifies = if pins > FAST_PINS {
        (4000 / pins.max(1)).clamp(6, 128)
    } else {
        256
    };
    let verify_period = (iters / max_verifies).max(1);
    let mut last_verify = 0usize;

    let mut cur = proxy_cost(items, inc, ir, &cohesion);
    let mut cur_rv = relation_viol(items, ir);
    let mut proxy_best = cur;
    let mut proxy_best_items: Vec<Item> = items.to_vec();
    let mut best_true = amplified_score(problem, eval, items);
    let mut best_items: Vec<Item> = items.to_vec();

    for it in 0..iters {
        let p = it as f64 / iters as f64;
        let t = (t0 * (1.0 - p)).max(0.05);
        let m = rng.below(10);
        let undo: Vec<(usize, [f64; 2], f64)>;
        if m < 6 {
            let i = sats[rng.below(sats.len())];
            undo = vec![(i, items[i].at.into(), items[i].angle)];
            items[i].at = relocate(&mut rng, items[i].at.into(), 2).into();
        } else if m < 8 {
            let i = sats[rng.below(sats.len())];
            undo = vec![(i, items[i].at.into(), items[i].angle)];
            items[i].angle = orient_angle(&items[i].geom, orients[rng.below(4)]);
        } else if m < 9 && sats.len() >= 2 {
            let a = sats[rng.below(sats.len())];
            let b = sats[rng.below(sats.len())];
            undo = vec![
                (a, items[a].at.into(), items[a].angle),
                (b, items[b].at.into(), items[b].angle),
            ];
            let (pa, pb) = (items[a].at, items[b].at);
            items[a].at = pb;
            items[b].at = pa;
        } else if !anchors.is_empty() {
            // RANGE-LIMITED CLUSTER JUMP: large displacement when hot, decaying to a
            // 1-cell nudge when cold — carries the anchor's whole block rigidly.
            let radius = (((1.0 - p) * span_cells as f64).round() as i32).max(1);
            let i = anchors[rng.below(anchors.len())];
            let new = relocate(&mut rng, items[i].at.into(), radius);
            let d = [new[0] - items[i].at[0], new[1] - items[i].at[1]];
            let group = cluster_group(i, &blocks, &siblings);
            undo = group
                .iter()
                .map(|&k| (k, items[k].at.into(), items[k].angle))
                .collect();
            for &k in &group {
                items[k].at = [
                    geom::GRID_50_MIL.snap(items[k].at[0] + d[0]),
                    geom::GRID_50_MIL.snap(items[k].at[1] + d[1]),
                ]
                .into();
            }
        } else {
            continue;
        }

        if relation_regressed(items, ir, cur_rv) {
            for (i, at, angle) in undo {
                items[i].at = at.into();
                items[i].angle = angle;
            }
            continue;
        }
        let c = proxy_cost(items, inc, ir, &cohesion);
        let d = c - cur;
        if d < 0.0 || rng.unit() < (-d / t).exp() {
            cur = c;
            cur_rv = relation_viol(items, ir);
            if c < proxy_best {
                proxy_best = c;
                proxy_best_items.clone_from_slice(items);
                // Pay the true routed cost only on a new proxy-best, throttled.
                if it - last_verify >= verify_period {
                    last_verify = it;
                    let tc = amplified_score(problem, eval, items);
                    if tc < best_true {
                        best_true = tc;
                        best_items.clone_from_slice(items);
                    }
                }
            }
        } else {
            for (i, at, angle) in undo {
                items[i].at = at.into();
                items[i].angle = angle;
            }
        }
    }
    // Always verify the final proxy-best against the true cost.
    let tc = amplified_score(problem, eval, &proxy_best_items);
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
    let sats: Vec<usize> = (0..items.len())
        .filter(|&i| items[i].geom.pins.len() < 3 && !items[i].frozen)
        .collect();
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
                p[axis] += dir * GRID_STEP;
                let pad = item_rect(&items[i], p).inflate(GRID_STEP);
                if items
                    .iter()
                    .enumerate()
                    .any(|(j, it)| j != i && pad.overlaps(&item_rect(it, it.at)))
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
}
/// Router-free satellite SEATING: teleport each free satellite to the best
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
            let p = pin_endpoint(
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
                    geom::GRID_50_MIL.snap(t[0] + dx as f64 * COL_GAP),
                    geom::GRID_50_MIL.snap(t[1] + dy as f64 * ROW_GAP),
                ];
                let pad = item_rect(&items[si], p).inflate(GRID_STEP);
                if items
                    .iter()
                    .enumerate()
                    .any(|(j, it)| j != si && pad.overlaps(&item_rect(it, it.at)))
                {
                    continue;
                }
                items[si].at = p.into();
                let c = proxy_cost(items, inc, ir, cohesion);
                if c + 0.25 < best_c {
                    best_c = c;
                    best_pos = p.into();
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
    let anchors: Vec<usize> = (0..items.len())
        .filter(|&i| items[i].geom.pins.len() >= 3)
        .collect();
    let sats: Vec<usize> = (0..items.len())
        .filter(|&i| items[i].geom.pins.len() < 3 && !items[i].frozen)
        .collect();
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
                delta[axis] = dir * GRID_STEP;
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
                    let pad = item_rect(&items[k], np).inflate(G);
                    items.iter().enumerate().any(|(j, it)| {
                        !in_group.contains(&j) && pad.overlaps(&item_rect(it, it.at))
                    })
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
}
/// MOTIF TILING (mined rule #9): N≥3 anchors of the SAME part (e.g. 4× DRV8871 motor-
/// driver channels) are placed on a regular lattice — uniform pitch, each anchor carrying
/// its tap-satellite block rigidly — so repeated structure reads as a clean grid of cells
/// instead of N scattered islands (the named motordrv defect). Targeted (only repeated
/// parts), unlike the global authored grid which over-constrains and hurts. Finalize-only;
/// positions only — connectivity untouched (router redraws; long inter-cell nets → labels).
fn align_repeated_motifs(items: &mut [Item], inc: &Incidence, ir: &LayoutIr) -> bool {
    let anchors: Vec<usize> = (0..items.len())
        .filter(|&i| items[i].geom.pins.len() >= 3)
        .collect();
    let sats: Vec<usize> = (0..items.len())
        .filter(|&i| items[i].geom.pins.len() < 3 && !items[i].frozen)
        .collect();
    let blocks = build_anchor_blocks(items, inc, &anchors, &sats, ir);
    let blk_bbox = |items: &[Item], ai: usize| -> Rect {
        let mut corners = Vec::new();
        let mut extend = |k: usize| {
            let r = item_rect(&items[k], items[k].at);
            corners.push(Point2::new(r.min_x, r.min_y));
            corners.push(Point2::new(r.max_x, r.max_y));
        };
        extend(ai);
        if let Some(b) = blocks.get(&ai) {
            for &k in b {
                extend(k);
            }
        }
        Rect::bounding(&corners).expect("anchor bbox has at least one item")
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
                .then(
                    items[a].at[1]
                        .partial_cmp(&items[b].at[1])
                        .unwrap_or(std::cmp::Ordering::Equal),
                )
        });
        const GAP: f64 = 7.62;
        let pitch_x = g
            .iter()
            .map(|&ai| {
                let b = blk_bbox(items, ai);
                b.width()
            })
            .fold(0.0_f64, f64::max)
            + GAP;
        let pitch_y = g
            .iter()
            .map(|&ai| {
                let b = blk_bbox(items, ai);
                b.height()
            })
            .fold(0.0_f64, f64::max)
            + GAP;
        let cols = (g.len() as f64).sqrt().ceil().max(1.0) as usize;
        let origin = blk_bbox(items, g[0]);
        for (idx, &ai) in g.iter().enumerate() {
            let (col, row) = (idx % cols, idx / cols);
            let bb = blk_bbox(items, ai);
            let dx = geom::GRID_50_MIL.snap(origin.min_x + col as f64 * pitch_x - bb.min_x);
            let dy = geom::GRID_50_MIL.snap(origin.min_y + row as f64 * pitch_y - bb.min_y);
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
