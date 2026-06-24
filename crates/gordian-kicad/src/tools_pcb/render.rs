//! `render_board` — rasterize the placed or routed board to a PNG for the model
//! and the user.

use anyhow::{Context, Result};
use serde_json::{Value, json};

use pcb_place::placement::{PlaceReport, PlaceResult, to_route_problem};

use crate::tools::PcbToolCtx;

use super::draft::BoardDraft;
use super::place::place_problem_from_draft;
use super::route::{StoredRoute, inject_keepouts};

/// Render the board to a PNG using the placement or routed SVG, save under
/// `.autopcb/renders/`, and attach via `IMAGE_PATH_KEY`.
///
/// `view` may be `"placed"` or `"routed"`. When omitted the default is
/// `"routed"` when `route.json` exists, `"placed"` otherwise.
pub fn render_board(input: Value, ctx: &PcbToolCtx) -> Result<Value> {
    // ── load draft ───────────────────────────────────────────────────────────
    let Some(draft) = BoardDraft::load(ctx) else {
        return Ok(json!({
            "error": "no board draft yet — run derive_board first",
        }));
    };

    // ── resolve view ─────────────────────────────────────────────────────────
    let has_route = ctx.workspace().read_route().is_some();
    let view_str = input.get("view").and_then(Value::as_str);
    let view = match view_str {
        Some("placed") => "placed",
        Some("routed") => "routed",
        None => {
            if has_route { "routed" } else { "placed" }
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
            // Need route.json.
            let Some(raw) = ctx.workspace().read_route() else {
                return Ok(json!({
                    "error": "board has not been routed yet — run route_board first, \
                              then render_board",
                }));
            };
            // Need placements too (to rebuild the RouteProblem).
            let Some(placements) = draft.last_placement.clone() else {
                return Ok(json!({
                    "error": "board has no placement in the draft — run place_board \
                              then route_board before rendering the routed view",
                }));
            };
            let stored: StoredRoute = match serde_json::from_str(&raw) {
                Ok(s) => s,
                Err(e) => {
                    return Ok(json!({
                        "error": format!("route.json is corrupt or schema-mismatch: {e}"),
                    }));
                }
            };
            let problem = match place_problem_from_draft(&draft, ctx) {
                Ok(p) => p,
                Err(msg) => return Ok(json!({ "error": msg })),
            };
            let mut rp = to_route_problem(&problem, &placements);
            inject_keepouts(&mut rp, &draft.keepouts);
            super::engine_svg::render_svg(&rp, &stored.solution, &stored.failed)
        }
        // The match above is exhaustive over {"placed","routed"}; the `other`
        // arm returned early, so this branch is unreachable.
        _ => unreachable!(),
    };

    // ── rasterize + persist ──────────────────────────────────────────────────
    let png = crate::render::svg_to_png(&svg, crate::tools::RENDER_MAX_PX)?;
    let path = ctx.workspace().next_render_path()?;
    std::fs::write(&path, &png)
        .with_context(|| format!("writing render to {}", path.display()))?;

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
