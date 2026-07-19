//! `spine-place` — deterministic grammar-typesetting schematic placement.
//!
//! Parses the netlist into net classes ([`net`]), contracts series 2-pin runs
//! into chains over a reduced node graph ([`chain`]), then typesets modules and
//! spines with corpus-mined human conventions. No stochastic search.

mod bands;
pub mod chain;
mod compact;
mod engine;
mod module;
pub mod net;
mod order;
mod scene;

pub use engine::SpinePlace;

/// The 50 mil schematic grid, shared by every typesetting stage.
pub(crate) const GRID: f64 = geom::GRID_50_MIL.pitch();

/// Snap onto [`GRID`].
pub(crate) fn snap(v: f64) -> f64 {
    geom::GRID_50_MIL.snap(v)
}
