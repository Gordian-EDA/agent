//! `sch-place` — shared schematic placement vocabulary.
//!
//! This crate owns the layout IR, placeable items, placement SDK, and emit result
//! types. Pure geometry, grid snapping, ids, and disjoint-set helpers live in
//! `geom`; net/part-name classification lives in `circuit-graph::netclass`.

pub mod ir;
pub mod item;
pub mod place;
pub mod result;
