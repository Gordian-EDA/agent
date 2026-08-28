//! `ViaDiameterRule` — a via below KiCAD's minimum diameter for its type.
//!
//! Through/blind/buried vias must meet the netclass via diameter
//! (`problem.via_diameter` — what kicad-cli enforces); only true micro vias get
//! the relaxed microvia floor. Iterates `solution.vias` directly (the copper
//! model drops the span/type), so it's the one rule that knows micro-vs-through.

use crate::problem::ViaSpan;
use crate::rules::geom::EPS;
use crate::{DrcCtx, Finding, Rule};

/// KiCAD's relaxed minimum diameter for a true MICRO via (laser, adjacent-layer).
/// Matches the netclass `microvia_diameter` the board export writes (= 0.3 mm).
/// Through/blind vias instead use `problem.via_diameter`.
const MICRO_VIA_MIN_DIAMETER: f64 = 0.3;

/// Flags any via whose diameter is below its type's minimum.
pub struct ViaDiameterRule;

impl Rule for ViaDiameterRule {
    fn name(&self) -> &'static str {
        "via-diameter-below-min"
    }

    fn check(&self, ctx: &DrcCtx) -> Vec<Finding> {
        let mut out = Vec::new();
        for v in &ctx.solution.vias {
            let required = match v.span {
                ViaSpan::Partial { micro: true, .. } => MICRO_VIA_MIN_DIAMETER,
                _ => ctx.problem.via_diameter,
            };
            if v.diameter + EPS < required {
                out.push(Finding::ViaDiameterBelowMin {
                    connection: v.connection.clone(),
                    diameter: v.diameter,
                    required,
                    at: v.at,
                });
            }
        }
        out
    }
}
