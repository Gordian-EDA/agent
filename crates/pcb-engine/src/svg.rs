//! SVG debug render for a routed PCB.
//!
//! [`render_svg`] produces a standalone SVG string from a [`RouteProblem`],
//! a [`RouteSolution`], and the optional list of nets that failed routing
//! (from [`crate::router::RouteResult::failed`]). Pure string assembly — no
//! external dependencies beyond the standard library.
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

use crate::problem::{LayerRef, RouteProblem, RouteSolution};
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

// ── helpers ──────────────────────────────────────────────────────────────────

/// SVG stroke colour for a layer.
fn layer_stroke(layer: &LayerRef) -> &'static str {
    match layer.0.as_str() {
        "top" => "#c00",    // red for top / F.Cu
        "bottom" => "#00c", // blue for bottom / B.Cu
        _ => "#080",        // green for inner layers
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
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

    // ── fixture render + write to target/pcb-render/ for eyeballing ──────────
    //
    // Routes all fixtures, renders them, and writes SVGs to
    // `<workspace_root>/target/pcb-render/` for a developer to open in a
    // browser after `cargo test -p pcb-engine`.  The test never fails on SVG
    // content — only if the router panics or the target directory is
    // unwritable.

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

        let fixtures = ["led-r.json", "quad.json", "tscircuit-shape.json"];
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
            let result = router::route(&p);
            if !result.failed.is_empty() {
                eprintln!(
                    "WARN: {name} has {} failed net(s) -- highlighting them in SVG",
                    result.failed.len()
                );
            }
            let svg = render_svg(&p, &result.solution, &result.failed);
            let stem = name.trim_end_matches(".json");
            let svg_path = out_dir.join(format!("{stem}.svg"));
            std::fs::write(&svg_path, svg.as_bytes())
                .unwrap_or_else(|e| panic!("write {}: {e}", svg_path.display()));
            eprintln!("rendered: {}", svg_path.display());
        }
    }
}
