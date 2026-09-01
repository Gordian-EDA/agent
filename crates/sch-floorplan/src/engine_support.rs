//! Lower-level implementation support for schematic placement engines.
//!
//! This module is public because engines are separate crates, but it is not the
//! stable engine contract. Prefer [`crate::contract`] unless implementing an
//! engine that needs the shared geometry or realization primitives.

pub use crate::floorplan::place::{
    COL_GAP, FAST_PINS, GRID_KEY, ROW_GAP, align_idiom_clusters, align_led_chains,
    align_rail_cap_rows, apply_cells, assign_cells, body_overlap_count, build_anchor_blocks,
    build_writer, cluster_group, cohesion_targets, decongest, grid_order_viol, item_rect,
    multi_unit_siblings, normalize, orient_angle, overlaps_any, relation_group_spread, relation_viol,
    repair_relations, signal_anchor_centroid,
    supply_pin_target,
};
pub use sch_io::write::pin_endpoint;
