//! `sch-place-core` — the engine-agnostic schematic placement core that turns a
//! `circuit_lang::Design` into a real `.kicad_sch` (and back).
//!
//! Owns the [`floorplan`] pipeline: **infer → place → wire → write**. It is
//! engine-agnostic by design — the placement ENGINES (`greedy-place`,
//! `anneal-place`) depend on this core and drive it through the
//! [`sch_model::place`] boundary, never the reverse. The cost a placement engine
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
//! The shared vocabulary (geometry, grid, ids, the IR types, the
//! [`sch_model::place`] engine boundary) lives in `sch-model`; the elbow router +
//! text solver + `SchematicWriter` + reader live in `sch-io`. The
//! `crate::{write,wire,label,read,grid,ids}` re-exports below let the `floorplan`
//! module reach those I/O modules through plain `crate::` paths.

pub mod contract;
pub mod floorplan;

// The I/O layer (elbow router + text solver + SchematicWriter + reader) lives in the
// `sch-io` crate; re-exported so `floorplan`'s `crate::wire` / `crate::write` /
// `crate::label` / `crate::read` paths resolve unchanged.
pub use sch_io::{label, read, wire, write};

pub use sch_model::{grid, ids};
