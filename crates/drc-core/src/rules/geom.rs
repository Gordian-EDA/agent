//! Geometry helpers shared by the clearance and bounds rules.
//!
//! The point/segment/rect distance kernel lives in [`pcb_model::geom2d`] (shared
//! with the connectivity oracle); only the board-relative helpers (overshoot,
//! polygon-edge gap, ownership) live here.

use crate::ctx::CopperItem;
use crate::problem::RouteProblem;

/// Geometric slop, mm. A gap is only a violation when it falls short of the
/// required clearance by more than this.
pub(crate) const EPS: f64 = 1e-6;

/// How far a disc of `radius` centred at `p` pokes past the nearest board edge
/// (positive = outside), plus the point itself. Zero or negative = inside.
pub(crate) fn point_overshoot(p: [f64; 2], radius: f64, problem: &RouteProblem) -> (f64, [f64; 2]) {
    let b = &problem.bounds;
    let left = (b.min_x - (p[0] - radius)).max(0.0);
    let right = ((p[0] + radius) - b.max_x).max(0.0);
    let top = (b.min_y - (p[1] - radius)).max(0.0);
    let bottom = ((p[1] + radius) - b.max_y).max(0.0);
    (left.max(right).max(top).max(bottom), p)
}

/// Minimum distance from segment `pq` (a via passes p == q) to the boundary of polygon `poly` —
/// i.e. to its nearest edge. Used to verify routed copper clears a custom board outline.
pub(crate) fn poly_edge_gap(p: [f64; 2], q: [f64; 2], poly: &[[f64; 2]]) -> f64 {
    let n = poly.len();
    if n < 2 {
        return f64::INFINITY;
    }
    let pq = geom::Segment::new(p.into(), q.into());
    let mut best = f64::INFINITY;
    for i in 0..n {
        let g = pq.dist_to_segment(geom::Segment::new(poly[i].into(), poly[(i + 1) % n].into()));
        if g < best {
            best = g;
        }
    }
    best
}

/// Do two items share at least one owning connection? (A pad owned by the
/// trace's net, the same net's own copper, etc. — never a clearance conflict.)
pub(crate) fn share_owner(x: &CopperItem, y: &CopperItem) -> bool {
    x.owners.iter().any(|o| y.owned_by(o))
}
