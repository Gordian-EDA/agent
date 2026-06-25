//! `render_board` — rasterize the placed or routed board to a PNG for the model
//! and the user.

use anyhow::{Context, Result};
use serde_json::{Value, json};

use pcb_place::placement::{PlaceReport, PlaceResult};

use crate::tools::PcbToolCtx;

use super::place::place_problem_from_draft;

/// Render the board to a PNG using the placement or routed SVG, save under
/// `.gordian/renders/`, and attach via `IMAGE_PATH_KEY`.
///
/// `view` may be `"placed"` or `"routed"`. When omitted the default is
/// `"routed"` when the live board has copper, `"placed"` otherwise.
pub fn render_board(input: Value, ctx: &PcbToolCtx) -> Result<Value> {
    let draft = match super::active::draft_from_live(ctx) {
        Ok(draft) => draft,
        Err(err) => return Ok(json!({ "error": err })),
    };

    // ── resolve view ─────────────────────────────────────────────────────────
    let has_route = super::active::copper_solution(ctx)
        .map(|s| !s.traces.is_empty() || !s.vias.is_empty())
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
            // Need a last_placement in the draft.
            let Some(placements) = draft.last_placement.clone() else {
                return Ok(json!({
                    "error": "board has not been placed yet — run place_board first, \
                              then render_board",
                }));
            };
            let problem = match place_problem_from_draft(&draft, ctx) {
                Ok(p) => p,
                Err(msg) => return Ok(json!({ "error": msg })),
            };
            // Reconstruct a minimal PlaceResult from the stored placements.
            // render_placement uses .placements to look up part positions, and
            // problem.parts for courtyard/pad geometry. legal/report are not
            // used by the renderer — any zero-default values are fine.
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
            super::engine_svg::render_placement(&problem, &draft.hints, &result)
        }
        "routed" => {
            let solution = match super::active::copper_solution(ctx) {
                Ok(solution) if !solution.traces.is_empty() || !solution.vias.is_empty() => {
                    solution
                }
                Ok(_) => {
                    return Ok(json!({
                        "error": "board has no routed copper yet — run route_board first, then render_board",
                    }));
                }
                Err(err) => {
                    return Ok(json!({ "error": err }));
                }
            };
            let board = match super::active::board_problem(ctx) {
                Ok(board) => board,
                Err(err) => {
                    return Ok(json!({ "error": err }));
                }
            };
            if board.problem.connections.is_empty() {
                return Ok(json!({
                    "error": "board has no routeable nets — derive/place the board first",
                }));
            }
            super::engine_svg::render_svg(&board.problem, &solution, &[])
        }
        // The match above is exhaustive over {"placed","routed"}; the `other`
        // arm returned early, so this branch is unreachable.
        _ => unreachable!(),
    };

    // ── rasterize + persist ──────────────────────────────────────────────────
    let png = crate::render::svg_to_png(&svg, crate::tools::RENDER_MAX_PX)?;
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
