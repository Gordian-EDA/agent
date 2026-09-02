//! What the premium router is built from.
//!
//! The mesh engine is a composite: it reconciles its copper against a design-rule
//! oracle and falls back to a grid router on the nets its own passes strand.
//! Both are injected as trait objects, so this crate names neither a rule set nor
//! a concrete sub-router.

use pcb_model::{Budget, Drc, PcbRouter};

/// The collaborators and resource envelope one premium routing run is given.
#[derive(Clone, Copy)]
pub struct MeshDeps<'a> {
    /// The geometry/connectivity authority every emitted solution is reconciled
    /// against before it may be reported as routed.
    pub drc: &'a dyn Drc,
    /// The fallback router the rescue passes hand a stranded sub-problem to.
    pub grid: &'a dyn PcbRouter,
    /// The cheap deterministic router that produces the pipeline's seed pass.
    pub grid_seed: &'a dyn PcbRouter,
    /// Wall-clock/effort envelope handed on to the injected sub-routers.
    pub budget: Budget,
}
