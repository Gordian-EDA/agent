//! `negotiated-mesh` — the premium detailed PCB router.
//!
//! A quadtree **capacity mesh** with **negotiated rip-up/reroute** global pathing
//! (`mesh` + `pathing`), boundary `crossing` assignment, and per-cell `detail`
//! routing, plus the `pipeline::route_auto` "best of naive (grid-astar) vs
//! detailed" selector. Builds on `grid-astar` (the naive router + grid/astar
//! primitives); DRC via `drc-lint`; shared types from `pcb-model`.

pub use drc_lint::{connectivity, lint};
pub use grid_astar::{astar, grid, router};
pub use pcb_model as problem;

pub mod crossing;
pub mod detail;
pub mod mesh;
pub mod pathing;
pub mod pipeline;
