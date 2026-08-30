//! PCB application workflows over the active KiCad board.
//!
//! This crate coordinates board creation, physical design, validation, rendering,
//! and fabrication export. KiCad persistence lives in `kicad-board`; placement
//! and routing implementation selection lives in `pcb-engine`. Recoverable
//! workflow failures are returned as JSON error payloads rather than `Err`.
//!
//! ## Active board
//!
//! The saved `.kicad_pcb` is the durable board state. When pcbnew is open, tools
//! snapshot and save it through IPC; headless operations use `kicad-board`'s
//! atomic file-edit fallback and invalidate any stale live session.
//!
//! `regenerate_board` synthesizes the initial `.kicad_pcb` file. Active
//! placement, routing, rendering, and `get_board` read the live IPC board.
//!
//! ## Tool families (one module each)
//!
//! - [`seed`] — board-construction rule/extra input types.
//! - [`footprints`] — footprint discovery + assignment: `search_footprints`,
//!   `get_footprint_info`, `assign_footprints`.
//! - [`create`] — board construction + input parsing: `regenerate_board`,
//!   rules and bounds parsing.
//! - [`place`] — `get_board`, IPC snapshot→`PlacementView`, and `place_board`.
//! - [`route`] — `route_board` IPC copper write-back + triage.
//! - [`export`] — `check_board`.
//! - [`fab`] — `export_fab`: bundle a routed board into Gerbers/drill/pos/BOM.
//! - [`render`] — `render_board`.
//! - [`interactive`] — live IPC board editing (`open_board`, `move_parts`,
//!   `route_track`, `delete_copper`, `set_net_width`).

pub mod corpus;
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
mod silk;

pub(crate) fn fmt_num(v: f64) -> String {
    let v = if v == 0.0 { 0.0 } else { v };
    format!("{v}")
}

pub use create::regenerate_board;
pub use export::check_board;
pub use fab::export_fab;
pub use footprints::{
    assign_footprints, common_default_footprint, get_footprint_info, search_footprints,
};
pub use footprints::{patch_footprint, patch_part_and_footprint};
pub use interactive::{
    delete_copper, move_parts, open_board, route_track, save_session_if_open, set_net_width,
};
pub use outline::update_board_outline;
pub use place::{get_board, place_board};
pub use render::render_board;
pub use route::route_board;
pub use seed::{BoardSeedRules, PourSpec};

fn active_board(
    ctx: &gordian_runtime::AgentRuntime,
) -> Result<kicad_board::IpcBoardSnapshot, String> {
    kicad_board::board_problem(&ctx.pcb_path(), ctx.kicad())
}

fn save_active_board(ctx: &gordian_runtime::AgentRuntime) -> Result<std::path::PathBuf, String> {
    kicad_board::save_live_board(&ctx.pcb_path(), ctx.kicad())
}
