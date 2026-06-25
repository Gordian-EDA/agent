//! PCB-side tools over the active KiCAD board.
//!
//! `tools.rs` stays the schematic file; the PCB tools live here and are merged
//! into [`crate::tools::tool_defs`]/[`run`](crate::tools::run_tool). They follow the same house pattern:
//! genai [`crate::Tool`] JSON schemas, free `fn(input, ctx) -> Result<Value>`
//! handlers, `require_str`-style arg handling, and recoverable failures returned
//! as `{"error": …, "suggestions": …}` values rather than `Err`.
//!
//! ## Active board
//!
//! The live KiCAD IPC session is the source of truth for PCB state. Tools ask the
//! global session manager for the project board, save it when a parser/importer
//! needs the persisted `.kicad_pcb`, and write geometry back through IPC.
//!
//! [`BoardDraft`] remains as a transient adapter for existing placement/routing
//! code; it is no longer the persisted PCB state.
//!
//! ## Tool families (one module each)
//!
//! - [`active`] — live-session save/import helpers for placement, routing, and rendering.
//! - [`draft`] — transient [`BoardDraft`] adapter types.
//! - [`footprints`] — footprint discovery + assignment: `search_footprints`,
//!   `get_footprint_info`, `assign_footprint`.
//! - [`create`] — board construction + input parsing: `derive_board`,
//!   `build_board_draft`, rules / bounds / keepout / group parsing.
//! - [`place`] — `get_board`, the draft→`PlaceProblem` bridge, and `place_board`.
//! - [`route`] — `route_board` IPC copper write-back + triage.
//! - [`export`] — `check_board`.
//! - [`fab`] — `export_fab`: bundle a routed board into Gerbers/drill/pos/BOM.
//! - [`render`] — `render_board`.
//! - [`engine_svg`] — diagnostic SVG of the engine's own view (placement/routed),
//!   the fast in-loop alternative to the `kicad-cli` production render.
//! - [`interactive`] — live IPC board editing (`open_board`, `move_part`,
//!   `route_track`, `set_net_width`, `board_state`) + `autoroute`.

mod active;
mod create;
mod draft;
pub mod engine_svg;
mod export;
mod fab;
mod footprints;
mod interactive;
mod place;
mod render;
mod route;

pub use create::{build_board_draft, derive_board};
pub use draft::{BoardDraft, DraftPart, DraftRules, Keepout, PourSpec, apply_spec_extras};
pub use export::check_board;
pub use fab::export_fab;
pub use footprints::{assign_footprint, get_footprint_info, search_footprints};
pub use interactive::{
    autoroute, board_state, move_part, open_board, route_track, save_session_if_open, set_net_width,
};
pub use place::{get_board, place_board};
pub use render::render_board;
pub use route::route_board;
