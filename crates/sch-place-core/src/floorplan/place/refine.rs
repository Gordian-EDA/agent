//! `place::refine` — the placement scaffold the engines drive: the greedy
//! `refine_items` hill-climb, the routed `polish`, and the overlap relaxers
//! (`decongest`, `compact`, `free_nudge`) plus the seed `normalize`.

use std::collections::BTreeSet;
use std::io;

use kicad_cli_rs::env::KicadEnv;

use crate::write::SchematicWriter;
use sch_model::geom::Dir;

use super::*;
use sch_model::item::{Incidence, Item};

// The disjoint-set forest (over a caller-owned `parent` slice) lives in
// `sch_model::union_find`, shared with circuit-lang's pin reconciler.
use sch_model::ir::{LayoutIr, Orient};


// ---------------------------------------------------------------------------
// Refinement — nudge satellites for a tidier routed result (anchors fixed).
// ---------------------------------------------------------------------------

/// Hill-climb the satellite (2-pin) cells with the anchors held fixed, accepting
/// only strict improvements to the routed-layout cost. Because every candidate
/// is scored on the ACTUAL routing — not a placement proxy — the loop can never
/// trade a clean wire for a hidden short or label fallback, and it can only
/// improve on (or match) the starting placement. This is the "move and align
/// until it looks good" step a human does after roughing in anchors + satellites.
/// Default deterministic seed for the placement search (the SA's PRNG). Made a
/// parameter so a search is reproducible by seed, not a hard-coded constant.
pub(crate) const SEARCH_SEED: u64 = 0xD1B54A32D192ED03;

/// Pin-count threshold above which the premium anneal takes the router-free FAST
/// LANE. The tuned routed paths (greedy refine + anneals A/B/C + routed polish)
/// route the WHOLE sheet per move, which is fine on the ≤34-pin reference/snapshot
/// fixtures (<1.2 s) but explodes past ~60 pins (a 119-pin agent board took 113 s).
/// Above this, the search uses only the router-free `proxy_cost` (path D) + a
/// router-free `polish_proxy`, paying the true routed cost only a bounded number of
/// times (candidate selection + the one final emit). Set above every reference/
/// snapshot fixture (max 34 pins) so those stay on the exact tuned path —
/// byte-identical snapshots and tuned-fixture quality are untouched. (Set to 34 =
/// the largest reference/snapshot fixture, uart, so EVERY board above it — including
/// the 35-49-pin agent boards whose tuned routed path ran 4-6 s — takes the fast
/// lane; the `> FAST_PINS` test keeps uart itself routed, hence byte-identical.)
pub const FAST_PINS: usize = 34;



/// Greedy hill-climb over the satellites' mm positions/orientation (the seed is
/// the IR grid projected to mm). Local moves — nudge a cell-step, swap a pair,
/// re-orient, side-flip across the served IC — each kept only on strict
/// improvement of the REAL routed cost (`score_items`). Anchors hold. Operating
/// directly on mm means the objective IS the geometry that ships.
pub fn refine_items(
    env: &KicadEnv,
    items: &mut [Item],
    inc: &Incidence,
    ir: &LayoutIr,
    needs_flag: &BTreeSet<String>,
) {
    let _ = ir;
    let satellites: Vec<usize> =
        (0..items.len()).filter(|&i| items[i].geom.pins.len() < 3 && !items[i].frozen).collect();
    if satellites.is_empty() {
        return;
    }
    let mut best = score_items(env, items, inc, ir, needs_flag);
    const MAX_ROUNDS: usize = 6;
    for _ in 0..MAX_ROUNDS {
        let mut improved = false;
        // Single-part nudges: shift one satellite by one cell-step (grid-snapped).
        for &i in &satellites {
            for d in [[COL_GAP, 0.0], [-COL_GAP, 0.0], [0.0, ROW_GAP], [0.0, -ROW_GAP]] {
                let prev = items[i].at;
                items[i].at = [crate::grid::snap(prev[0] + d[0]), crate::grid::snap(prev[1] + d[1])];
                let c = score_items(env, items, inc, ir, needs_flag);
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
                let c = score_items(env, items, inc, ir, needs_flag);
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
                let c = score_items(env, items, inc, ir, needs_flag);
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
            if let Some(ax) = anchor_x(items, inc, i) {
                let nx = crate::grid::snap(2.0 * ax - items[i].at[0]);
                if (nx - items[i].at[0]).abs() > EPS {
                    let prev = items[i].at;
                    items[i].at = [nx, prev[1]];
                    let c = score_items(env, items, inc, ir, needs_flag);
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
pub(crate) fn anchor_x(items: &[Item], inc: &Incidence, i: usize) -> Option<f64> {
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
pub(crate) fn compact(
    env: &KicadEnv,
    items: &mut [Item],
    inc: &Incidence,
    ir: &LayoutIr,
    needs_flag: &BTreeSet<String>,
) {
    let sats: Vec<usize> = (0..items.len()).filter(|&i| items[i].geom.pins.len() < 3 && !items[i].frozen).collect();
    if sats.is_empty() {
        return;
    }
    let mut best = score_items(env, items, inc, ir, needs_flag);
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
                let r = item_rect(&items[i], p);
                let a = [r[0] - 1.27, r[1] - 1.27, r[2] + 1.27, r[3] + 1.27];
                if items
                    .iter()
                    .enumerate()
                    .any(|(j, it)| j != i && rects_overlap(a, item_rect(it, it.at)))
                {
                    continue;
                }
                items[i].at = p;
                let sc = score_items(env, items, inc, ir, needs_flag);
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
/// fixpoint, all gated by the real routed cost (`score_items`), so it only ever
/// lowers cost — the references can't regress past their settled minimum, and the
/// busier sheets get the extra freedom to tighten. `decongest` still guarantees
/// no overlap afterwards.
pub fn polish(
    env: &KicadEnv,
    items: &mut [Item],
    inc: &Incidence,
    ir: &LayoutIr,
    needs_flag: &BTreeSet<String>,
) {
    // Each pass routes the whole sheet per candidate move, so this is the engine's
    // hot loop. The directed slides converge in 1-2 iterations on the small
    // reference sheets (the break fires early); the cap bounds the cost on a dense
    // board (a 121-ball BGA) where there's always a sub-grid step left to find.
    let mut prev = score_items(env, items, inc, ir, needs_flag);
    for _ in 0..3 {
        align_to_pins(env, items, inc, ir, needs_flag);
        compact(env, items, inc, ir, needs_flag);
        free_nudge(env, items, inc, ir, needs_flag);
        let now = score_items(env, items, inc, ir, needs_flag);
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
pub(crate) fn free_nudge(
    env: &KicadEnv,
    items: &mut [Item],
    inc: &Incidence,
    ir: &LayoutIr,
    needs_flag: &BTreeSet<String>,
) {
    let sats: Vec<usize> = (0..items.len()).filter(|&i| items[i].geom.pins.len() < 3 && !items[i].frozen).collect();
    if sats.is_empty() {
        return;
    }
    let mut best = score_items(env, items, inc, ir, needs_flag);
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
                let c = score_items(env, items, inc, ir, needs_flag);
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




/// Whether placing item `si` at `at` would overlap any other item's body.
pub fn overlaps_any(items: &[Item], si: usize, at: [f64; 2]) -> bool {
    let a = item_rect(&items[si], at);
    items
        .iter()
        .enumerate()
        .any(|(j, it)| j != si && rects_overlap(a, item_rect(it, it.at)))
}

/// Final overlap relaxation (deterministic): push any two overlapping bodies
/// apart along their axis of least penetration, snapped to the grid, until the
/// sheet is collision-free or a hard iteration cap is hit. ICs (anchors) hold
/// when paired with a 2-pin part — the satellite yields; two of a kind split the
/// push. Only positions move, so connectivity is untouched and the router redraws
/// around the new placement on the following pass.
pub fn decongest(items: &mut [Item]) {
    const MAX_ITERS: usize = 3000;
    for _ in 0..MAX_ITERS {
        // First overlapping pair in a fixed order (determinism).
        let mut hit = None;
        'scan: for i in 0..items.len() {
            for j in (i + 1)..items.len() {
                let (a, b) = (item_rect(&items[i], items[i].at), item_rect(&items[j], items[j].at));
                if rects_overlap(a, b) {
                    hit = Some((i, j, a, b));
                    break 'scan;
                }
            }
        }
        let Some((i, j, a, b)) = hit else { break };
        let pen_x = (a[2].min(b[2]) - a[0].max(b[0])).max(0.0);
        let pen_y = (a[3].min(b[3]) - a[1].max(b[1])).max(0.0);
        let axis = if pen_x <= pen_y { 0 } else { 1 };
        let pen = if axis == 0 { pen_x } else { pen_y };
        let push = ((pen / 1.27).ceil() * 1.27).max(1.27);
        // Move j away from i along `axis` (deterministic by the +side of i).
        let dir = if items[j].at[axis] >= items[i].at[axis] { 1.0 } else { -1.0 };
        let (i_anchor, j_anchor) =
            (items[i].geom.pins.len() >= 3, items[j].geom.pins.len() >= 3);
        match (i_anchor, j_anchor) {
            (false, true) => items[i].at[axis] -= dir * push,
            (true, false) => items[j].at[axis] += dir * push,
            _ => {
                let half = (push / 2.0 / 1.27).ceil() * 1.27;
                items[i].at[axis] -= dir * half;
                items[j].at[axis] += dir * half;
            }
        }
    }
}

/// Collapse a large EMPTY horizontal band between two clusters — the "sprawls with an empty
/// mid-region" defect (two loosely-coupled sub-circuits, e.g. a USB connector block and its
/// LDO/decoupling block, placed far apart on one sub-sheet). Surgical: finds the FIRST gap
/// between part rows wider than `TRIGGER` and shifts everything below it UP to leave a clean
/// `MIN_GAP`, preserving each cluster's internal layout. Repeats for further bands (capped).
/// Returns whether anything moved. Gated by the caller on MULTISHEET_REFINE.
pub(crate) fn collapse_empty_bands(items: &mut [Item]) -> bool {
    const MIN_GAP: f64 = 12.7;
    const TRIGGER: f64 = 25.4;
    let mut any = false;
    for _ in 0..8 {
        let mut iv: Vec<(f64, f64)> =
            items.iter().map(|it| { let r = item_rect(it, it.at); (r[1], r[3]) }).collect();
        iv.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut cover = f64::MIN;
        let mut band = None;
        for (lo, hi) in &iv {
            if cover != f64::MIN && lo - cover > TRIGGER {
                band = Some((cover, *lo));
                break;
            }
            cover = cover.max(*hi);
        }
        let Some((top, bot)) = band else { break };
        let dy = (bot - top) - MIN_GAP;
        if dy <= 0.0 {
            break;
        }
        for it in items.iter_mut() {
            if it.at[1] > top {
                it.at[1] = crate::grid::snap(it.at[1] - dy);
            }
        }
        any = true;
    }
    any
}

/// Multi-sheet relief: decongest that ALSO keeps free satellites off FOREIGN port-label
/// boxes. Plain `decongest` (part-vs-part only) evicts a satellite off a connector straight
/// back onto a neighbor's port-label pennant — the two passes fight and the label stays
/// overprinted (the storage-sheet SD_MOSI/R8 defect). Resolving both in ONE loop lets a
/// satellite settle where it clears parts AND foreign labels. Net-aware: a part is never
/// pushed off its OWN port's label (that label extends away from it anyway). Gated by the
/// caller on `MULTISHEET_REFINE`, so single-sheet references never reach it.
pub(crate) fn decongest_off_labels(items: &mut [Item], inc: &Incidence, keepouts: &[([f64; 4], String)]) {
    let item_nets: Vec<Vec<String>> = (0..items.len())
        .map(|i| {
            inc.iter()
                .filter(|(_, pins)| pins.iter().any(|(j, _)| *j == i))
                .map(|(net, _)| net.clone())
                .collect()
        })
        .collect();
    const MAX_ITERS: usize = 4000;
    for _ in 0..MAX_ITERS {
        // (a) Resolve the first part-vs-part overlap (identical to `decongest`).
        let mut part_hit = None;
        'scan: for i in 0..items.len() {
            for j in (i + 1)..items.len() {
                let (a, b) = (item_rect(&items[i], items[i].at), item_rect(&items[j], items[j].at));
                if rects_overlap(a, b) {
                    part_hit = Some((i, j, a, b));
                    break 'scan;
                }
            }
        }
        if let Some((i, j, a, b)) = part_hit {
            let pen_x = (a[2].min(b[2]) - a[0].max(b[0])).max(0.0);
            let pen_y = (a[3].min(b[3]) - a[1].max(b[1])).max(0.0);
            let axis = if pen_x <= pen_y { 0 } else { 1 };
            let pen = if axis == 0 { pen_x } else { pen_y };
            let push = ((pen / 1.27).ceil() * 1.27).max(1.27);
            let dir = if items[j].at[axis] >= items[i].at[axis] { 1.0 } else { -1.0 };
            let (ia, ja) = (items[i].geom.pins.len() >= 3, items[j].geom.pins.len() >= 3);
            match (ia, ja) {
                (false, true) => items[i].at[axis] -= dir * push,
                (true, false) => items[j].at[axis] += dir * push,
                _ => {
                    let half = (push / 2.0 / 1.27).ceil() * 1.27;
                    items[i].at[axis] -= dir * half;
                    items[j].at[axis] += dir * half;
                }
            }
            continue;
        }
        // (b) Else push the first free satellite that sits on a FOREIGN port-label box out of
        //     it, along the shorter exit, toward the side it is already closer to leaving.
        let mut lab_hit = None;
        'scan2: for i in 0..items.len() {
            if items[i].geom.pins.len() >= 3 || items[i].frozen {
                continue;
            }
            let a = item_rect(&items[i], items[i].at);
            for (bx, net) in keepouts {
                if !item_nets[i].contains(net) && rects_overlap(a, *bx) {
                    lab_hit = Some((i, a, *bx));
                    break 'scan2;
                }
            }
        }
        let Some((i, a, b)) = lab_hit else { break };
        let pen_x = (a[2].min(b[2]) - a[0].max(b[0])).max(0.0);
        let pen_y = (a[3].min(b[3]) - a[1].max(b[1])).max(0.0);
        let axis = if pen_x <= pen_y { 0 } else { 1 };
        let pen = if axis == 0 { pen_x } else { pen_y };
        let push = ((pen / 1.27).ceil() * 1.27).max(1.27);
        let ci = (a[axis] + a[axis + 2]) / 2.0;
        let cb = (b[axis] + b[axis + 2]) / 2.0;
        let dir = if ci >= cb { 1.0 } else { -1.0 };
        items[i].at[axis] = crate::grid::snap(items[i].at[axis] + dir * push);
    }
}

/// Port-label keepout boxes: for each port net, the pennant box at its exit (the same box the router
/// already reserves at emit, but computed here so PLACEMENT can keep satellites off it). Built from
/// the writer's pin geometry; the port-owning part is an anchor that won't move, so these stay valid
/// across the satellite nudge below.
pub(crate) fn port_label_keepouts(
    env: &KicadEnv,
    w: &mut SchematicWriter,
    items: &[Item],
    inc: &Incidence,
    ir: &LayoutIr,
) -> io::Result<Vec<([f64; 4], String)>> {
    let mut ks = Vec::new();
    for (net, side) in &ir.ports {
        let Some(pins) = inc.get(net) else { continue };
        let mut eps: Vec<([f64; 2], Dir)> = Vec::new();
        for (i, num) in pins {
            for (ep, dir) in w.pin_dirs(env, &items[*i].refdes, num)? {
                eps.push((ep, dir));
            }
        }
        if eps.is_empty() {
            continue;
        }
        if let Some(s) = effective_port_side(Some(*side), &eps) {
            ks.push((port_label_obstacle(port_exit_point(&eps, s), s, net), net.clone()));
        }
    }
    Ok(ks)
}

pub(crate) fn normalize(items: &mut [Item]) {
    let (mut min_x, mut min_y) = (f64::MAX, f64::MAX);
    for it in items.iter() {
        min_x = min_x.min(it.at[0]);
        min_y = min_y.min(it.at[1]);
    }
    if !min_x.is_finite() {
        return;
    }
    let dx = MARGIN - min_x;
    let dy = MARGIN - min_y;
    for it in items.iter_mut() {
        it.at = [it.at[0] + dx, it.at[1] + dy];
    }
}
