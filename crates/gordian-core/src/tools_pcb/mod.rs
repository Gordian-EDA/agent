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
//! [`BoardSeed`] is only the create/derive input model used to bootstrap a board.
//! Active placement, routing, rendering, and `get_board` read the live IPC board.
//!
//! ## Tool families (one module each)
//!
//! - [`active`] — live-session snapshot/save helpers.
//! - [`seed`] — create/derive [`BoardSeed`] input types.
//! - [`footprints`] — footprint discovery + assignment: `search_footprints`,
//!   `get_footprint_info`, `assign_footprint`.
//! - [`create`] — board construction + input parsing: `derive_board`,
//!   `build_seed_board`, rules / bounds / keepout / group parsing.
//! - [`place`] — `get_board`, IPC snapshot→`PlaceProblem`, and `place_board`.
//! - [`route`] — `route_board` IPC copper write-back + triage.
//! - [`export`] — `check_board`.
//! - [`fab`] — `export_fab`: bundle a routed board into Gerbers/drill/pos/BOM.
//! - [`render`] — `render_board`.
//! - [`engine_svg`] — diagnostic SVG of the engine's own view (placement/routed),
//!   the fast in-loop alternative to the `kicad-cli` production render.
//! - [`interactive`] — live IPC board editing (`open_board`, `move_part`,
//!   `route_track`, `set_net_width`, `board_state`). `autoroute` is disabled
//!   until Freerouting is reconnected to IPC.

mod active;
mod create;
pub mod engine_svg;
mod export;
mod fab;
mod footprints;
mod interactive;
mod place;
mod render;
mod route;
mod seed;

pub(super) fn fmt_num(v: f64) -> String {
    let v = if v == 0.0 { 0.0 } else { v };
    format!("{v}")
}

pub use create::{build_seed_board, derive_board};
pub use export::check_board;
pub use fab::export_fab;
pub use footprints::{assign_footprint, get_footprint_info, search_footprints};
pub use interactive::{
    autoroute, board_state, move_part, open_board, route_track, save_session_if_open, set_net_width,
};
pub use place::{get_board, place_board};
pub use render::render_board;
pub use route::route_board;
pub use seed::{BoardSeed, BoardSeedPart, BoardSeedRules, Keepout, PourSpec, apply_seed_extras};
