//! `place::refine` — overlap relaxers and placement thresholds.

use super::*;
use sch_place::item::Item;

/// Default deterministic seed for the placement search (a stochastic engine's PRNG).
/// Threaded into the `PlacementView` by emit so a search is reproducible by seed.
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
        .any(|(j, it)| j != si && a.overlaps(&item_rect(it, it.at)))
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
                // A pair of FROZEN parts can't be separated (a finalize gather pins both),
                // so skip it — otherwise the loop spins on an immovable overlap. Pairs with
                // no frozen part behave exactly as before (references freeze nothing).
                if items[i].frozen && items[j].frozen {
                    continue;
                }
                let (a, b) = (
                    item_rect(&items[i], items[i].at),
                    item_rect(&items[j], items[j].at),
                );
                if a.overlaps(&b) {
                    hit = Some((i, j, a, b));
                    break 'scan;
                }
            }
        }
        let Some((i, j, a, b)) = hit else { break };
        let Some((pen_x, pen_y)) = a.overlap_size(&b) else {
            continue;
        };
        let axis = if pen_x <= pen_y { 0 } else { 1 };
        let pen = if axis == 0 { pen_x } else { pen_y };
        let grid = geom::GRID_50_MIL;
        let push = grid.snap_up(pen).max(grid.pitch());
        // Move j away from i along `axis` (deterministic by the +side of i).
        let dir = if items[j].at[axis] >= items[i].at[axis] {
            1.0
        } else {
            -1.0
        };
        // A FROZEN part (a finalize gather seated it precisely — a tap ladder, a
        // decoupling bank) never moves; push only its partner. This lets a gather
        // reserve its block and have the loose bystanders flow around it, instead of
        // the gather having to abort whenever the scattered sheet leaves no clear lane.
        // Inert for the references (nothing frozen) ⇒ snapshots byte-identical.
        let (i_anchor, j_anchor) = (items[i].geom.pins.len() >= 3, items[j].geom.pins.len() >= 3);
        match (items[i].frozen, items[j].frozen) {
            (true, false) => items[j].at[axis] += dir * push,
            (false, true) => items[i].at[axis] -= dir * push,
            _ => match (i_anchor, j_anchor) {
                (false, true) => items[i].at[axis] -= dir * push,
                (true, false) => items[j].at[axis] += dir * push,
                _ => {
                    let half = grid.snap_up(push / 2.0);
                    items[i].at[axis] -= dir * half;
                    items[j].at[axis] += dir * half;
                }
            },
        }
    }
}

pub fn normalize(items: &mut [Item]) {
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
