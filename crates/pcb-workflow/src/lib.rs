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
//! `sync_board` writes the `.kicad_pcb`: it creates the file when absent and
//! otherwise applies only the schematic delta. Active placement, routing,
//! rendering, and `get_board` read the live IPC board.
//!
//! ## Tool families (one module each)
//!
//! - [`board`] — the guard every board mutator passes through.
//! - [`seed`] — board-construction rule/extra input types.
//! - [`footprints`] — footprint discovery + assignment: `search_footprints`,
//!   `get_footprint_info`.
//! - [`create`] — board synthesis + input parsing: the seed-board writer,
//!   rules and bounds parsing.
//! - [`sync`] — `sync_board`: the schematic↔board netlist diff and its
//!   incremental application.
//! - [`copper`] — copper retraction shared by the board mutators.
//! - [`rules`] — the design rules the board's own footprints permit.
//! - [`diagnose`] — actionable payloads for a refused route.
//! - [`intent`] — board intent (edges, proximity, groups, zones) → placement
//!   constraints. The model states intent; solvers own coordinates.
//! - [`sizing`] — how big a board its own parts require.
//! - [`place`] — `get_board`, IPC snapshot→`PlacementView`, and `place_board`.
//! - [`route`] — `route_board` IPC copper write-back + triage.
//! - [`export`] — `check_board`.
//! - [`fab`] — `export_fab`: bundle a routed board into Gerbers/drill/pos/BOM.
//! - [`render`] — `render_board`.
//! - [`interactive`] — live IPC board editing (`open_board`, `move_parts`,
//!   `route_track`, `delete_copper`, `set_net_width`).

mod board;
mod copper;
pub mod corpus;
mod create;
mod diagnose;
mod export;
mod fab;
mod footprints;
mod intent;
mod interactive;
mod outline;
mod place;
mod render;
mod route;
mod rules;
mod seed;
mod silk;
mod sizing;
mod sync;

pub(crate) struct WorkflowPhase {
    span: tracing::Span,
    started: std::time::Instant,
}

impl WorkflowPhase {
    pub(crate) fn start(phase: &'static str, parts: usize, nets: usize) -> Self {
        Self {
            span: tracing::info_span!(
                "pcb_workflow_phase",
                phase,
                parts,
                nets,
                elapsed_ms = tracing::field::Empty
            ),
            started: std::time::Instant::now(),
        }
    }

    pub(crate) fn facts(
        &self,
        routed: Option<usize>,
        failed: Option<usize>,
        violations: Option<usize>,
    ) {
        tracing::info!(parent: &self.span, routed, failed, violations, "PCB workflow facts");
    }
}

impl Drop for WorkflowPhase {
    fn drop(&mut self) {
        let elapsed_ms = self
            .started
            .elapsed()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        self.span.record("elapsed_ms", elapsed_ms);
        tracing::info!(parent: &self.span, elapsed_ms, "PCB workflow phase finished");
    }
}

pub(crate) fn fmt_num(v: f64) -> String {
    let v = if v == 0.0 { 0.0 } else { v };
    format!("{v}")
}

pub use export::check_board;
pub use fab::export_fab;
pub use footprints::{get_footprint_info, search_footprints};
pub use interactive::{
    delete_copper, move_parts, open_board, route_track, save_session_if_open, set_net_width,
};
pub use outline::update_board_outline;
pub use place::{get_board, place_board};
pub use render::render_board;
pub use route::route_board;
pub use seed::{BoardSeedRules, PourSpec};
pub use sync::sync_board;

fn active_board(
    ctx: &gordian_runtime::AgentRuntime,
) -> Result<kicad_board::IpcBoardSnapshot, String> {
    kicad_board::board_problem(&ctx.pcb_path(), ctx.kicad())
}

fn save_active_board(ctx: &gordian_runtime::AgentRuntime) -> Result<std::path::PathBuf, String> {
    kicad_board::save_live_board(&ctx.pcb_path(), ctx.kicad())
}
