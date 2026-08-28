//! `InvalidLayerRule` — copper or a route point on a layer name that does not
//! resolve for this board's `layer_count`.
//!
//! The grid router silently maps unknown layer names to layer 0; this rule
//! catches it explicitly. Reports the solution traces first (in order), then the
//! connections' route points (in order), matching the canonical report order.

use crate::{DrcCtx, Finding, Rule};

/// Flags any solution trace or connection route point whose layer name resolves
/// to `None` for this board's `layer_count`.
pub struct InvalidLayerRule;

impl Rule for InvalidLayerRule {
    fn name(&self) -> &'static str {
        "invalid-layer"
    }

    fn check(&self, ctx: &DrcCtx) -> Vec<Finding> {
        let layer_count = ctx.problem.layer_count.max(1);
        let mut out = Vec::new();

        for trace in &ctx.solution.traces {
            if trace.layer.index(layer_count).is_none() {
                out.push(Finding::InvalidLayer {
                    connection: trace.connection.clone(),
                    layer: trace.layer.0.clone(),
                    layer_count,
                });
            }
        }
        for conn in &ctx.problem.connections {
            for pt in &conn.points_to_connect {
                if pt.layer.index(layer_count).is_none() {
                    out.push(Finding::InvalidLayer {
                        connection: conn.name.clone(),
                        layer: pt.layer.0.clone(),
                        layer_count,
                    });
                }
            }
        }
        out
    }
}
