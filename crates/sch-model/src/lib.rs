//! `sch-model` — the schematic layout MODEL: every type and contract the placement,
//! routing and text-solving algorithms speak, and nothing that implements one.
//!
//! Each algorithm is a LEAF with a defined input and a defined output, so an author can
//! work on it without the rest of the system:
//!
//! | leaf | trait | input → output |
//! |---|---|---|
//! | typesetter (`sch-flex`) | — | [`tree::Tree`] + [`item::Item`]s → poses |
//! | wire router | [`route::SchRouter`] | [`route::RouteScene`] + terminals → paths |
//! | text solver | [`text::TextSolver`] | [`text::Obstacle`]s + [`text::Movable`]s → [`text::Pick`]s |
//!
//! The heavy collaborators (sheet realization, the symbol library, the `.kicad_sch`
//! writer) are INJECTED as trait objects by the composition root, `sch-floorplan`, which
//! also implements them. No leaf depends on it.
//!
//! Everything else here is the shared vocabulary the leaves compute over: the layout tree
//! and IR, placeable items, and placement geometry. Pure geometry, grid snapping, ids and
//! disjoint-set helpers live in `geom`; net/part-name classification lives in
//! `circuit-graph::netclass`.

pub mod engine;
pub mod geometry;
pub mod ir;
pub mod item;
pub mod place;
pub mod result;
pub mod route;
pub mod text;
pub mod tree;
