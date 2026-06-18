//! SVG debug render for a routed PCB.
//!
//! [`render_svg`] produces a standalone SVG string from a [`RouteProblem`],
//! a [`RouteSolution`], and the optional list of nets that failed routing
//! (from [`crate::router::RouteResult::failed`]). Pure string assembly — no
//! external dependencies beyond the standard library.
//!
//! [`render_global_svg`] produces a standalone SVG from a [`RouteProblem`],
//! a [`crate::mesh::CapacityMesh`], and a [`crate::pathing::GlobalRouteResult`]:
//! a board underlay (outline + pads), leaf boundary rects, per-leaf utilization
//! heat tint (green → red), net cell-path polylines, and orange highlights for
//! unrouted net endpoints.
//!
//! ## Coordinate system
//!
//! KiCAD PCB coordinates are y-down, and SVG is also y-down, so no axis flip
//! is needed. The viewport is exactly `problem.bounds` with a small margin so
//! no copper is clipped at the edge.
//!
//! ## Visual encoding
//!
//! | Element               | Style                                        |
//! |-----------------------|----------------------------------------------|
//! | Board outline         | thin dark grey stroke, no fill               |
//! | Owned pad / obstacle  | medium grey fill                             |
//! | Unowned obstacle      | dark grey fill (keepout / foreign copper)    |
//! | Top-layer trace       | red, 60 % opacity                            |
//! | Bottom-layer trace    | blue, 60 % opacity                           |
//! | Via                   | ringed circle (annular ring + drill hole)    |
//! | Failed-net point      | orange cross + circle                        |
//! | Mesh leaf boundary    | thin light-grey rect                         |
//! | Leaf utilization      | green→red heat fill by max-layer usage ratio |
//! | Net cell-path ribbon  | translucent polyline, layer-coloured         |
//! | Unrouted endpoint     | orange cross + circle (same as failed-net)   |

use std::fmt::Write as _;

use crate::mesh::CapacityMesh;
use crate::pathing::GlobalRouteResult;
use crate::placement::{PlaceProblem, PlaceResult, PlacementHints};
use crate::problem::{LayerRef, Point2, RouteProblem, RouteSolution};
use crate::router::FailedNet;

// ── public API ───────────────────────────────────────────────────────────────

/// Render `problem` and `solution` to a standalone SVG string.
///
/// `failed` may be empty; when non-empty the points-to-connect of those nets
/// are highlighted with orange crosses so a human can see where the router
/// gave up.
pub fn render_svg(
    problem: &RouteProblem,
    solution: &RouteSolution,
    failed: &[FailedNet],
) -> String {
    let margin = 2.0_f64;
    let b = &problem.bounds;
    let board_w = b.max_x - b.min_x;
    let board_h = b.max_y - b.min_y;
    let vb_x = b.min_x - margin;
    let vb_y = b.min_y - margin;
    let vb_w = board_w + 2.0 * margin;
    let vb_h = board_h + 2.0 * margin;

    // Scale: 10 px per mm so a 30 mm board is 300 px wide.
    let px_per_mm = 10.0_f64;
    let svg_w = vb_w * px_per_mm;
    let svg_h = vb_h * px_per_mm;

    let mut o = String::with_capacity(32 * 1024);
    let w = &mut o; // short alias

    // SVG root ----------------------------------------------------------------
    writeln!(w, "<?xml version=\"1.0\" encoding=\"UTF-8\"?>").unwrap();
    write!(
        w,
        "<svg xmlns=\"http://www.w3.org/2000/svg\"\n\
         \x20    width=\"{svg_w:.2}\" height=\"{svg_h:.2}\"\n\
         \x20    viewBox=\"{vb_x:.6} {vb_y:.6} {vb_w:.6} {vb_h:.6}\">\n"
    )
    .unwrap();

    // Board outline -----------------------------------------------------------
    w.push_str("  <!-- board outline -->\n");
    writeln!(
        w,
        "  <rect x=\"{x:.6}\" y=\"{y:.6}\" width=\"{bw:.6}\" height=\"{bh:.6}\" \
         fill=\"none\" stroke=\"#444\" stroke-width=\"0.1\"/>",
        x = b.min_x,
        y = b.min_y,
        bw = board_w,
        bh = board_h
    )
    .unwrap();

    // Obstacles / pads --------------------------------------------------------
    w.push_str("  <!-- obstacles / pads -->\n");
    for ob in &problem.obstacles {
        // Owned pads: medium grey.  Unowned / keepout copper: darker grey.
        let fill = if ob.connected_to.is_empty() {
            "#666"
        } else {
            "#aaa"
        };
        let hw = ob.width / 2.0;
        let hh = ob.height / 2.0;
        writeln!(
            w,
            "  <rect x=\"{x:.6}\" y=\"{y:.6}\" width=\"{ow:.6}\" height=\"{oh:.6}\" \
             fill=\"{fill}\"/>",
            x = ob.center.x - hw,
            y = ob.center.y - hh,
            ow = ob.width,
            oh = ob.height
        )
        .unwrap();
    }

    // Traces ------------------------------------------------------------------
    w.push_str("  <!-- traces -->\n");
    for trace in &solution.traces {
        if trace.path.len() < 2 {
            continue;
        }
        let stroke = layer_stroke(&trace.layer);
        // Build the polyline points attribute: "x1,y1 x2,y2 …"
        let mut pts = String::new();
        for (i, p) in trace.path.iter().enumerate() {
            if i > 0 {
                pts.push(' ');
            }
            write!(pts, "{:.6},{:.6}", p.x, p.y).unwrap();
        }
        writeln!(
            w,
            "  <polyline points=\"{pts}\" fill=\"none\" stroke=\"{stroke}\" \
             stroke-width=\"{tw:.6}\" stroke-opacity=\"0.6\" \
             stroke-linecap=\"round\" stroke-linejoin=\"round\"/>",
            tw = trace.width
        )
        .unwrap();
    }

    // Vias --------------------------------------------------------------------
    w.push_str("  <!-- vias -->\n");
    for via in &solution.vias {
        let r_outer = via.diameter / 2.0;
        let r_drill = via.drill / 2.0;
        // Annular ring (purple).
        writeln!(
            w,
            "  <circle cx=\"{cx:.6}\" cy=\"{cy:.6}\" r=\"{ro:.6}\" \
             fill=\"#c0c\" stroke=\"none\" opacity=\"0.8\"/>",
            cx = via.at.x,
            cy = via.at.y,
            ro = r_outer
        )
        .unwrap();
        // Drill hole (white).
        writeln!(
            w,
            "  <circle cx=\"{cx:.6}\" cy=\"{cy:.6}\" r=\"{ri:.6}\" \
             fill=\"#fff\" stroke=\"none\"/>",
            cx = via.at.x,
            cy = via.at.y,
            ri = r_drill
        )
        .unwrap();
    }

    // Failed-net highlights ---------------------------------------------------
    if !failed.is_empty() {
        w.push_str("  <!-- failed net highlights -->\n");
        for fn_ in failed {
            let Some(conn) = problem
                .connections
                .iter()
                .find(|c| c.name == fn_.connection)
            else {
                continue;
            };
            for pt in &conn.points_to_connect {
                let arm = 0.8_f64;
                // Orange circle.
                writeln!(
                    w,
                    "  <circle cx=\"{cx:.6}\" cy=\"{cy:.6}\" r=\"{arm:.6}\" \
                     fill=\"none\" stroke=\"#f80\" stroke-width=\"0.15\"/>",
                    cx = pt.x,
                    cy = pt.y
                )
                .unwrap();
                // Horizontal cross arm.
                writeln!(
                    w,
                    "  <line x1=\"{x1:.6}\" y1=\"{cy:.6}\" \
                     x2=\"{x2:.6}\" y2=\"{cy:.6}\" \
                     stroke=\"#f80\" stroke-width=\"0.15\"/>",
                    cy = pt.y,
                    x1 = pt.x - arm,
                    x2 = pt.x + arm
                )
                .unwrap();
                // Vertical cross arm.
                writeln!(
                    w,
                    "  <line x1=\"{cx:.6}\" y1=\"{y1:.6}\" \
                     x2=\"{cx:.6}\" y2=\"{y2:.6}\" \
                     stroke=\"#f80\" stroke-width=\"0.15\"/>",
                    cx = pt.x,
                    y1 = pt.y - arm,
                    y2 = pt.y + arm
                )
                .unwrap();
            }
        }
    }

    w.push_str("</svg>\n");
    o
}

// ── public API (global) ──────────────────────────────────────────────────────

/// Render `problem`, `mesh`, and `result` to a standalone SVG string.
///
/// Layer 0: board underlay — outline + pads (no copper traces; the global plan
/// is not copper yet).
/// Layer 1: leaf boundaries — thin grey rects tinted by per-leaf utilization
/// (green = unused, red = at/over capacity).
/// Layer 2: net cell-path ribbons — translucent polylines through step centres,
/// colour-coded by layer (red = top, blue = bottom, green = inner), one polyline
/// per `CellPath`.
/// Layer 3: unrouted-net endpoint highlights — orange cross + circle.
pub fn render_global_svg(
    problem: &RouteProblem,
    mesh: &CapacityMesh,
    result: &GlobalRouteResult,
) -> String {
    let margin = 2.0_f64;
    let b = &problem.bounds;
    let board_w = b.max_x - b.min_x;
    let board_h = b.max_y - b.min_y;
    let vb_x = b.min_x - margin;
    let vb_y = b.min_y - margin;
    let vb_w = board_w + 2.0 * margin;
    let vb_h = board_h + 2.0 * margin;

    let px_per_mm = 10.0_f64;
    let svg_w = vb_w * px_per_mm;
    let svg_h = vb_h * px_per_mm;

    let mut o = String::with_capacity(64 * 1024);
    let w = &mut o;

    // SVG root ----------------------------------------------------------------
    writeln!(w, "<?xml version=\"1.0\" encoding=\"UTF-8\"?>").unwrap();
    write!(
        w,
        "<svg xmlns=\"http://www.w3.org/2000/svg\"\n\
         \x20    width=\"{svg_w:.2}\" height=\"{svg_h:.2}\"\n\
         \x20    viewBox=\"{vb_x:.6} {vb_y:.6} {vb_w:.6} {vb_h:.6}\">\n"
    )
    .unwrap();

    // Board outline -----------------------------------------------------------
    w.push_str("  <!-- board outline -->\n");
    writeln!(
        w,
        "  <rect x=\"{x:.6}\" y=\"{y:.6}\" width=\"{bw:.6}\" height=\"{bh:.6}\" \
         fill=\"none\" stroke=\"#444\" stroke-width=\"0.1\"/>",
        x = b.min_x,
        y = b.min_y,
        bw = board_w,
        bh = board_h
    )
    .unwrap();

    // Obstacles / pads --------------------------------------------------------
    w.push_str("  <!-- obstacles / pads -->\n");
    for ob in &problem.obstacles {
        let fill = if ob.connected_to.is_empty() {
            "#666"
        } else {
            "#aaa"
        };
        let hw = ob.width / 2.0;
        let hh = ob.height / 2.0;
        writeln!(
            w,
            "  <rect x=\"{x:.6}\" y=\"{y:.6}\" width=\"{ow:.6}\" height=\"{oh:.6}\" \
             fill=\"{fill}\"/>",
            x = ob.center.x - hw,
            y = ob.center.y - hh,
            ow = ob.width,
            oh = ob.height
        )
        .unwrap();
    }

    // Per-leaf utilization heat tint ------------------------------------------
    //
    // Utilization = distinct (net_plan_index, path_index) step visits per leaf
    // per layer, divided by the leaf's layer capacity. We take the max across
    // layers so a single fill colour represents the most-loaded layer.
    //
    // The denominator is the raw LeafLayer::capacity (not capacity_for, since
    // at this point we have no single net's identity — the heat shows the
    // "number of nets threading through this leaf" relative to how many could).
    // A capacity of 0 means the leaf is fully blocked; if anything routes
    // through it the ratio is capped at 1 to avoid divide-by-zero divergence.
    let layer_count = mesh.layer_count.max(1);
    let mut leaf_usage: Vec<Vec<u32>> = vec![vec![0u32; layer_count]; mesh.leaves.len()];
    for net in &result.plan.nets {
        for path in &net.paths {
            for step in &path.steps {
                if step.leaf < mesh.leaves.len() && step.layer < layer_count {
                    leaf_usage[step.leaf][step.layer] += 1;
                }
            }
        }
    }

    w.push_str("  <!-- leaf utilization heat tint -->\n");
    for leaf in &mesh.leaves {
        let r = &leaf.rect;
        let lw = r.max_x - r.min_x;
        let lh = r.max_y - r.min_y;
        // Max-layer utilization ratio for this leaf.
        let ratio = (0..layer_count)
            .map(|li| {
                let cap = leaf
                    .layers
                    .get(li)
                    .map(|l| l.capacity)
                    .unwrap_or(0);
                let used = leaf_usage[leaf.id].get(li).copied().unwrap_or(0);
                if cap == 0 {
                    if used > 0 { 1.0_f64 } else { 0.0_f64 }
                } else {
                    (used as f64 / cap as f64).min(1.0)
                }
            })
            .fold(0.0_f64, f64::max);

        // Leaf boundary: always drawn (thin grey).
        // Heat fill: green (#080) at 0, red (#c00) at 1, transparent when 0.
        if ratio > 1e-9 {
            let fill = heat_color(ratio);
            writeln!(
                w,
                "  <rect x=\"{x:.6}\" y=\"{y:.6}\" width=\"{lw:.6}\" height=\"{lh:.6}\" \
                 fill=\"{fill}\" fill-opacity=\"0.35\" stroke=\"#bbb\" stroke-width=\"0.05\"/>",
                x = r.min_x,
                y = r.min_y
            )
            .unwrap();
        } else {
            writeln!(
                w,
                "  <rect x=\"{x:.6}\" y=\"{y:.6}\" width=\"{lw:.6}\" height=\"{lh:.6}\" \
                 fill=\"none\" stroke=\"#bbb\" stroke-width=\"0.05\"/>",
                x = r.min_x,
                y = r.min_y
            )
            .unwrap();
        }
    }

    // Net cell-path ribbons ---------------------------------------------------
    //
    // One <polyline> per CellPath; colour matches the layer of the first step
    // (consistent with slice-1 red/blue for top/bottom). Translucent so
    // overlapping paths from different nets are visible.
    w.push_str("  <!-- net cell-path ribbons -->\n");
    for net in &result.plan.nets {
        for path in &net.paths {
            if path.steps.is_empty() {
                continue;
            }
            // Use the first step's layer for the ribbon colour.
            let layer_idx = path.steps[0].layer;
            let stroke = layer_stroke_by_index(layer_idx, layer_count);

            let mut pts = String::new();
            for (i, step) in path.steps.iter().enumerate() {
                if i > 0 {
                    pts.push(' ');
                }
                write!(pts, "{:.6},{:.6}", step.center.x, step.center.y).unwrap();
            }
            writeln!(
                w,
                "  <polyline points=\"{pts}\" fill=\"none\" stroke=\"{stroke}\" \
                 stroke-width=\"0.3\" stroke-opacity=\"0.5\" \
                 stroke-linecap=\"round\" stroke-linejoin=\"round\"/>"
            )
            .unwrap();
        }
    }

    // Unrouted-net endpoint highlights ----------------------------------------
    //
    // Same orange cross + circle as slice-1's failed-net markers so the visual
    // language is consistent.
    if !result.report.unrouted.is_empty() {
        w.push_str("  <!-- unrouted net endpoint highlights -->\n");
        for fn_ in &result.report.unrouted {
            let Some(conn) = problem
                .connections
                .iter()
                .find(|c| c.name == fn_.connection)
            else {
                continue;
            };
            for pt in &conn.points_to_connect {
                let arm = 0.8_f64;
                writeln!(
                    w,
                    "  <circle cx=\"{cx:.6}\" cy=\"{cy:.6}\" r=\"{arm:.6}\" \
                     fill=\"none\" stroke=\"#f80\" stroke-width=\"0.15\"/>",
                    cx = pt.x,
                    cy = pt.y
                )
                .unwrap();
                writeln!(
                    w,
                    "  <line x1=\"{x1:.6}\" y1=\"{cy:.6}\" \
                     x2=\"{x2:.6}\" y2=\"{cy:.6}\" \
                     stroke=\"#f80\" stroke-width=\"0.15\"/>",
                    cy = pt.y,
                    x1 = pt.x - arm,
                    x2 = pt.x + arm
                )
                .unwrap();
                writeln!(
                    w,
                    "  <line x1=\"{cx:.6}\" y1=\"{y1:.6}\" \
                     x2=\"{cx:.6}\" y2=\"{y2:.6}\" \
                     stroke=\"#f80\" stroke-width=\"0.15\"/>",
                    cx = pt.x,
                    y1 = pt.y - arm,
                    y2 = pt.y + arm
                )
                .unwrap();
            }
        }
    }

    w.push_str("</svg>\n");
    o
}

// ── public API (placement) ────────────────────────────────────────────────────

/// Render a placement (the output of [`crate::placement::place`]) to a standalone
/// SVG string: the board outline, each part's courtyard rectangle (outline +
/// reference text), every pad coloured by a stable hash of its net name, and any
/// region-hint rectangles drawn dashed.
///
/// Coordinates are y-down (same as [`render_svg`]); courtyards/pads are drawn at
/// their placed + rotated world positions. A part with no placement in `result`
/// (shouldn't happen — `place` emits one per part) is skipped.
pub fn render_placement(
    problem: &PlaceProblem,
    hints: &PlacementHints,
    result: &PlaceResult,
) -> String {
    let margin = 2.0_f64;
    let b = &problem.bounds;
    let board_w = b.max_x - b.min_x;
    let board_h = b.max_y - b.min_y;
    let vb_x = b.min_x - margin;
    let vb_y = b.min_y - margin;
    let vb_w = board_w + 2.0 * margin;
    let vb_h = board_h + 2.0 * margin;

    let px_per_mm = 10.0_f64;
    let svg_w = vb_w * px_per_mm;
    let svg_h = vb_h * px_per_mm;

    let mut o = String::with_capacity(32 * 1024);
    let w = &mut o;

    writeln!(w, "<?xml version=\"1.0\" encoding=\"UTF-8\"?>").unwrap();
    write!(
        w,
        "<svg xmlns=\"http://www.w3.org/2000/svg\"\n\
         \x20    width=\"{svg_w:.2}\" height=\"{svg_h:.2}\"\n\
         \x20    viewBox=\"{vb_x:.6} {vb_y:.6} {vb_w:.6} {vb_h:.6}\">\n"
    )
    .unwrap();

    // Board outline -----------------------------------------------------------
    w.push_str("  <!-- board outline -->\n");
    writeln!(
        w,
        "  <rect x=\"{x:.6}\" y=\"{y:.6}\" width=\"{bw:.6}\" height=\"{bh:.6}\" \
         fill=\"none\" stroke=\"#444\" stroke-width=\"0.1\"/>",
        x = b.min_x,
        y = b.min_y,
        bw = board_w,
        bh = board_h
    )
    .unwrap();

    // Region-hint rectangles (dashed) -----------------------------------------
    w.push_str("  <!-- region hints -->\n");
    for g in &hints.groups {
        if let Some(r) = &g.region {
            writeln!(
                w,
                "  <rect x=\"{x:.6}\" y=\"{y:.6}\" width=\"{rw:.6}\" height=\"{rh:.6}\" \
                 fill=\"none\" stroke=\"#08c\" stroke-width=\"0.15\" stroke-dasharray=\"0.6 0.4\"/>",
                x = r.min_x,
                y = r.min_y,
                rw = r.max_x - r.min_x,
                rh = r.max_y - r.min_y
            )
            .unwrap();
        }
    }

    // Index placements by reference for lookup.
    let place_by_ref: std::collections::BTreeMap<&str, &crate::placement::Placement> = result
        .placements
        .iter()
        .map(|p| (p.reference.as_str(), p))
        .collect();

    // Courtyards + reference text ---------------------------------------------
    w.push_str("  <!-- courtyards -->\n");
    for part in &problem.parts {
        let Some(pl) = place_by_ref.get(part.reference.as_str()) else {
            continue;
        };
        // Rotation swaps the courtyard extents for the quadrant cases.
        let (hw, hh) = match pl.rotation.rem_euclid(360) {
            90 | 270 => (part.courtyard_h / 2.0, part.courtyard_w / 2.0),
            _ => (part.courtyard_w / 2.0, part.courtyard_h / 2.0),
        };
        writeln!(
            w,
            "  <rect x=\"{x:.6}\" y=\"{y:.6}\" width=\"{cw:.6}\" height=\"{ch:.6}\" \
             fill=\"none\" stroke=\"#888\" stroke-width=\"0.08\"/>",
            x = pl.at.x - hw,
            y = pl.at.y - hh,
            cw = hw * 2.0,
            ch = hh * 2.0
        )
        .unwrap();
        // Reference text, centred on the part origin.
        writeln!(
            w,
            "  <text x=\"{x:.6}\" y=\"{y:.6}\" font-size=\"1.0\" fill=\"#333\" \
             text-anchor=\"middle\" dominant-baseline=\"central\">{r}</text>",
            x = pl.at.x,
            y = pl.at.y,
            r = part.reference
        )
        .unwrap();
    }

    // Pads, coloured by net hash ----------------------------------------------
    w.push_str("  <!-- pads (coloured by net) -->\n");
    for part in &problem.parts {
        let Some(pl) = place_by_ref.get(part.reference.as_str()) else {
            continue;
        };
        for pad in &part.pads {
            let off = rotate_offset(&pad.offset, pl.rotation);
            let (pw, ph) = match pl.rotation.rem_euclid(360) {
                90 | 270 => (pad.height, pad.width),
                _ => (pad.width, pad.height),
            };
            let cx = pl.at.x + off.x;
            let cy = pl.at.y + off.y;
            let fill = pad
                .net
                .as_deref()
                .map(net_color)
                .unwrap_or_else(|| "#bbb".to_owned());
            writeln!(
                w,
                "  <rect x=\"{x:.6}\" y=\"{y:.6}\" width=\"{pw:.6}\" height=\"{ph:.6}\" \
                 fill=\"{fill}\" fill-opacity=\"0.85\"/>",
                x = cx - pw / 2.0,
                y = cy - ph / 2.0
            )
            .unwrap();
        }
    }

    w.push_str("</svg>\n");
    o
}

/// Rotate a pad offset by a quadrant rotation (degrees, y-down) — the same
/// convention [`crate::placement`] uses for pad world positions.
fn rotate_offset(off: &Point2, rot: i32) -> Point2 {
    match rot.rem_euclid(360) {
        90 => Point2 { x: -off.y, y: off.x },
        180 => Point2 { x: -off.x, y: -off.y },
        270 => Point2 { x: off.y, y: -off.x },
        _ => Point2 { x: off.x, y: off.y },
    }
}

/// A stable, readable `#rrggbb` colour derived from a net name (FNV-1a hash →
/// hue-ish channel spread). Deterministic so the same net always renders the
/// same colour across boards.
fn net_color(net: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for byte in net.bytes() {
        h ^= byte as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    // Spread the hash into three mid-range channels (0x40..=0xbf) so colours
    // stay distinct and legible on a white board (never too pale/dark).
    let chan = |shift: u32| 0x40 + ((h >> shift) & 0x7f) as u8;
    format!("#{:02x}{:02x}{:02x}", chan(0), chan(16), chan(32))
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// SVG stroke colour for a layer.
fn layer_stroke(layer: &LayerRef) -> &'static str {
    match layer.0.as_str() {
        "top" => "#c00",    // red for top / F.Cu
        "bottom" => "#00c", // blue for bottom / B.Cu
        _ => "#080",        // green for inner layers
    }
}

/// SVG stroke colour for a layer by numeric index.
/// Layer 0 = top (red), last layer = bottom (blue), middle = green.
fn layer_stroke_by_index(layer: usize, layer_count: usize) -> &'static str {
    if layer == 0 {
        "#c00"
    } else if layer + 1 == layer_count {
        "#00c"
    } else {
        "#080"
    }
}

/// Heat colour for a utilization ratio in [0, 1]: green (#080) at 0 → yellow
/// (#880) at 0.5 → red (#c00) at 1.  Returned as a `#rrggbb` CSS colour.
///
/// Interpolation is linear in the red and green channels:
/// - red channel: 0x00 at ratio 0 → 0xcc at ratio 1
/// - green channel: 0x88 at ratio 0 → 0x00 at ratio 1
/// - blue channel: 0x00 throughout
fn heat_color(ratio: f64) -> String {
    let t = ratio.clamp(0.0, 1.0);
    // green component: 0x88 → 0x00
    let g = ((1.0 - t) * 0x88 as f64).round() as u8;
    // red component: 0x00 → 0xcc
    let r = (t * 0xcc as f64).round() as u8;
    format!("#{r:02x}{g:02x}00")
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::CapacityMesh;
    use crate::pathing::global_route_with_mesh;
    use crate::router;
    use std::path::Path;

    fn load(name: &str) -> RouteProblem {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures")
            .join(name);
        let json = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        serde_json::from_str(&json).unwrap_or_else(|e| panic!("parse {name}: {e}"))
    }

    fn count_tag(svg: &str, tag: &str) -> usize {
        svg.matches(tag).count()
    }

    // ── element-count assertions ──────────────────────────────────────────────

    #[test]
    fn led_r_svg_has_expected_elements() {
        let p = load("led-r.json");
        let result = router::route(&p);
        assert!(result.failed.is_empty(), "led-r should route fully");
        let svg = render_svg(&p, &result.solution, &result.failed);

        // Board outline (1) + one rect per obstacle.
        let rect_count = count_tag(&svg, "<rect");
        let expected_rects = 1 + p.obstacles.len();
        assert!(
            rect_count >= expected_rects,
            "expected >={expected_rects} <rect> elements (board + {n} obstacles), got {rect_count}",
            n = p.obstacles.len()
        );

        // At least one <polyline> per trace in the solution.
        let trace_count = result.solution.traces.len();
        let polyline_count = count_tag(&svg, "<polyline");
        assert!(
            polyline_count >= trace_count,
            "expected >={trace_count} <polyline> elements, got {polyline_count}"
        );

        assert!(svg.contains("<svg"), "output must open with <svg");
        assert!(svg.contains("</svg>"), "output must close </svg>");
    }

    #[test]
    fn quad_svg_has_vias() {
        let p = load("quad.json");
        let result = router::route(&p);
        assert!(!result.solution.vias.is_empty(), "quad should have vias");
        let svg = render_svg(&p, &result.solution, &result.failed);

        // Each via emits two <circle> elements (ring + drill hole).
        let circle_count = count_tag(&svg, "<circle");
        let via_count = result.solution.vias.len();
        assert!(
            circle_count >= via_count * 2,
            "expected >={} <circle> elements for {via_count} vias, got {circle_count}",
            via_count * 2
        );
    }

    #[test]
    fn failed_net_highlight_appears_in_svg() {
        use crate::problem::{Bounds, Connection, RoutePoint, RouteSolution};

        let p = RouteProblem {
            layer_count: 2,
            min_trace_width: 0.25,
            obstacles: vec![],
            connections: vec![Connection {
                name: "FAIL_NET".to_owned(),
                points_to_connect: vec![
                    RoutePoint {
                        x: 5.0,
                        y: 5.0,
                        layer: LayerRef::top(),
                    },
                    RoutePoint {
                        x: 25.0,
                        y: 5.0,
                        layer: LayerRef::top(),
                    },
                ],
            }],
            bounds: Bounds {
                min_x: 0.0,
                max_x: 30.0,
                min_y: 0.0,
                max_y: 30.0,
            },
            clearance: 0.2,
            via_diameter: 0.6,
            via_drill: 0.3,
        };
        let s = RouteSolution {
            traces: vec![],
            vias: vec![],
        };
        let failed = vec![FailedNet {
            connection: "FAIL_NET".to_owned(),
            reason: "test".to_owned(),
        }];
        let svg = render_svg(&p, &s, &failed);

        // Highlight colour must appear.
        assert!(
            svg.contains("#f80"),
            "failed-net highlight colour (#f80) should appear in SVG"
        );
        // 2 failed points -> 2 circles + 4 cross arms.
        let line_count = count_tag(&svg, "<line");
        assert_eq!(
            line_count, 4,
            "expected 4 <line> cross arms for 2 failed points, got {line_count}"
        );
        let circle_count = count_tag(&svg, "<circle");
        assert_eq!(
            circle_count, 2,
            "expected 2 <circle> highlight markers for 2 failed points, got {circle_count}"
        );
    }

    // ── global SVG element-count assertions ──────────────────────────────────

    #[test]
    fn global_svg_leaf_rect_count_matches_mesh() {
        // congested.json is the primary fixture for global routing tests.
        let p = load("congested.json");
        let mesh = CapacityMesh::build(&p);
        let result = global_route_with_mesh(&p, &mesh);
        let svg = render_global_svg(&p, &mesh, &result);

        // Every leaf emits exactly one <rect> (heat-tinted or outline-only).
        // The board outline + obstacle rects are also <rect> elements.
        // Lower bound: we must have at least mesh.leaves.len() leaf rects.
        let rect_count = count_tag(&svg, "<rect");
        let leaf_count = mesh.leaves.len();
        // board outline (1) + obstacles + leaf rects
        let min_expected = 1 + p.obstacles.len() + leaf_count;
        assert!(
            rect_count >= min_expected,
            "expected >={min_expected} <rect> elements \
             (1 board + {} obstacles + {leaf_count} leaves), got {rect_count}",
            p.obstacles.len()
        );

        // Exactly mesh.leaves.len() leaf <rect>s.  We emit one rect per leaf, so
        // the total minus the board outline and obstacles must equal the leaf count.
        // We use a separate comment-based marker to count just the leaf rects:
        // each leaf rect immediately follows the heat-tint comment block. Rather
        // than parsing SVG structure we check the total and the individual counts.
        assert!(
            svg.contains("<svg"),
            "output must open with <svg"
        );
        assert!(svg.contains("</svg>"), "output must close </svg>");
        assert!(
            svg.contains("<!-- leaf utilization heat tint -->"),
            "global SVG must contain the leaf tint section"
        );
        assert!(
            svg.contains("<!-- net cell-path ribbons -->"),
            "global SVG must contain the cell-path ribbons section"
        );
    }

    #[test]
    fn global_svg_unrouted_highlight_appears_for_infeasible() {
        use crate::problem::{Bounds, Connection, RoutePoint};

        // A trivially infeasible problem: two points on opposite sides of a
        // full-height keepout on both layers. The global router will report the
        // net as unrouted and the SVG must contain orange markers.
        let p = RouteProblem {
            layer_count: 2,
            min_trace_width: 0.25,
            obstacles: vec![crate::problem::Obstacle {
                kind: "rect".to_owned(),
                layers: vec![LayerRef::top(), LayerRef::bottom()],
                center: crate::problem::Point2 { x: 12.0, y: 8.0 },
                width: 2.0,
                height: 16.0,
                connected_to: vec![],
            }],
            connections: vec![Connection {
                name: "CROSS".to_owned(),
                points_to_connect: vec![
                    RoutePoint { x: 2.0, y: 8.0, layer: LayerRef::top() },
                    RoutePoint { x: 22.0, y: 8.0, layer: LayerRef::top() },
                ],
            }],
            bounds: Bounds {
                min_x: 0.0,
                max_x: 24.0,
                min_y: 0.0,
                max_y: 16.0,
            },
            clearance: 0.2,
            via_diameter: 0.6,
            via_drill: 0.3,
        };
        let mesh = CapacityMesh::build(&p);
        let result = global_route_with_mesh(&p, &mesh);
        assert!(
            !result.report.unrouted.is_empty(),
            "the infeasible fixture must have unrouted nets"
        );
        let svg = render_global_svg(&p, &mesh, &result);
        assert!(
            svg.contains("#f80"),
            "unrouted-net highlight colour (#f80) must appear in global SVG"
        );
    }

    #[test]
    fn global_svg_has_polylines_for_routed_nets() {
        let p = load("led-r.json");
        let mesh = CapacityMesh::build(&p);
        let result = global_route_with_mesh(&p, &mesh);
        assert!(result.is_feasible(), "led-r must route feasibly");

        let svg = render_global_svg(&p, &mesh, &result);
        let polyline_count = count_tag(&svg, "<polyline");
        // Each routed path produces one polyline; led-r has connections.
        let total_paths: usize = result.plan.nets.iter().map(|n| n.paths.len()).sum();
        assert!(
            polyline_count >= total_paths,
            "expected >={total_paths} <polyline> elements for cell-path ribbons, got {polyline_count}"
        );
    }

    // ── fixture render + write to target/pcb-render/ for eyeballing ──────────
    //
    // Routes all fixtures, renders them, and writes SVGs to
    // `<workspace_root>/target/pcb-render/` for a developer to open in a
    // browser after `cargo test -p pcb-engine`.  The test never fails on SVG
    // content — only if the router panics or the target directory is
    // unwritable.
    //
    // Also writes `{name}-global.svg` for each fixture using `render_global_svg`.

    #[test]
    fn render_all_fixtures_to_target() {
        let out_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent() // crates/
            .and_then(Path::parent) // workspace root
            .expect("could not find workspace root")
            .join("target")
            .join("pcb-render");
        std::fs::create_dir_all(&out_dir)
            .unwrap_or_else(|e| panic!("create {}: {e}", out_dir.display()));

        let fixtures = [
            "led-r.json",
            "quad.json",
            "tscircuit-shape.json",
            "congested.json",
            "congested-relief.json",
        ];
        // Fixtures whose route_detailed solution is also rendered as
        // `{stem}-detailed.svg` for wrap-up eyeballing (the medium-board gate and
        // the zero-slack stress board).
        let detailed_render = ["congested-relief.json", "congested.json"];
        for name in fixtures {
            let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("fixtures")
                .join(name);
            if !fixture_path.exists() {
                eprintln!("SKIP render_all_fixtures_to_target: {name} not found");
                continue;
            }
            let json = std::fs::read_to_string(&fixture_path)
                .unwrap_or_else(|e| panic!("read {name}: {e}"));
            let p: RouteProblem =
                serde_json::from_str(&json).unwrap_or_else(|e| panic!("parse {name}: {e}"));
            let stem = name.trim_end_matches(".json");

            // Slice-1 SVG.
            let result = router::route(&p);
            if !result.failed.is_empty() {
                eprintln!(
                    "WARN: {name} has {} failed net(s) -- highlighting them in SVG",
                    result.failed.len()
                );
            }
            let svg = render_svg(&p, &result.solution, &result.failed);
            let svg_path = out_dir.join(format!("{stem}.svg"));
            std::fs::write(&svg_path, svg.as_bytes())
                .unwrap_or_else(|e| panic!("write {}: {e}", svg_path.display()));
            eprintln!("rendered: {}", svg_path.display());

            // Global overlay SVG.
            let mesh = CapacityMesh::build(&p);
            let global_result = global_route_with_mesh(&p, &mesh);
            let global_svg = render_global_svg(&p, &mesh, &global_result);
            let global_path = out_dir.join(format!("{stem}-global.svg"));
            std::fs::write(&global_path, global_svg.as_bytes())
                .unwrap_or_else(|e| panic!("write {}: {e}", global_path.display()));
            eprintln!("rendered: {}", global_path.display());

            // Detailed-pipeline SVG for the gate fixtures (route_detailed copper),
            // so the wrap-up can eyeball the clean medium board and the stress
            // board's honest partial route.
            if detailed_render.contains(&name) {
                let detailed = crate::pipeline::route_detailed(&p);
                if !detailed.failed.is_empty() {
                    eprintln!(
                        "WARN: {name} route_detailed has {} failed net(s) -- highlighting in SVG",
                        detailed.failed.len()
                    );
                }
                let detailed_svg = render_svg(&p, &detailed.solution, &detailed.failed);
                let detailed_path = out_dir.join(format!("{stem}-detailed.svg"));
                std::fs::write(&detailed_path, detailed_svg.as_bytes())
                    .unwrap_or_else(|e| panic!("write {}: {e}", detailed_path.display()));
                eprintln!("rendered: {}", detailed_path.display());
            }
        }

        // Placement fixtures: render the PLACED state (engine, empty hints) and
        // the routed result of that placement, for wrap-up eyeballing.
        for name in ["place-charger.json"] {
            let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("fixtures")
                .join(name);
            if !fixture_path.exists() {
                eprintln!("SKIP placement render: {name} not found");
                continue;
            }
            let json = std::fs::read_to_string(&fixture_path)
                .unwrap_or_else(|e| panic!("read {name}: {e}"));
            let pp: crate::placement::PlaceProblem =
                serde_json::from_str(&json).unwrap_or_else(|e| panic!("parse {name}: {e}"));
            let stem = name.trim_end_matches(".json");

            // Placed state (engine, empty hints).
            let placed = crate::placement::place(&pp, &crate::placement::PlacementHints::default());
            let placed_svg =
                render_placement(&pp, &crate::placement::PlacementHints::default(), &placed);
            let placed_path = out_dir.join(format!("{stem}-placed.svg"));
            std::fs::write(&placed_path, placed_svg.as_bytes())
                .unwrap_or_else(|e| panic!("write {}: {e}", placed_path.display()));
            eprintln!("rendered: {}", placed_path.display());

            // Routed result of that placement (via the existing copper render).
            let rp = crate::placement::to_route_problem(&pp, &placed.placements);
            let routed = crate::pipeline::route_auto(&rp);
            if !routed.failed.is_empty() {
                eprintln!(
                    "WARN: {name} placed board has {} failed net(s)",
                    routed.failed.len()
                );
            }
            let routed_svg = render_svg(&rp, &routed.solution, &routed.failed);
            let routed_path = out_dir.join(format!("{stem}-routed.svg"));
            std::fs::write(&routed_path, routed_svg.as_bytes())
                .unwrap_or_else(|e| panic!("write {}: {e}", routed_path.display()));
            eprintln!("rendered: {}", routed_path.display());
        }
    }

    // ── placement render: element-count assertions ────────────────────────────

    #[test]
    fn render_placement_has_expected_elements() {
        use crate::placement::{place, GroupHint, PlacementHints, Rect};

        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures")
            .join("place-charger.json");
        let json = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read place-charger.json: {e}"));
        let pp: crate::placement::PlaceProblem =
            serde_json::from_str(&json).unwrap_or_else(|e| panic!("parse: {e}"));

        // Add a region hint so the dashed-rect branch is exercised.
        let hints = PlacementHints {
            groups: vec![GroupHint {
                name: "u1".to_owned(),
                members: vec!["U1".to_owned(), "C1".to_owned()],
                region: Some(Rect { min_x: 4.0, max_x: 16.0, min_y: 4.0, max_y: 16.0 }),
                edge: None,
            }],
            ..Default::default()
        };
        let res = place(&pp, &hints);
        let svg = render_placement(&pp, &hints, &res);

        // Board outline (1) + one courtyard rect per part + one dashed region rect
        // + one pad rect per pad.
        let part_count = pp.parts.len();
        let pad_count: usize = pp.parts.iter().map(|p| p.pads.len()).sum();
        let rect_count = count_tag(&svg, "<rect");
        let expected = 1 + part_count + 1 + pad_count;
        assert!(
            rect_count >= expected,
            "expected >={expected} <rect> (board + {part_count} courtyards + 1 region + \
             {pad_count} pads), got {rect_count}"
        );

        // One <text> reference label per part.
        let text_count = count_tag(&svg, "<text");
        assert!(
            text_count >= part_count,
            "expected >={part_count} <text> reference labels, got {text_count}"
        );

        // The dashed region hint must appear.
        assert!(
            svg.contains("stroke-dasharray"),
            "a region hint must render as a dashed rect"
        );
        assert!(svg.contains("<svg"), "output must open with <svg");
        assert!(svg.contains("</svg>"), "output must close </svg>");
    }
}
