//! `pcb-svg` — diagnostic SVG renderer for the engine's *own* view of a board.
//!
//! Draws placement, routed copper, vias, the capacity mesh, and failed-net
//! highlights — the fast in-loop view (no KiCAD needed), distinct from the
//! professional `kicad-cli` render. Pulls its inputs from the algorithm crates
//! (`grid-astar`/`negotiated-mesh`/`pcb-place`) over `pcb-model` types.

pub use grid_astar::router;
pub use negotiated_mesh::{mesh, pathing};
pub use pcb_model as problem;
pub use pcb_place::placement;

pub mod svg;
