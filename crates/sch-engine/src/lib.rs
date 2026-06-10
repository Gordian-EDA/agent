//! `sch-engine` — turns a [`circuit_lang::Design`] into a real `.kicad_sch`
//! file (and back), deterministically.
//!
//! This crate is built incrementally per the sch-engine plan. Task 2
//! establishes the deterministic primitives every later stage relies on:
//!
//! - [`ids`] — content-derived (UUIDv5) identifiers so re-emitting the same
//!   `Design` yields byte-identical output (spec §5.1).
//! - [`grid`] — snapping coordinates onto KiCAD's 1.27 mm schematic grid.

pub mod emit;
pub mod grid;
pub mod ids;
