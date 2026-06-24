//! PCB-side tools and the persisted board draft.
//!
//! `tools.rs` stays the schematic file; the PCB tools live here and are merged
//! into [`crate::tools::tool_defs`]/[`run`](crate::tools::run_tool). They follow the same house pattern:
//! [`gordian_core::ToolDef`] JSON schemas, free `fn(input, ctx) -> Result<Value>`
//! handlers, `require_str`-style arg handling, and recoverable failures returned
//! as `{"error": …, "suggestions": …}` values rather than `Err`.
//!
//! ## The board draft
//!
//! Board state follows the DRAFT pattern (the mirror of the schematic
//! `draft.circuit.yaml`): a [`BoardDraft`] persisted as `.autopcb/board.json`.
//! It carries the parts (reference, footprint lib_id, per-pad nets, optional
//! locked position), board bounds, design rules, keepouts, placement hints, and
//! the last placement. Tools mutate the draft; `place_board`/`route_board` read
//! it. The LLM never emits trace coordinates — placement positions enter only via
//! a part `lock`, snapped/legalized by the placer on place.
//!
//! The serde shape reuses `pcb-place` types directly so a draft round-trips
//! straight into a `PlaceProblem` without a translation layer.
//!
//! ## Tool families (one module each)
//!
//! - [`draft`] — the [`BoardDraft`] state model (load/save) + the test-harness
//!   `apply_spec_extras` seeding.
//! - [`footprints`] — footprint discovery + assignment: `search_footprints`,
//!   `get_footprint_info`, `assign_footprint`.
//! - [`create`] — board construction + input parsing: `derive_board`,
//!   `build_board_draft`, rules / bounds / keepout / group parsing.
//! - [`place`] — `get_board`, the draft→`PlaceProblem` bridge, and `place_board`.
//! - [`route`] — `route_board` and the plane/escape routing pipeline + triage.
//! - [`export`] — `export_board`, the synth wiring, and the copper-zone builders.
//! - [`render`] — `render_board`.
//! - [`interactive`] — live IPC board editing (`open_board`, `move_part`,
//!   `route_track`, `set_net_width`, `board_state`) + `autoroute`.

mod create;
mod draft;
mod export;
mod footprints;
mod interactive;
mod place;
mod render;
mod route;

pub use create::{build_board_draft, derive_board};
pub use draft::{apply_spec_extras, BoardDraft, DraftPart, DraftRules, Keepout, PourSpec};
pub use export::export_board;
pub use footprints::{assign_footprint, get_footprint_info, search_footprints};
pub use interactive::{
    autoroute, board_state, move_part, open_board, route_track, save_session_if_open, set_net_width,
};
pub use place::{get_board, place_board};
pub use render::render_board;
pub use route::route_board;
