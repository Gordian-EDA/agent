//! `sch-model` — the shared schematic vocabulary the placement engine (`sch-floorplan`) builds
//! on: the layout `ir`, placeable `item`s, the placement SDK (`place`), the emit
//! `result` types, and `netclass`. No engine logic; just the types every stage
//! (infer → place → wire → write) speaks. Pure math (geometry, grid, ids,
//! disjoint-set) lives in the leaf [`geom`] crate and is re-exported here so the
//! historical `sch_model::{geom, grid, ids, union_find}` paths keep resolving.

pub use ::geom::{grid, ids, shape as geom, union_find};

pub mod ir;
pub mod item;
pub mod netclass;
pub mod place;
pub mod result;
