//! Geometry helpers shared by the clearance and bounds rules.
//!
//! The point/segment/rect distance kernel lives in `geom`; only board-relative
//! helpers live here.

use crate::ctx::CopperItem;
use crate::problem::RouteProblem;

pub(crate) use geom::EPS;

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

/// Do two items share at least one owning connection? (A pad owned by the
/// trace's net, the same net's own copper, etc. — never a clearance conflict.)
pub(crate) fn share_owner(x: &CopperItem, y: &CopperItem) -> bool {
    x.owners.iter().any(|o| y.owned_by(o))
}
