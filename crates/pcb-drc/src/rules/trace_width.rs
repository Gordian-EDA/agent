//! `TraceWidthRule` — a trace narrower than the board's `min_trace_width`.

use crate::rules::geom::EPS;
use crate::{DrcCtx, Finding, Rule};

/// Flags any solution trace whose width is below `problem.min_trace_width`.
pub struct TraceWidthRule;

impl Rule for TraceWidthRule {
    fn name(&self) -> &'static str {
        "trace-width-below-min"
    }

    fn check(&self, ctx: &DrcCtx) -> Vec<Finding> {
        let mut out = Vec::new();
        for trace in &ctx.solution.traces {
            if trace.width + EPS < ctx.problem.min_trace_width {
                out.push(Finding::TraceWidthBelowMin {
                    connection: trace.connection.clone(),
                    layer: trace.layer.0.clone(),
                    width: trace.width,
                    required: ctx.problem.min_trace_width,
                });
            }
        }
        out
    }
}
