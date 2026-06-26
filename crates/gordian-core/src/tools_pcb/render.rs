//! `render_board` — rasterize the placed or routed board to a PNG for the model
//! and the user.

use anyhow::{Context, Result};
use serde_json::{Value, json};

use pcb_place::placement::PlacementHints;
use pcb_place::placement::{PlaceReport, PlaceResult};

use crate::AgentRuntime;

use super::place::place_problem_from_snapshot;

/// Render the board to a PNG using the placement or routed SVG, save under
/// `.gordian/renders/`, and attach via `IMAGE_PATH_KEY`.
///
/// `view` may be `"placed"` or `"routed"`. When omitted the default is
/// `"routed"` when the live board has copper, `"placed"` otherwise.
pub fn render_board(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    // ── resolve view ─────────────────────────────────────────────────────────
    let board = super::active::board_problem(ctx).ok();
    let has_route = board
        .as_ref()
        .map(|b| !b.copper.traces.is_empty() || !b.copper.vias.is_empty())
        .unwrap_or(false);
    let view_str = input.get("view").and_then(Value::as_str);
    let view = match view_str {
        Some("placed") => "placed",
        Some("routed") => "routed",
        None => {
            if has_route {
                "routed"
            } else {
                "placed"
            }
        }
        Some(other) => {
            return Ok(json!({
                "error": format!(
                    "unknown view `{other}` — pass \"placed\" or \"routed\", or omit for auto"
                ),
            }));
        }
    };

    // ── generate SVG ─────────────────────────────────────────────────────────
    let svg = match view {
        "placed" => {
            let board = match board {
                Some(board) => board,
                None => match super::active::board_problem(ctx) {
                    Ok(board) => board,
                    Err(err) => return Ok(json!({ "error": err })),
                },
            };
            if super::active::is_seed_imported_board(&board.imported) {
                return Ok(json!({
                    "error": "board has not been placed yet — run place_board first, \
                              then render_board",
                }));
            }
            let problem = match place_problem_from_snapshot(&board, ctx) {
                Ok(p) => p,
                Err(msg) => return Ok(json!({ "error": msg })),
            };
            let placements = super::active::imported_placements(&board.imported);
            let result = PlaceResult {
                placements,
                legal: true,
                report: PlaceReport {
                    overlaps_resolved: 0,
                    out_of_bounds_clamps: 0,
                    hpwl: 0.0,
                    layout_cost: 0.0,
                },
            };
            super::engine_svg::render_placement(&problem, &PlacementHints::default(), &result)
        }
        "routed" => {
            let board = match board {
                Some(board) => board,
                None => match super::active::board_problem(ctx) {
                    Ok(board) => board,
                    Err(err) => return Ok(json!({ "error": err })),
                },
            };
            if board.copper.traces.is_empty() && board.copper.vias.is_empty() {
                return Ok(json!({
                    "error": "board has no routed copper yet — run route_board first, then render_board",
                }));
            }
            if board.problem.connections.is_empty() {
                return Ok(json!({
                "error": "board has no routeable nets — derive/place the board first",
                }));
            }
            super::engine_svg::render_svg(&board.problem, &board.copper, &[])
        }
        // The match above is exhaustive over {"placed","routed"}; the `other`
        // arm returned early, so this branch is unreachable.
        _ => unreachable!(),
    };

    // ── rasterize + persist ──────────────────────────────────────────────────
    let png = crate::render::svg_to_png(&svg, ctx.config().tools.render_max_px)?;
    let path = ctx.workspace().next_render_path()?;
    std::fs::write(&path, &png).with_context(|| format!("writing render to {}", path.display()))?;

    let mut obj = json!({
        "ok": true,
        "view": view,
        "png_path": path.display().to_string(),
        "note": format!(
            "Board ({view} view) rendered and attached. \
             Top-layer copper = red, bottom = blue, failed nets = orange crosses. \
             In the placed view, keepout rectangles appear as dark-grey unowned \
             obstacles and part courtyards as grey outlines. \
             PNG also saved to png_path for the user to open."
        ),
    });
    obj[crate::tools::IMAGE_PATH_KEY] = json!(path.display().to_string());
    Ok(obj)
}
