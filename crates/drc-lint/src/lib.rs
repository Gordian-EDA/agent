//! `drc-lint` — the in-house PCB DRC oracle (the agent-facing facade).
//!
//! Strict clearance/width/via geometry checks ([`lint::lint`]) plus the
//! independent [`connectivity`] oracle, measured over a routed `RouteSolution`.
//! The router can be wrong; this is the authority that catches it.
//!
//! The checks themselves now live in the pluggable engine kernel
//! [`pcb_drc_core`] (a `Rule` per check, composed by a `DrcSuite`); this crate is
//! the stable facade the router, board harness, and route-quality scorers use.
//! [`lint::lint`] runs [`pcb_drc_core::DrcSuite::standard`] and adds the two
//! copper-dropping helpers ([`lint::drop_violating_copper`],
//! [`lint::drop_unconnected_copper`]) built on top of the report. A third party
//! adds a rule against `pcb-drc-core` directly, with no edit here.
//!
//! Shared types come from `pcb-model` (re-exported here as [`problem`]). The
//! report type [`DrcViolation`] (= [`pcb_drc_core::Finding`]) is re-exported at
//! the crate root for convenience.

pub use pcb_model as problem;

pub mod connectivity;
pub mod lint;

pub use lint::DrcViolation;
