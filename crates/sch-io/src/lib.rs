//! `sch-io` — the schematic I/O layer:
//!
//! - [`wire`] — the elbow (Manhattan) router.
//! - [`label`] — the text-placement collision solver.
//! - [`write`] — the `SchematicWriter`: placed symbols + routed wires → `.kicad_sch`.
//! - [`read`] — recover a `Design` view from an emitted `.kicad_sch`.
//!
//! `wire` and `write` are mutually coupled (the writer hands the router a `RouteScene`;
//! the router fills the writer), so they share one crate rather than a forced split.
//! Geometry/grid/ids vocabulary comes from `sch-model` (re-exported for the modules'
//! `crate::grid` / `crate::ids` paths).

pub use sch_model::{grid, ids};

pub mod label;
pub mod read;
pub mod wire;
pub mod write;
