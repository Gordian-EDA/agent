//! PCB application workflows over saved KiCad board files.
//!
//! This crate coordinates board creation, physical design, validation, rendering,
//! and fabrication export. KiCad persistence lives in `kicad-board`; placement
//! and routing implementation selection lives in `pcb-engine`. Recoverable
//! workflow failures are returned as JSON error payloads rather than `Err`.
//!
//! `sync_board` writes the `.kicad_pcb`: it creates the file when absent and
//! otherwise applies only the schematic delta. Placement, routing,
//! rendering, and `get_board` read the saved board.
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
//! - [`locks`] — `lock_parts` / `unlock_parts`: the poses no helper moves.
//! - [`staging`] — the seed row read back as board state: staged, placed, locked.
//! - [`intent`] — board intent (edges, proximity, groups, zones) → placement
//!   constraints. The model states intent; solvers own coordinates.
//! - [`ratsnest`] — the one connectivity shape `get_board` and `route_board`
//!   both answer with: endpoints, status, blocker, escapes.
//! - [`selection`] — `bbox` board-window selection, lowered to the `refs` /
//!   `nets` subsets the local tools take.
//! - [`sizing`] — how big a board its own parts require.
//! - [`place`] — `get_board`, saved snapshot→`PlacementView`, and `place_board`.
//! - [`route`] — `route_board` copper write-back + triage.
//! - [`export`] — `check_board`.
//! - [`fab`] — `export_fab`: bundle a routed board into Gerbers/drill/pos/BOM.
//! - [`render`] — `render_board`.
//! - [`interactive`] — file-backed board editing (`move_parts`, `route_track`,
//!   `delete_copper`, `set_net_width`). Reload the file in KiCad after changes.

// `check_board`'s single JSON payload is wider than the `json!` macro's default
// expansion depth.
#![recursion_limit = "256"]

mod board;
mod copper;
pub mod corpus;
mod create;
mod diagnose;
mod export;
mod fab;
mod footprints;
mod intent;
mod locks;
mod interactive;
mod outline;
mod place;
mod render;
mod route;
mod ratsnest;
mod rules;
mod seed;
mod selection;
mod silk;
mod staging;
mod sizing;
mod sync;

pub(crate) struct WorkflowPhase {
    span: tracing::Span,
    started: std::time::Instant,
}

impl WorkflowPhase {
    pub(crate) fn start(phase: &'static str, parts: usize, nets: usize) -> Self {
        Self {
            span: tracing::info_span!("pcb_workflow_phase", phase, parts, nets),
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
        let elapsed_ms = self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        tracing::info!(parent: &self.span, elapsed_ms, "PCB workflow phase finished");
    }
}

pub(crate) fn fmt_num(v: f64) -> String {
    let v = if v == 0.0 { 0.0 } else { v };
    format!("{v}")
}

pub use export::{check_board, refill_zones};
pub use fab::export_fab;
pub use footprints::{get_footprint_info, search_footprints};
pub use interactive::{delete_copper, move_parts, route_track, set_net_width};
pub use locks::{lock_parts, unlock_parts};
pub use outline::update_board_outline;
pub use place::{get_board, place_board};
pub use render::render_board;
pub use route::route_board;
pub use seed::{BoardSeedRules, PourSpec};
pub use sync::sync_board;

fn active_board(
    ctx: &gordian_runtime::AgentRuntime,
) -> Result<kicad_board::BoardSnapshot, String> {
    kicad_board::read_snapshot(&ctx.pcb_path())
}
