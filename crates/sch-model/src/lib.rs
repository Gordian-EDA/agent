//! `sch-model` — the shared schematic vocabulary the engine (`sch-layout`) builds
//! on: geometry primitives (`geom`), KiCAD `grid` snapping, deterministic `ids`,
//! and the emit `result` types. No engine logic; just the types and pure helpers
//! that every stage (infer → place → wire → write) speaks.

pub mod geom;
pub mod grid;
pub mod ids;
pub mod ir;
pub mod item;
pub mod netclass;
pub mod place;
pub mod result;
pub mod union_find;
