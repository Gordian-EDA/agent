//! Stable schematic placement-engine contract.
//!
//! Engines receive a neutral [`SchematicPlaceProblem`], may evaluate routed
//! candidates through [`RoutedEvaluator`], and return [`PlacementOutput`].
//! Geometry and realization helpers used to implement an engine live in the
//! explicitly lower-level [`crate::engine_support`] module.

pub use crate::floorplan::place::{
    PlacementEngine, PlacementOutput, RawMetrics, RouteRealization, RoutedEvaluator,
    RoutedSheetRealizer, SchematicPlaceProblem,
};
