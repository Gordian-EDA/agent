//! `floorplan` — the schematic layout engine, as a pipeline of submodules:
//!
//! - [`infer`] — connectivity → Layout IR (net classification, idiom→IR seeding).
//! - [`idiom`] — circuit-idiom detection, feeding `infer`.
//! - `place` — IR → exact millimetre placement + routing + the `.kicad_sch` writer.
//!
//! All exact geometry is decided in `place`; the LLM that emits the IR never sees a
//! millimetre. The shared vocabulary (geometry, grid, ids, result, and the IR types
//! re-exported below) lives in the `sch-place` crate.
//!
//! `place` is `pub(crate)`: its internals are the LAYOUT TEAM's to rename freely. The
//! pipeline entry points callers run are re-exported by name below; the measurement-based
//! ENGINES reach the realization library + layout geometry only through
//! [`crate::contract`], never `floorplan::place::<internal>`.

pub use sch_place::ir::{Band, Cell, Flow, LayoutIr, Orient, Side};

mod idiom;
mod infer;
pub(crate) mod place;

pub use infer::{baseline_ir, infer_ir};
pub use place::{SchematicPlaceProblem, compose_writers, emit_group, emit_strategy, emit_writer};
