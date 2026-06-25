//! `geom` — the leaf math layer shared across the schematic and PCB stacks.
//!
//! Pure, dependency-light helpers with no domain knowledge: 2D direction +
//! axis-aligned segment math ([`shape`]), KiCAD grid snapping ([`grid`]),
//! deterministic content-derived identifiers ([`ids`]), the disjoint-set forest
//! ([`union_find`]), and stable hashing ([`hash`]). Everything here is a pure
//! function of its inputs so callers stay reproducible.

pub mod grid;
pub mod hash;
pub mod ids;
pub mod shape;
pub mod union_find;
