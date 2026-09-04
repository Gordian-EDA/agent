//! `geom` — the leaf math layer shared across the schematic and PCB stacks.
//!
//! Dependency-light 2-D geometry and deterministic support utilities.
//! Core coordinates are millimetres in y-down space.

mod angle;
mod broadphase;
mod consts;
mod grid;
mod hash;
mod ids;
mod point;
mod polygon;
mod polyline;
mod rect;
mod route_shape;
mod segment;
mod shape;
mod union_find;

pub use angle::snap_quadrant;
pub use broadphase::{boxes_meet, candidate_pairs};
pub use consts::{EPS, JOIN_EPS, PAGE_MARGIN, STRICT_EPS};
pub use grid::{GRID_50_MIL, Grid};
pub use hash::{fnv1a, uuid_v5};
pub use ids::stable_uuid;
pub use point::Point2;
pub use polygon::Polygon;
pub use polyline::Polyline;
pub use rect::{BoundaryAxis, Rect, SharedBoundary};
pub use route_shape::{CROSSING_COST, RouteShape, SHAPE_GENERAL, SHAPE_SIMPLE};
pub use segment::Segment;
pub use shape::Dir;
pub use union_find::{ParentForest, UnionFind};
