//! The placement cost the annealer minimizes (and the routability oracle's
//! secondary selection key). The cost carries a SILK-GAP term so parts keep room
//! for their reference designators (the recurring critic complaint). HPWL — the
//! cheap quality number every engine reports — lives in the kernel
//! ([`pcb_model::place::compute_hpwl`]) and is re-exported here.

use super::geometry::{courtyard_overlap, part_keepout_overlap};
use super::model::{LogicalNet, PlaceProblem};
use crate::problem::Point2;

pub(crate) use crate::problem::place::compute_hpwl;

/// SA cost weights (mm units), scaled like the schematic floorplan cost.
pub(crate) const SA_OVERLAP_W: f64 = 1000.0; // hard: courtyard collision
pub(crate) const SA_BOUNDS_W: f64 = 1000.0; // hard: out of board bounds
pub(crate) const SA_KEEPOUT_W: f64 = 1000.0; // hard: part overlapping a signal-layer keep-out
pub(crate) const SA_SILK_W: f64 = 6.0; // soft: parts crowding each other's refdes
pub(crate) const SA_WL_W: f64 = 0.4; // half-perimeter wirelength (over part centres)
pub(crate) const SA_SPREAD_W: f64 = 0.25; // mild whole-board compaction
pub(crate) const SA_COHERE_W: f64 = 8.0; // decoupling cap → nearest anchor power pad (hug the IC).
// Deliberately ABOVE SA_SILK_W (refdes-crowding): a bypass cap hugging its IC is an
// electrical necessity that must outrank silk aesthetics, else a big cap (1210) next to a
// small IC (SOIC-8) gets pushed away by the crowding penalty and strands (critic-caught on
// power-buck). Targeted to detected decoupling PAIRS only, so it does not perturb parts
// with normal net springs.
pub(crate) const SA_EDGE_W: f64 = 2.5; // connector → nearest board edge
/// Breathing room (mm) a refdes needs around a part before it crowds a neighbour.
pub(crate) const SA_SILK_GAP: f64 = 1.0;

/// Distance from a decoupling cap's origin to the NEAREST power pad of its anchor
/// (the proximity a bypass cap should minimize). 0 if the anchor shares no pad net.
fn cap_anchor_dist(
    problem: &PlaceProblem,
    pos: &[Point2],
    rotations: &[i32],
    cap: usize,
    ic: usize,
) -> f64 {
    let cap_nets: Vec<&str> =
        problem.parts[cap].pads.iter().filter_map(|p| p.net.as_deref()).collect();
    let mut best = f64::MAX;
    for pad in &problem.parts[ic].pads {
        if pad.net.as_deref().is_some_and(|nn| cap_nets.contains(&nn)) {
            let off = pad.offset.rotate(rotations[ic] as f64);
            let (px, py) = (pos[ic].x + off.x, pos[ic].y + off.y);
            best = best.min(((pos[cap].x - px).powi(2) + (pos[cap].y - py).powi(2)).sqrt());
        }
    }
    if best.is_finite() { best } else { 0.0 }
}

/// The placement cost the SA minimizes (also the [`crate::placement::place_best`]
/// selection key, so the variant that genuinely lays out best is the one chosen).
/// Lower is better.
#[allow(clippy::too_many_arguments)] // internal SA cost kernel; arg-struct adds indirection without value
pub(crate) fn place_cost(
    problem: &PlaceProblem,
    nets: &[LogicalNet],
    half: &[(f64, f64)],
    margin: f64,
    rotations: &[i32],
    pairs: &[(usize, usize)],
    edge_idx: &[usize],
    pos: &[Point2],
) -> f64 {
    let n = problem.parts.len();
    let mut cost = 0.0;

    // Pairwise courtyard overlap (hard) + a soft silk gap so refdes don't crowd.
    for i in 0..n {
        for j in (i + 1)..n {
            let (ox, oy) = courtyard_overlap(pos, half, margin, i, j);
            if ox > 0.0 && oy > 0.0 {
                cost += SA_OVERLAP_W * ox.min(oy);
            } else {
                let (sx, sy) = courtyard_overlap(pos, half, margin + 2.0 * SA_SILK_GAP, i, j);
                if sx > 0.0 && sy > 0.0 {
                    cost += SA_SILK_W * sx.min(sy);
                }
            }
        }
    }

    // Out-of-bounds (hard).
    let b = &problem.bounds;
    for i in 0..n {
        let h = half[i];
        let dx = (b.min_x - (pos[i].x - h.0)).max(0.0) + ((pos[i].x + h.0) - b.max_x).max(0.0);
        let dy = (b.min_y - (pos[i].y - h.1)).max(0.0) + ((pos[i].y + h.1) - b.max_y).max(0.0);
        cost += SA_BOUNDS_W * (dx + dy);
    }

    // Keep-out overlap (hard): a part inside a signal-layer keep-out has trapped
    // pads. Penalize the penetration depth so the SA pushes parts clear.
    for i in 0..n {
        for k in &problem.keepouts {
            let (ox, oy) = part_keepout_overlap(&pos[i], half[i], k);
            if ox > 0.0 && oy > 0.0 {
                cost += SA_KEEPOUT_W * ox.min(oy);
            }
        }
    }

    // Half-perimeter wirelength over part centres + whole-board spread.
    let (mut gx0, mut gy0, mut gx1, mut gy1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for p in pos {
        gx0 = gx0.min(p.x);
        gy0 = gy0.min(p.y);
        gx1 = gx1.max(p.x);
        gy1 = gy1.max(p.y);
    }
    if gx1 >= gx0 {
        cost += SA_SPREAD_W * ((gx1 - gx0) + (gy1 - gy0));
    }
    for net in nets {
        if net.pins.len() < 2 {
            continue;
        }
        let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for pin in &net.pins {
            let p = &pos[pin.part];
            x0 = x0.min(p.x);
            y0 = y0.min(p.y);
            x1 = x1.max(p.x);
            y1 = y1.max(p.y);
        }
        cost += SA_WL_W * ((x1 - x0) + (y1 - y0));
    }

    // Decoupling cohesion + connector edge-seek.
    for &(cap, ic) in pairs {
        cost += SA_COHERE_W * cap_anchor_dist(problem, pos, rotations, cap, ic);
    }
    for &i in edge_idx {
        let h = half[i];
        let dl = (pos[i].x - h.0) - b.min_x;
        let dr = b.max_x - (pos[i].x + h.0);
        let dt = (pos[i].y - h.1) - b.min_y;
        let db = b.max_y - (pos[i].y + h.1);
        cost += SA_EDGE_W * dl.min(dr).min(dt).min(db).max(0.0);
    }
    cost
}
