//! `sch-place` — shared schematic placement vocabulary.
//!
//! This crate owns the layout IR, placeable items, placement SDK, emit result
//! types, and netclass helpers. Pure geometry, grid snapping, ids, and
//! disjoint-set helpers live in `geom`.

pub mod ir;
pub mod item;
pub mod netclass;
pub mod place;
pub mod result;
