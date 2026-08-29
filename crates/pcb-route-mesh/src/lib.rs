//! `pcb-route-mesh` — the premium detailed PCB router.
//!
//! A quadtree **capacity mesh** with **negotiated rip-up/reroute** global pathing
//! (`mesh` + `pathing`), boundary `crossing` assignment, and per-cell `detail`
//! routing, exposed as the premium [`pipeline::NegotiatedMeshRouter`] behind the
//! `pcb-model` [`Router`](pcb_model::Router) trait. The generic
//! [`pipeline::select_best`] selector ranks any injected `&[&dyn Router]` by
//! routability then tidiness; [`pipeline::route_auto`] is the premium-portfolio
//! convenience (direct line-of-sight + via-escape + channel + pcb-route-mesh +
//! pcb-route-grid).
//! Builds on `pcb-route-grid` (the fallback router + grid/astar primitives); DRC via
//! `pcb-drc`; shared types from `pcb-model`.

pub(crate) mod channel;
pub mod copper;
pub mod crossing;
pub mod detail;
pub mod direct;
pub(crate) mod heuristics;
pub(crate) mod layer_hop;
pub mod mesh;
pub mod pathing;
pub(crate) mod pattern;
pub mod pipeline;
pub(crate) mod quality;
pub(crate) mod sequential;
pub(crate) mod via_cleanup;
pub(crate) mod via_escape;
