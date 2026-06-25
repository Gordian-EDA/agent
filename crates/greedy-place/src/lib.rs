//! `greedy-place` — the base greedy hill-climb placement engine. It OWNS its
//! objective (the base weighted combo of the 16 routed-sheet terms) and its search
//! (the routed `refine` hill-climb + the `polish` align→compact→nudge fixpoint). It is a
//! MEASUREMENT-based engine: it builds + routes each candidate to score it, so it calls
//! `sch-floorplan`'s measurement library ([`Realizer`]/[`RawMetrics`]) and the shared
//! geometry primitives, and implements the `PlacementEngine` trait `sch-floorplan`
//! publishes beside that library.
//!
//! The objective weights here are greedy's own. They COINCIDE today with the shared base
//! of the amplified engine's energy, but the two engines are independently evolvable — the
//! weights are copied, not factored into a shared type.

use geom::EPS;
use sch_place::ir::Orient;
use sch_place::item::{Incidence, Item};
use sch_place::place::{Crossings, PlaceProblem, PlaceResult};

use sch_floorplan::contract::{
    COL_GAP, GRID_KEY, PlacementEngine, ROW_GAP, RawMetrics, Realizer, build_writer, item_rect,
    orient_angle, overlaps_any, rects_overlap, signal_anchor_centroid, supply_pin_target,
};

/// Greedy hill-climb: local, strictly-cost-improving moves only over
/// the seeded mm placement.
pub struct Greedy;

impl PlacementEngine for Greedy {
    fn name(&self) -> &'static str {
        "greedy"
    }

    fn place(&self, r: &Realizer, _p: &PlaceProblem, items: &mut [Item]) -> PlaceResult {
        refine_items(r, items);
        // The base engine uses the ROUTED polish at EVERY size: it is the truthfulness-
        // safe path (each move re-routes, so the cost sees a net merge / short — the
        // router-free proxy polish does NOT, and greedy has no candidate pick to reject
        // a mis-wire). References keep the exact refine→polish order → byte-identical.
        polish(r, items);
        report(self.name(), r, items)
    }
}

/// Greedy's base OBJECTIVE: the 16 raw routed-sheet terms weighted into one scalar
/// the hill-climb minimises. A build failure saturates the terms (length/spread → ∞),
/// so the candidate is rejected.
fn greedy_cost(m: &RawMetrics) -> f64 {
    if !m.length.is_finite() {
        return f64::INFINITY;
    }
    let correctness = 2000.0 * m.merges as f64
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
        + 0.5 * m.stray
        + 0.15 * m.length
        + 0.45 * m.spread;
    let multiunit = SIB_COHESION * m.sib_spread;
    base + multiunit
}

/// Cohesion pull on a multi-unit part's units (same refdes, no shared net). Greedy's own
/// copy of the constant — the amplified engine carries its own; they are not shared.
const SIB_COHESION: f64 = 3.0;

/// Score `items` under greedy's objective by measuring the routed sheet.
fn cost(r: &Realizer, items: &[Item]) -> f64 {
    greedy_cost(&r.measure(items))
}

/// Measure the FINAL placement for the diagnostic [`PlaceResult`]. Empty placements
/// report all-zero.
fn report(engine: &str, r: &Realizer, items: &[Item]) -> PlaceResult {
    if items.is_empty() {
        return PlaceResult {
            engine: engine.to_string(),
            truthfulness_breaks: 0,
            warnings: 0,
            crossings: Crossings::default(),
            cost: 0.0,
        };
    }
    PlaceResult {
        engine: engine.to_string(),
        truthfulness_breaks: r.truthfulness_breaks(items),
        warnings: r.warnings(items),
        crossings: r.crossings(items),
        cost: cost(r, items),
    }
}

/// Greedy hill-climb over the satellites' mm positions/orientation (the seed is
/// the IR grid projected to mm). Local moves — nudge a cell-step, swap a pair,
/// re-orient, side-flip across the served IC — each kept only on strict
/// improvement of the REAL routed cost. Anchors hold. Operating directly on mm means
/// the objective IS the geometry that ships.
fn refine_items(r: &Realizer, items: &mut [Item]) {
    let satellites: Vec<usize> = (0..items.len())
        .filter(|&i| items[i].geom.pins.len() < 3 && !items[i].frozen)
        .collect();
    if satellites.is_empty() {
        return;
    }
    let mut best = cost(r, items);
    const MAX_ROUNDS: usize = 6;
    for _ in 0..MAX_ROUNDS {
        let mut improved = false;
        // Single-part nudges: shift one satellite by one cell-step (grid-snapped).
        for &i in &satellites {
            for d in [
                [COL_GAP, 0.0],
                [-COL_GAP, 0.0],
                [0.0, ROW_GAP],
                [0.0, -ROW_GAP],
            ] {
                let prev = items[i].at;
                items[i].at = [snap(prev[0] + d[0]), snap(prev[1] + d[1])].into();
                let c = cost(r, items);
                if c + 0.5 < best {
                    best = c;
                    improved = true;
                } else {
                    items[i].at = prev;
                }
            }
        }
        // Pairwise swaps: exchange two satellites' positions (keep each orientation).
        for a in 0..satellites.len() {
            for b in (a + 1)..satellites.len() {
                let (i, j) = (satellites[a], satellites[b]);
                let (pi, pj) = (items[i].at, items[j].at);
                items[i].at = pj;
                items[j].at = pi;
                let c = cost(r, items);
                if c + 0.5 < best {
                    best = c;
                    improved = true;
                } else {
                    items[i].at = pi;
                    items[j].at = pj;
                }
            }
        }
        // Rotation: re-orient one satellite (free — not displacement-penalised).
        for &i in &satellites {
            let mut best_a = items[i].angle;
            for o in [Orient::Up, Orient::Down, Orient::Left, Orient::Right] {
                let a = orient_angle(&items[i].geom, o);
                if (a - best_a).abs() < EPS {
                    continue;
                }
                items[i].angle = a;
                let c = cost(r, items);
                if c + 0.5 < best {
                    best = c;
                    best_a = a;
                    improved = true;
                }
            }
            items[i].angle = best_a;
        }
        // Side-flip: mirror a satellite across the single IC anchor it serves (the
        // big relocation a one-cell nudge cannot reach), so a part on the wrong
        // side of its IC migrates over (the wire then drops straight).
        for &i in &satellites {
            if let Some(ax) = anchor_x(items, r.incidence(), i) {
                let nx = snap(2.0 * ax - items[i].at[0]);
                if (nx - items[i].at[0]).abs() > EPS {
                    let prev = items[i].at;
                    items[i].at = [nx, prev[1]].into();
                    let c = cost(r, items);
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

/// The mm x of the single IC anchor a satellite serves (None if it taps zero or
/// several distinct anchor x's), for the side-flip move. Reads the anchors' live
/// mm positions, so it tracks a moved anchor.
fn anchor_x(items: &[Item], inc: &Incidence, i: usize) -> Option<f64> {
    use std::collections::BTreeSet;
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

/// Sub-grid compaction: slide each satellite one grid step toward the drawing's
/// centroid wherever that does NOT raise the routed cost (which already prices
/// whitespace via `spread`, plus length/corners/body-crossings) and creates no
/// overlap. The cell grid can only place parts at column centres with fixed gaps;
/// this closes the slack between them. Strictly cost-gated, so it only ever
/// tightens — it can never regress a layout the optimiser already settled.
fn compact(r: &Realizer, items: &mut [Item]) {
    let sats: Vec<usize> = (0..items.len())
        .filter(|&i| items[i].geom.pins.len() < 3 && !items[i].frozen)
        .collect();
    if sats.is_empty() {
        return;
    }
    let mut best = cost(r, items);
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
                p[axis] += dir * 1.27;
                // Keep a full grid of clearance (compaction must not pack two parts
                // into a touch the readability lint flags even when the bare body
                // rects technically clear).
                let rr = item_rect(&items[i], p);
                let a = [rr[0] - 1.27, rr[1] - 1.27, rr[2] + 1.27, rr[3] + 1.27];
                if items
                    .iter()
                    .enumerate()
                    .any(|(j, it)| j != i && rects_overlap(a, item_rect(it, it.at)))
                {
                    continue;
                }
                items[i].at = p.into();
                let sc = cost(r, items);
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

/// Continuous-placement polish — the post-cell-search optimiser. The cell grid
/// only places parts at column centres; the directed slides (`align_to_pins`,
/// `compact`) and a FREE per-axis nudge recover the sub-grid freedom. Running them
/// run-once-in-sequence is myopic: a part aligned to its pin is never re-considered
/// after a neighbour compacts away. So iterate {align → compact → free-nudge} to a
/// fixpoint, all gated by the real routed cost, so it only ever lowers cost — the
/// references can't regress past their settled minimum, and the busier sheets get the
/// extra freedom to tighten. The emit finalize's `decongest` still guarantees no overlap.
fn polish(r: &Realizer, items: &mut [Item]) {
    // Each pass routes the whole sheet per candidate move, so this is the engine's
    // hot loop. The directed slides converge in 1-2 iterations on the small
    // reference sheets (the break fires early); the cap bounds the cost on a dense
    // board (a 121-ball BGA) where there's always a sub-grid step left to find.
    let mut prev = cost(r, items);
    for _ in 0..3 {
        align_to_pins(r, items);
        compact(r, items);
        free_nudge(r, items);
        let now = cost(r, items);
        if prev - now < 1.0 {
            break; // converged (or not worth another full sweep)
        }
        prev = now;
    }
}

/// Free per-axis nudge: try sliding each satellite ±1 grid in x and y, keeping any
/// move that lowers the routed cost without creating a (clearance-padded) overlap.
/// The directed slides only move a part TOWARD its pin axis or the centroid; this
/// reaches the off-axis positions they can never propose (e.g. a part that should
/// step sideways to uncross a wire), which is the extra freedom `polish` adds over
/// the old align-then-compact.
fn free_nudge(r: &Realizer, items: &mut [Item]) {
    let sats: Vec<usize> = (0..items.len())
        .filter(|&i| items[i].geom.pins.len() < 3 && !items[i].frozen)
        .collect();
    if sats.is_empty() {
        return;
    }
    let mut best = cost(r, items);
    for _ in 0..2 {
        let mut improved = false;
        for &i in &sats {
            let orig = items[i].at;
            let (mut best_pos, mut best_cost) = (orig, best);
            for (axis, dir) in [(0usize, 1.0), (0, -1.0), (1, 1.0), (1, -1.0)] {
                let mut p = orig;
                p[axis] += dir * 1.27;
                // Keep a full grid of clearance, as `compact` does, so a free nudge
                // never packs two parts into a touch the readability lint flags.
                let rr = item_rect(&items[i], p);
                let pad = [rr[0] - 1.27, rr[1] - 1.27, rr[2] + 1.27, rr[3] + 1.27];
                if items
                    .iter()
                    .enumerate()
                    .any(|(j, it)| j != i && rects_overlap(pad, item_rect(it, it.at)))
                {
                    continue;
                }
                items[i].at = p.into();
                let c = cost(r, items);
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

/// Pin-alignment polish: slide each satellite onto the AXIS of the signal pin it
/// wires to, so the connecting wire drops (or runs) straight instead of jogging
/// out from a column centre — a vertical part aligns its x to the pin, a
/// horizontal part its y. The coarse cell grid can only place a part at a column
/// centre, so this sub-column offset is done here on raw positions, kept only
/// when it lowers cost (a straighter, shorter wire) and overlaps nothing.
fn align_to_pins(r: &Realizer, items: &mut [Item]) {
    let env = r.env();
    let inc = r.incidence();
    let ir = r.ir();
    let Ok(w0) = build_writer(env, None, items, inc, ir, r.needs_flag(), false) else {
        return;
    };
    // Per satellite: is it vertical, and where is its signal-pin target?
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
            // A part touching a real IC SIGNAL pin aligns to it (a pull-up over its
            // pin, a series element onto its pin row).
            plans.push((si, vertical, t));
        } else if let Some(t) = supply_pin_target(env, &w0, items, inc, ir, s) {
            // A decoupling/bypass cap with no signal pin hugs the IC SUPPLY pin it
            // bypasses, so it hangs right at that pin instead of drifting to a far
            // frame column (decongest spreads a bank that all wants one pin x).
            plans.push((si, vertical, t));
        }
    }
    drop(w0);

    let mut best = cost(r, items);
    for (si, vertical, target) in plans {
        // Walk one grid step at a time TOWARD the pin axis, keeping the cheapest
        // clear position found. Walking (not jumping) means that when the exact
        // axis is taken — two pull-ups for adjacent IC pins want the same x — the
        // part still slides as close as it can instead of staying put.
        let axis = if vertical { 0 } else { 1 };
        let orig = items[si].at;
        let goal = snap(target[axis]);
        let dir = (goal - orig[axis]).signum();
        if dir == 0.0 {
            continue;
        }
        let (mut best_pos, mut best_cost) = (orig, best);
        let mut p = orig;
        for _ in 0..24 {
            p[axis] += dir * 1.27;
            if (p[axis] - goal) * dir > EPS || overlaps_any(items, si, p) {
                break;
            }
            items[si].at = p.into();
            let c = cost(r, items);
            if c + 0.5 < best_cost {
                best_cost = c;
                best_pos = p;
            }
        }
        items[si].at = best_pos;
        best = best_cost;
    }
}

/// Grid snap — the engine works on the 1.27 mm grid like the seed.
fn snap(v: f64) -> f64 {
    sch_place::grid::snap(v)
}
