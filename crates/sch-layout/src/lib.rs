//! `sch-layout` — the modern floorplan layout engine.
//!
//! Extracted from `sch-engine` so the active `floorplan` layout path lives apart
//! from the legacy reconcile/grammar/place path. This crate owns:
//!
//! - [`floorplan`] — the cost-scored placement + routing engine and its IR.
//! - [`emit`] — the `SchematicWriter` that renders placements to `.kicad_sch`.
//! - [`route`] / [`textplace`] — wire routing and label/text geometry.
//! - [`grid`] — snapping coordinates onto KiCAD's 1.27 mm schematic grid.
//! - [`ids`] — content-derived (UUIDv5) identifiers for byte-stable output.
//! - [`cluster_geom`] / [`grammar`] — closed-form cluster geometry and the
//!   grammar shapes it consumes (used by the legacy placer in `sch-engine`).
//! - [`lift`] — recovering a `Design` view from an emitted schematic.
//!
//! The shared emission types [`EmitOutput`] / [`Relayout`] and the `ap_*`
//! identity-property keys live in [`output`] and are re-exported here.

pub mod cluster_geom;
pub mod emit;
pub mod floorplan;
pub mod grammar;
pub mod grid;
pub mod ids;
pub mod lift;
mod output;
mod route;
mod textplace;

pub use output::{
    AP_BLOCK, AP_INDEX, AP_LAYOUT_REV, AP_PARENT, AP_ROLE, EmitOutput, ROLE_AUTHORED, Relayout,
};

// The legacy reconcile path (in `sch-engine`) routes local nets with the same
// primitives the modern engine uses. `route` stays a private module; its
// cross-crate surface is re-exported here so reconcile can drive it.
pub use route::{Path, RouteScene, junction_points, mst_edges, route_edge};
