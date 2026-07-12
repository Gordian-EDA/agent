//! `negotiated-mesh` — the premium detailed PCB router.
//!
//! A quadtree **capacity mesh** with **negotiated rip-up/reroute** global pathing
//! (`mesh` + `pathing`), boundary `crossing` assignment, and per-cell `detail`
//! routing, exposed as the premium [`pipeline::NegotiatedMeshRouter`] behind the
//! `pcb-model` [`Router`](pcb_model::Router) trait. The generic
//! [`pipeline::select_best`] selector ranks any injected `&[&dyn Router]` by
//! routability then tidiness; [`pipeline::route_auto`] is the premium-portfolio
//! convenience (direct line-of-sight + via-escape + channel + negotiated-mesh +
//! grid-astar).
//! Builds on `grid-astar` (the fallback router + grid/astar primitives); DRC via
//! `drc-lint`; shared types from `pcb-model`.

pub use drc_lint::{connectivity, lint};
pub use grid_astar::{astar, grid, router};
pub use pcb_model as problem;

pub mod channel;
pub(crate) mod copper;
pub mod crossing;
pub mod detail;
pub mod direct;
pub(crate) mod heuristics;
pub mod layer_hop;
pub mod mesh;
pub mod pathing;
pub mod pattern;
pub mod pipeline;
pub(crate) mod quality;
pub mod sequential;
pub(crate) mod via_cleanup;
pub mod via_escape;
