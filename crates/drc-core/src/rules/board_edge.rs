//! `BoardEdgeClearanceRule` — routed copper too close to a CUSTOM outline edge.
//!
//! [`OutOfBoundsRule`](super::out_of_bounds::OutOfBoundsRule) only checks the
//! rectangular bounds; KiCAD checks copper against the actual `Edge.Cuts`
//! POLYGON, so routed copper near a non-rect outline's diagonal edge (invisible
//! to the bbox check) would ship a `copper_edge_clearance` fault. This rule
//! mirrors KiCAD: a routed trace/via's copper edge must clear every outline
//! segment by `EDGE_CLEAR`. Pads are placed copper (kept inside by the placer's
//! copper-extent legality check), so only the router's traces/vias are checked.
//! No outline ⇒ no findings.

use crate::ctx::CopperGeom;
use crate::rules::geom::{EPS, poly_edge_gap};
use crate::{DrcCtx, Finding, Rule};

/// KiCAD's copper-to-board-edge clearance, mm.
const EDGE_CLEAR: f64 = 0.5;

/// Flags routed copper within `EDGE_CLEAR` of a custom board-outline edge.
pub struct BoardEdgeClearanceRule;

impl Rule for BoardEdgeClearanceRule {
    fn name(&self) -> &'static str {
        "board-edge-clearance"
    }

    fn check(&self, ctx: &DrcCtx) -> Vec<Finding> {
        let mut out = Vec::new();
        let Some(poly) = &ctx.problem.outline else {
            return out;
        };
        let pts: Vec<[f64; 2]> = poly.iter().map(|p| [p.x, p.y]).collect();
        for item in &ctx.copper {
            let (gap, half, at) = match &item.geom {
                CopperGeom::Via { at, radius } => (poly_edge_gap(*at, *at, &pts), *radius, *at),
                CopperGeom::Segment { a, b, half_w, .. } => (poly_edge_gap(*a, *b, &pts), *half_w, *a),
                CopperGeom::Rect { .. } => continue,
            };
            if gap < EDGE_CLEAR + half - EPS {
                out.push(Finding::OutOfBounds {
                    connection: item.first_owner(),
                    overshoot: (EDGE_CLEAR + half - gap).max(0.0),
                    at,
                });
            }
        }
        out
    }
}
