//! `contract` — THE engine API. This is the one stable surface a measurement-based
//! placement ENGINE (`anneal-place` today) is allowed to depend on. The
//! engines own their objective (the weights) and their search; this module hands them
//! the neutral [`SchematicPlaceProblem`] (items + connectivity), optional routed
//! realization/evaluation via [`RoutedSheetRealizer`] and [`RoutedEvaluator`],
//! caller-supplied layout intent via the trait method, plus the engine-independent
//! layout geometry both need — and NOTHING ELSE.
//!
//! The golden rule this enforces: the layout team owns every module under
//! [`crate::floorplan::place`] and may rename its internals freely. The engines never
//! reach into those internals — they import only the names re-published here, so an
//! internal rename touches at most ONE line in this module, never an engine crate. To
//! rename a contract symbol the layout team must edit this file (the seam), which is the
//! deliberate, visible cost of changing a published API.
//!
//! The surface, in three groups:
//!
//! - **Placement problem + optional routed realization/evaluation** —
//!   [`SchematicPlaceProblem`], [`RoutedSheetRealizer`], [`RoutedEvaluator`],
//!   [`RawMetrics`], and the [`PlacementEngine`] trait.
//! - **Pad geometry** — [`pin_endpoint`] (a placed pin's world position), the input every
//!   geometric proxy needs to reason about wiring without re-routing.
//! - **Layout geometry / scaffold** — the engine-independent realization helpers both
//!   engines need to propose and finalize a candidate: the writer builder
//!   [`build_writer`], the body-rect primitive [`item_rect`], [`overlaps_any`],
//!   [`body_overlap_count`], the orientation map [`orient_angle`], the
//!   anchor-pin targets [`signal_anchor_centroid`] / [`supply_pin_target`], the
//!   authored-order check [`grid_order_viol`], the overlap relaxer [`decongest`], the idiom
//!   re-seating passes ([`align_idiom_clusters`] / [`align_led_chains`]) and the
//!   cluster-block helpers ([`build_anchor_blocks`] / [`cluster_group`] /
//!   [`cohesion_targets`] / [`multi_unit_siblings`]), plus the grid constants
//!   [`COL_GAP`] / [`ROW_GAP`] / [`GRID_KEY`] / [`FAST_PINS`].

pub use crate::floorplan::place::{
    COL_GAP, FAST_PINS, GRID_KEY, PlacementEngine, PlacementOutput, ROW_GAP, RawMetrics,
    RouteRealization, RoutedEvaluator, RoutedSheetRealizer, SchematicPlaceProblem,
    align_idiom_clusters, align_led_chains, align_rail_cap_rows, apply_cells, assign_cells,
    body_overlap_count, build_anchor_blocks, build_writer, cluster_group, cohesion_targets,
    decongest, grid_order_viol, item_rect, multi_unit_siblings, normalize, orient_angle,
    overlaps_any, signal_anchor_centroid, supply_pin_target,
};

pub use crate::floorplan::infer_ir;
pub use kicad_env::KicadEnv;
pub use sch_io::write::pin_endpoint;
