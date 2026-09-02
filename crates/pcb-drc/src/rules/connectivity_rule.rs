//! `ConnectivityRule` — folds the [`connectivity`](crate::connectivity) oracle's
//! defects into the unified report as [`Finding::Connectivity`].
//!
//! This makes the suite the single one-stop report: geometry findings first
//! (the earlier rules), then connectivity defects folded in last, exactly as the
//! standard suite reports them.

use crate::{DrcCtx, Rule, connectivity};
use pcb_model::Finding;

/// Runs the connectivity oracle and wraps each [`connectivity::Violation`] in a
/// [`Finding::Connectivity`].
pub struct ConnectivityRule;

impl Rule for ConnectivityRule {
    fn name(&self) -> &'static str {
        "connectivity"
    }

    fn check(&self, ctx: &DrcCtx) -> Vec<Finding> {
        connectivity::check(ctx.problem, ctx.solution)
            .into_iter()
            .map(|violation| Finding::Connectivity { violation })
            .collect()
    }
}
