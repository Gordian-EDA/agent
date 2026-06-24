//! `OutOfBoundsRule` — routed copper whose extent leaves the rectangular board
//! bounds.
//!
//! Obstacles are *inputs* (the board's own pads/keepouts), not router output, so
//! they are never flagged — only emitted traces and vias.

use crate::ctx::{CopperGeom, CopperItem};
use crate::problem::RouteProblem;
use crate::rules::geom::{EPS, point_overshoot};
use crate::{DrcCtx, Finding, Rule};

/// Flags any trace or via whose copper extent (segment fattened by its
/// half-width, via by its radius) pokes past the board's `bounds`.
pub struct OutOfBoundsRule;

impl Rule for OutOfBoundsRule {
    fn name(&self) -> &'static str {
        "out-of-bounds"
    }

    fn check(&self, ctx: &DrcCtx) -> Vec<Finding> {
        let mut out = Vec::new();
        for item in &ctx.copper {
            if let Some(v) = out_of_bounds(item, ctx.problem) {
                out.push(v);
            }
        }
        out
    }
}

/// How far the item's copper extent (segment fattened by half-width, via by its
/// radius, rect as-is) leaves the board bounds, or `None` if it is inside.
fn out_of_bounds(item: &CopperItem, problem: &RouteProblem) -> Option<Finding> {
    let (overshoot, at, owner) = match &item.geom {
        CopperGeom::Segment {
            a, b: bb, half_w, ..
        } => {
            let o_a = point_overshoot(*a, *half_w, problem);
            let o_b = point_overshoot(*bb, *half_w, problem);
            let (over, at) = if o_a.0 >= o_b.0 {
                (o_a.0, *a)
            } else {
                (o_b.0, *bb)
            };
            (over, at, item.first_owner())
        }
        CopperGeom::Via { at, radius } => {
            let (over, _) = point_overshoot(*at, *radius, problem);
            (over, *at, item.first_owner())
        }
        CopperGeom::Rect { .. } => return None,
    };
    if overshoot > EPS {
        Some(Finding::OutOfBounds {
            connection: owner,
            overshoot,
            at,
        })
    } else {
        None
    }
}
