//! Deterministic PCB auto-layout.
//!
//! [`board_from_schematic`] stages a board from a schematic, [`auto_layout`] sizes an outline,
//! seats the connectors, places the rest by connectivity, pours ground, routes with Freerouting
//! and asks KiCad's DRC whether the result is legal, and [`render`] plots what came out.
//! Every step is deterministic and the whole call keeps a wall-clock budget.

pub mod checks;
pub mod dsn;
pub mod fab;
pub mod freerouting;
pub mod fanout;
pub mod geom;
pub mod model;
pub mod pipeline;
pub mod place;
pub mod project;
pub mod render;
pub mod repair;
pub mod rules;
pub mod schematic;
pub mod ses;
pub mod sexp;
pub mod silk;
pub mod stitch;
pub mod tidy;

pub use checks::{check, CheckReport};
pub use fab::export_fab;
pub use model::Board;
pub use pipeline::{auto_layout, AutoOptions, AutoReport, Outline};
pub use render::{render, Side};
pub use schematic::board_from_schematic;
