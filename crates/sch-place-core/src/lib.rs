//! `sch-place-core` — the engine-agnostic placement core, extracted from
//! `sch-layout` so the placement ENGINES (`greedy-place`, `anneal-place`) can drive
//! it without depending on the layout crate itself (which only orchestrates this
//! core and round-trips `.kicad_sch`).
//!
//! Owns the [`floorplan`] pipeline: **infer → place → wire → write**. The cost a
//! placement engine minimises *is* a routed-sheet score, so the cost/scaffold and
//! the router/writer assembly stay together here. The shared vocabulary (geometry,
//! grid, ids, the IR types, the [`sch_model::place`] engine boundary) lives in
//! `sch-model`; the elbow router + text solver + `SchematicWriter` live in `sch-io`.
//!
//! The `crate::{write,wire,label,read,grid,ids}` re-exports below preserve the paths
//! the moved `floorplan` module used inside `sch-layout`, so its source is verbatim.

pub mod floorplan;

// The I/O layer (elbow router + text solver + SchematicWriter + reader) lives in the
// `sch-io` crate; re-exported so `floorplan`'s `crate::wire` / `crate::write` /
// `crate::label` / `crate::read` paths resolve unchanged.
pub use sch_io::{label, read, wire, write};

pub use sch_model::{grid, ids};
