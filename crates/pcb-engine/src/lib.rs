//! `pcb-engine` — deterministic placement + routing + DRC-lint for KiCAD PCBs.
//!
//! Pure geometry crate; no I/O beyond `serde`. KiCAD file access lives in
//! `kicad-bridge`. The LLM sits *around* this crate, never inside it.
//!
//! Shared data types live in the [`pcb_model`] crate, re-exported here as
//! [`problem`] for back-compat ([`problem::RouteProblem`] / [`problem::RouteSolution`]).

pub mod astar;
pub mod connectivity;
pub mod crossing;
pub mod detail;
pub mod grid;
pub mod lint;
pub mod mesh;
pub mod pathing;
pub mod pipeline;
pub mod placement;
pub use pcb_model as problem;
pub mod router;
pub mod svg;
