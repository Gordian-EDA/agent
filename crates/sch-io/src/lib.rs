//! `sch-io` — recover a `Design` view from an emitted `.kicad_sch`.
//!
//! The writing half (the elbow router, the label solver, and the
//! `SchematicWriter`) is the placement pipeline's realiser and lives with it in
//! `sch-floorplan`; only [`read`] remains here.

pub mod read;
