//! PCB-side tools over the active KiCAD board.
//!
//! `tools.rs` stays the schematic file; the PCB tools live here and are merged
//! into [`crate::tools::tool_defs`]/[`run`](crate::tools::run_tool). Recoverable
//! tool failures are returned as JSON error payloads rather than `Err`.
//!
//! ## Active board
//!
//! The live KiCAD IPC session is the source of truth for PCB state. Tools ask the
//! global session manager for the project board, save it for CLI checks/exports,
//! and write geometry back through IPC.
//!
//! `regenerate_board` synthesizes the initial `.kicad_pcb` file. Active
//! placement, routing, rendering, and `get_board` read the live IPC board.
//!
//! ## Tool families (one module each)
//!
//! - [`active`] — live-session snapshot/save helpers.
//! - [`seed`] — board-construction rule/extra input types.
//! - [`footprints`] — footprint discovery + assignment: `search_footprints`,
//!   `get_footprint_info`, `assign_footprints`.
//! - [`create`] — board construction + input parsing: `regenerate_board`,
//!   rules and bounds parsing.
//! - [`place`] — `get_board`, IPC snapshot→`PlaceProblem`, and `place_board`.
//! - [`route`] — `route_board` IPC copper write-back + triage.
//! - [`export`] — `check_board`.
//! - [`fab`] — `export_fab`: bundle a routed board into Gerbers/drill/pos/BOM.
//! - [`render`] — `render_board`.
//! - [`interactive`] — live IPC board editing (`open_board`, `move_parts`,
//!   `route_track`, `set_net_width`).

mod active;
mod create;
mod export;
mod fab;
mod footprints;
mod interactive;
mod outline;
mod place;
mod render;
mod route;
mod seed;

pub(super) fn fmt_num(v: f64) -> String {
    let v = if v == 0.0 { 0.0 } else { v };
    format!("{v}")
}

pub use create::regenerate_board;
pub use export::check_board;
pub use fab::export_fab;
pub use footprints::{assign_footprints, get_footprint_info, search_footprints};
pub use interactive::{move_parts, open_board, route_track, save_session_if_open, set_net_width};
pub use outline::update_board_outline;
pub use place::{get_board, place_board};
pub use render::render_board;
pub use route::route_board;
pub use seed::{BoardSeedRules, PourSpec};
