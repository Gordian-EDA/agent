//! `sch-floorplan` — the engine-agnostic schematic placement core that turns a
//! `circuit_lang::Design` into a real `.kicad_sch` (and back).
//!
//! Owns the [`floorplan`] pipeline: **infer → place → wire → write**. It is
//! engine-agnostic by design — the placement ENGINES (`greedy-place`,
//! `anneal-place`) depend on this core and drive it through the
//! [`sch_place::place`] boundary, never the reverse. The cost a placement engine
//! minimises *is* a routed-sheet score, so the cost/scaffold and the router/writer
//! assembly stay together here.
//!
//! Two surfaces, kept strictly apart:
//! - [`floorplan`] — the pipeline ENTRY POINTS callers run (`infer_ir` / `emit_strategy`
//!   / `emit_writer` / `compose_writers`). The `place` submodule's internals are
//!   `pub(crate)`: a caller cannot reach `floorplan::place::<internal>`.
//! - [`contract`] — THE engine API. The measurement-based engines import ONLY from here;
//!   it re-publishes the realization library + the stable layout geometry they need, so a
//!   `place`-internal rename never touches an engine crate. See the module docs.
//!
//! The shared placement vocabulary lives in `sch-place`; pure geometry and grid
//! snapping live in `geom`; schematic I/O lives in `sch-io`.

pub mod contract;
pub mod floorplan;

pub use sch_io::{label, read, wire, write};
