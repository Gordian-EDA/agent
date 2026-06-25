//! Diagnostic SVG render of the PCB engine's *own* view of a board.
//!
//! The fast in-loop view — no KiCAD needed — distinct from the professional
//! `kicad-cli` production render. [`render_svg`] draws a routed board (outline +
//! pads + copper + vias + failed-net markers); [`render_placement`] draws a
//! placement (courtyards + reference text + net-coloured pads + region hints).
//! Pure string assembly over `pcb-model`/`grid-astar`/`pcb-place` types.
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

use std::fmt::Write as _;

use grid_astar::router::FailedNet;
use pcb_model::{LayerRef, Point2, Rect, RouteProblem, RouteSolution};
use pcb_place::placement::{PlaceProblem, PlaceResult, Placement, PlacementHints};

/// Emit the board boundary: the custom polygon `outline` when present (>= 3 pts),
/// else the `bounds` rectangle. So a circle / hexagon / any custom-shaped board
/// shows its TRUE shape in the render — the agent's eyes for iterating on a custom
/// outline — instead of a misleading bounding-box square.
fn push_board_outline(w: &mut String, bounds: &Rect, outline: Option<&[Point2]>) {
    w.push_str("  <!-- board outline -->\n");
    if let Some(poly) = outline
        && poly.len() >= 3
    {
        let pts: String = poly
            .iter()
            .map(|p| format!("{:.4},{:.4}", p.x, p.y))
            .collect::<Vec<_>>()
            .join(" ");
        writeln!(
            w,
            "  <polygon points=\"{pts}\" fill=\"none\" stroke=\"#444\" stroke-width=\"0.1\"/>"
        )
        .unwrap();
        return;
    }
    writeln!(
        w,
        "  <rect x=\"{x:.6}\" y=\"{y:.6}\" width=\"{bw:.6}\" height=\"{bh:.6}\" \
         fill=\"none\" stroke=\"#444\" stroke-width=\"0.1\"/>",
        x = bounds.min_x,
        y = bounds.min_y,
        bw = bounds.max_x - bounds.min_x,
        bh = bounds.max_y - bounds.min_y
    )
    .unwrap();
}

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
    push_board_outline(w, b, problem.outline.as_deref());

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

/// Render a placement (the output of `pcb_place::placement::place`) to a standalone
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
    push_board_outline(w, b, problem.outline.as_deref());

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
    let place_by_ref: std::collections::BTreeMap<&str, &Placement> = result
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
        let (hw, hh) = match geom::snap_quadrant(pl.rotation) as i32 {
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
            let off = pad.offset.rotate(pl.rotation);
            let (pw, ph) = match geom::snap_quadrant(pl.rotation) as i32 {
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

/// A stable, readable `#rrggbb` colour derived from a net name (FNV-1a hash →
/// hue-ish channel spread). Deterministic so the same net always renders the
/// same colour across boards.
fn net_color(net: &str) -> String {
    let h = geom::hash::fnv1a(net.as_bytes());
    // Spread the hash into three mid-range channels (0x40..=0xbf) so colours
    // stay distinct and legible on a white board (never too pale/dark).
    let chan = |shift: u32| 0x40 + ((h >> shift) & 0x7f) as u8;
    format!("#{:02x}{:02x}{:02x}", chan(0), chan(16), chan(32))
}

/// SVG stroke colour for a layer.
fn layer_stroke(layer: &LayerRef) -> &'static str {
    match layer.0.as_str() {
        "top" => "#c00",    // red for top / F.Cu
        "bottom" => "#00c", // blue for bottom / B.Cu
        _ => "#080",        // green for inner layers
    }
}
