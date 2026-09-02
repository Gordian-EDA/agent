//! `floorplan` — the schematic layout engine, as a pipeline of submodules:
//!
//! - [`infer`] — connectivity → Layout IR (net classification, idiom→IR seeding).
//! - [`idiom`] — circuit-idiom detection, feeding `infer`.
//! - `place` — IR → exact millimetre placement + routing + the `.kicad_sch` writer.
//!
//! All exact geometry is decided in `place`; the LLM that emits the IR never sees a
//! millimetre. The shared vocabulary (geometry, grid, ids, result, and the IR types
//! re-exported below) lives in the `sch-model` crate.
//!
//! `place` is `pub(crate)`: its internals are the LAYOUT TEAM's to rename freely. The
//! pipeline entry points callers run are re-exported by name below; the measurement-based
//! ENGINES never reach in at all: they speak only `sch-model`, and the realization
//! library reaches them through `sch_model::engine::CandidateEvaluator`.

pub use sch_model::ir::{Band, Cell, Flow, LayoutIr, Orient, Side};

mod idiom;
mod infer;
pub mod place;

pub use infer::{baseline_ir, infer_ir};
pub use place::{emit_strategy, place_problem};
