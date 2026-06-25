//! `place::refine` — the overlap RELAXERS the emit finalize and the placement engines
//! drive: `decongest` (push colliding bodies apart), `normalize` (seed to the page
//! margin), the multi-sheet `decongest_off_labels` / `collapse_empty_bands` /
//! `port_label_keepouts` passes, and the placement `SEARCH_SEED` + `FAST_PINS` thresholds.
//! These are pure geometry — no cost, no objective; the cost-driven scaffold (greedy's
//! `refine`/`polish`) lives in the `greedy-place` engine, the SA in `anneal-place`.

use std::io;

use kicad_cli::env::KicadEnv;

use crate::write::SchematicWriter;
use sch_place::geom::Dir;

use super::*;
use sch_place::item::{Incidence, Item};

// The disjoint-set forest (over a caller-owned `parent` slice) lives in
// `sch_place::union_find`, shared with circuit-lang's pin reconciler.
use sch_place::ir::LayoutIr;

/// Default deterministic seed for the placement search (a stochastic engine's PRNG).
/// Threaded into the `PlaceProblem` by emit so a search is reproducible by seed.
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

/// Whether placing item `si` at `at` would overlap any other item's body.
pub fn overlaps_any(items: &[Item], si: usize, at: impl Into<::geom::Point2>) -> bool {
    let at = at.into();
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
                let (a, b) = (
                    item_rect(&items[i], items[i].at),
                    item_rect(&items[j], items[j].at),
                );
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
        let dir = if items[j].at[axis] >= items[i].at[axis] {
            1.0
        } else {
            -1.0
        };
        let (i_anchor, j_anchor) = (items[i].geom.pins.len() >= 3, items[j].geom.pins.len() >= 3);
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
        let mut iv: Vec<(f64, f64)> = items
            .iter()
            .map(|it| {
                let r = item_rect(it, it.at);
                (r[1], r[3])
            })
            .collect();
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
pub(crate) fn decongest_off_labels(
    items: &mut [Item],
    inc: &Incidence,
    keepouts: &[([f64; 4], String)],
) {
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
                let (a, b) = (
                    item_rect(&items[i], items[i].at),
                    item_rect(&items[j], items[j].at),
                );
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
            let dir = if items[j].at[axis] >= items[i].at[axis] {
                1.0
            } else {
                -1.0
            };
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
            ks.push((
                port_label_obstacle(port_exit_point(&eps, s), s, net),
                net.clone(),
            ));
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
        it.at = [it.at[0] + dx, it.at[1] + dy].into();
    }
}
