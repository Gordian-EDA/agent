//! `floorplan` — the schematic layout pipeline, as two submodules:
//!
//! - [`infer`] — connectivity → Layout IR (net classification, rail/port inference).
//! - `place` — IR → `sch_flex::typeset` → exact millimetre placement, routing, and the
//!   `.kicad_sch` writer.
//!
//! All exact geometry is decided in `place`; the LLM that emits the IR never sees a
//! millimetre. The shared vocabulary (geometry, grid, ids, result, and the IR types
//! re-exported below) lives in the `sch-model` crate.
//!
//! `place` is `pub`: its measurement library ([`place::RoutedEvaluator`] and the
//! truthfulness counts behind it) is what [`crate::region::arrange`] and the live-edit
//! surface gate a placement on. The pipeline entry points callers run are re-exported by
//! name below.

pub use sch_model::ir::{Band, LayoutIr, Side};

mod infer;
pub mod place;

pub use infer::{apply_intent, baseline_ir, infer_ir};
pub use place::{emit_strategy, place_problem};
