//! `geom` — the leaf math layer shared across the schematic and PCB stacks.
//!
//! Pure, dependency-light helpers with no domain knowledge. The canonical 2-D
//! kernel is [`Point2`] (mm, y-down), [`Rect`], [`Segment`], [`Polyline`], and
//! [`Polygon`], with shared tolerances in [`consts`] and rotated-extent helpers in
//! [`angle`]. Plus cardinal directions ([`shape`]), KiCAD grid snapping
//! ([`grid`]), deterministic content-derived identifiers ([`ids`]), the
//! disjoint-set forest ([`union_find`]), and stable hashing ([`hash`]).
//! Everything here is a pure function of its inputs so callers stay
//! reproducible.

pub mod angle;
pub mod consts;
pub mod grid;
pub mod hash;
pub mod ids;
pub mod point;
pub mod polygon;
pub mod polyline;
pub mod rect;
pub mod segment;
pub mod shape;
pub mod union_find;

pub use angle::{rotated_aabb_half, snap_quadrant};
pub use consts::{EPS, JOIN_EPS, STRICT_EPS};
pub use point::Point2;
pub use polygon::Polygon;
pub use polyline::Polyline;
pub use rect::{BoundaryAxis, Rect, SharedBoundary};
pub use segment::Segment;
pub use shape::Dir;
