//! `pcb-engine` — deterministic copper autorouter for KiCAD PCBs.
//!
//! Pure geometry crate; no I/O beyond `serde`. KiCAD file access lives in
//! `kicad-bridge`. The LLM sits *around* this crate, never inside it.
//!
//! - [`problem`] — [`RouteProblem`] / [`RouteSolution`] data model,
//!   SimpleRouteJson-compatible.

pub mod astar;
pub mod connectivity;
pub mod grid;
pub mod problem;
