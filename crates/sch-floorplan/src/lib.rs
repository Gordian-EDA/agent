//! `sch-floorplan` — the engine-agnostic schematic placement core that turns a
//! `circuit_lang::Design` into a real `.kicad_sch` (and back).
//!
//! Owns the [`floorplan`] pipeline: **infer → place → wire → write**. It is
//! engine-agnostic by design — placement engines (`anneal-place` today)
//! depend on this core and drive it through the
//! [`contract`] boundary, never the reverse. The cost a placement engine
//! minimises *is* a routed-sheet score, so the cost/scaffold and the router/writer
//! assembly stay together here.
//!
//! Two surfaces, kept strictly apart:
//! - [`floorplan`] — the pipeline ENTRY POINTS callers run (`infer_ir` / `emit_strategy`
//!   / `emit_writer` / `compose_writers`). The `place` submodule's internals are
//!   `pub(crate)`: a caller cannot reach `floorplan::place::<internal>`.
//! - [`contract`] — the small stable engine API.
//! - [`engine_support`] — lower-level geometry and realization helpers for engine
//!   implementations; public because engines live in separate crates.
//! - [`region`] — place a SUBSET of a sheet among fixed neighbours and obstacles, for
//!   live editing (`arrange(selection)`) and bulk part creation.
//!
//! The shared placement vocabulary lives in `sch-place`; pure geometry and grid
//! snapping live in `geom`; schematic I/O lives in `sch-io`.

pub mod contract;
pub mod engine_support;
pub mod floorplan;
pub mod region;

pub use sch_io::{label, read, wire, write};
