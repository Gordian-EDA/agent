//! `pcb-engine` — deterministic placement + routing + DRC-lint for KiCAD PCBs.
//!
//! Pure geometry crate; no I/O beyond `serde`. KiCAD file access lives in
//! `kicad-bridge`. The LLM sits *around* this crate, never inside it.
//!
//! Shared data types live in the [`pcb_model`] crate, re-exported here as
//! [`problem`] for back-compat ([`problem::RouteProblem`] / [`problem::RouteSolution`]).

pub mod placement;
pub use drc_lint::{connectivity, lint};
pub use grid_astar::{astar, grid, router};
pub use negotiated_mesh::{crossing, detail, mesh, pathing, pipeline};
pub use pcb_model as problem;
pub mod svg;
