//! `drc-lint` — the in-house PCB DRC oracle.
//!
//! Strict clearance/width/via geometry checks (`lint`) plus the independent
//! `connectivity` oracle, measured over a routed `RouteSolution`. The router can
//! be wrong; this is the authority that catches it. Shared types come from
//! `pcb-model` (re-exported here as `problem`).

pub use pcb_model as problem;

pub mod connectivity;
pub mod lint;
