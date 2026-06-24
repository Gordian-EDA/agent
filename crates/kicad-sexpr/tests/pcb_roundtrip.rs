//! Read-side tests for `kicad_sexpr::pcb`: the hand-authored `two_res`
//! fixture must parse cleanly and translate into the expected routing problem.
//! (The write side extends this file in the next task.)

use std::path::PathBuf;

use kicad_sexpr::pcb::{extract_copper, read_problem, write_solution};
use pcb_model::{LayerRef, Point2, RouteSolution, Trace, Via, ViaSpan};

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/two_res.kicad_pcb")
}

const EPS: f64 = 1e-6;

// ── write-back test helpers ────────────────────────────────────────────────

/// Copy the read-only fixture into a fresh temp file so a write test never
/// mutates the checked-in board. Returns the temp file (keep it alive) and path.
fn fixture_copy() -> (tempfile::NamedTempFile, PathBuf) {
    let tmp = tempfile::Builder::new()
        .prefix("gordian-rt-")
        .suffix(".kicad_pcb")
        .tempfile()
        .expect("tempfile");
    std::fs::copy(fixture(), tmp.path()).expect("copy fixture");
    let path = tmp.path().to_path_buf();
    (tmp, path)
}

/// The hand-built solution exercised by the write tests: a SIG L-route (two
/// segments on top) and a GND route with a layer change (top segment → via →
/// bottom segment). Routes really touch the pads (verified by the oracle test).
fn hand_solution() -> RouteSolution {
    let p = |x: f64, y: f64| Point2 { x, y };
    // All four pads sit on F.Cu (top), so the two SIG/GND corridors are kept on
    // separate y-bands to avoid a same-layer short, and the GND route exercises
    // a layer change. Because the destination GND pad is top-only, the single via
    // is placed *on* that pad: it stitches the bottom detour up to the top pad,
    // and the bottom segment carries the last stretch of the GND path. (The
    // connectivity oracle is the authority — these coordinates are tuned to make
    // it pass.)
    RouteSolution {
        traces: vec![
            // SIG (top, routed in the upper band y≈12.5 clear of the GND pads):
            // R1 pad1 (7.0875, 10) → up → across → down to R2 pad1 (22, 10.9125).
            Trace {
                connection: "SIG".to_owned(),
                layer: LayerRef::top(),
                width: 0.25,
                path: vec![
                    p(7.0875, 10.0),
                    p(7.0875, 12.5),
                    p(22.0, 12.5),
                    p(22.0, 10.9125),
                ],
            },
            // GND on top: R1 pad2 (8.9125, 10) → R2 pad2 (22, 9.0875), kept in
            // the lower band so it never meets the SIG corridor. This is the
            // load-bearing GND path (both pads are top-only).
            Trace {
                connection: "GND".to_owned(),
                layer: LayerRef::top(),
                width: 0.25,
                path: vec![p(8.9125, 10.0), p(22.0, 9.0875)],
            },
            // GND on bottom: a same-net spur from the destination pad. The single
            // via at (22, 9.0875) stitches it up to the top GND pad, so the
            // bottom emitter is exercised without breaking connectivity.
            Trace {
                connection: "GND".to_owned(),
                layer: LayerRef::bottom(),
                width: 0.25,
                path: vec![p(22.0, 9.0875), p(18.0, 8.0)],
            },
        ],
        // One via at the destination GND pad, stitching the bottom spur to the
        // top-layer GND copper.
        vias: vec![Via {
            connection: "GND".to_owned(),
            at: p(22.0, 9.0875),
            diameter: 0.6,
            drill: 0.3,
            span: ViaSpan::Through,
        }],
    }
}

/// The fixture parses with zero kiutils diagnostics.
#[test]
fn fixture_parses_without_diagnostics() {
    let doc = kiutils_kicad::PcbFile::read(fixture()).expect("read fixture");
    assert!(
        doc.diagnostics().is_empty(),
        "expected zero diagnostics, got {:?}",
        doc.diagnostics()
    );
    assert_eq!(doc.ast().version, Some(20241229));
}

/// Two signal copper layers → ["F.Cu", "B.Cu"], layer_count 2.
#[test]
fn layer_mapping_and_count() {
    let board = read_problem(&fixture()).unwrap();
    assert_eq!(
        board.layer_names,
        vec!["F.Cu".to_owned(), "B.Cu".to_owned()]
    );
    assert_eq!(board.problem.layer_count, 2);
}

/// Net codes for the named nets (code 0 / empty skipped).
#[test]
fn net_codes_mapping() {
    let board = read_problem(&fixture()).unwrap();
    assert_eq!(board.net_codes.get("GND"), Some(&1));
    assert_eq!(board.net_codes.get("SIG"), Some(&2));
    assert_eq!(board.net_codes.len(), 2);
}

/// Exactly two connections (GND, SIG), each with two points.
#[test]
fn connections_have_two_points_each() {
    let board = read_problem(&fixture()).unwrap();
    let mut names: Vec<&str> = board
        .problem
        .connections
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    names.sort_unstable();
    assert_eq!(names, vec!["GND", "SIG"]);
    for conn in &board.problem.connections {
        assert_eq!(conn.points_to_connect.len(), 2, "net {}", conn.name);
        for p in &conn.points_to_connect {
            assert_eq!(p.layer, LayerRef::top(), "all pads on F.Cu");
        }
    }
}

/// Hand-computed absolute pad positions.
///
/// Rotation rule (y-down, CCW-positive): an offset (dx, dy) rotated by θ
/// becomes (dx·cosθ + dy·sinθ, −dx·sinθ + dy·cosθ).
///
/// R1 at (8, 10), footprint rotation 0:
///   pad1 offset (-0.9125, 0) → (-0.9125, 0)      → abs (7.0875, 10)   [SIG]
///   pad2 offset (+0.9125, 0) → (+0.9125, 0)      → abs (8.9125, 10)   [GND]
///
/// R2 at (22, 10), footprint rotation 90 (cosθ=0, sinθ=1 → (dx,dy)→(dy,-dx)):
///   pad1 offset (-0.9125, 0) → (0, 0.9125)       → abs (22, 10.9125)  [SIG]
///   pad2 offset (+0.9125, 0) → (0, -0.9125)      → abs (22, 9.0875)   [GND]
#[test]
fn pad_positions_match_hand_math() {
    let board = read_problem(&fixture()).unwrap();

    let point = |net: &str, near: (f64, f64)| {
        board
            .problem
            .connections
            .iter()
            .find(|c| c.name == net)
            .unwrap()
            .points_to_connect
            .iter()
            .find(|p| (p.x - near.0).abs() < 0.5 && (p.y - near.1).abs() < 0.5)
            .unwrap_or_else(|| panic!("no {net} point near {near:?}"))
    };

    // R1 (rotation 0)
    let r1_sig = point("SIG", (7.0875, 10.0));
    assert!((r1_sig.x - 7.0875).abs() < EPS && (r1_sig.y - 10.0).abs() < EPS);
    let r1_gnd = point("GND", (8.9125, 10.0));
    assert!((r1_gnd.x - 8.9125).abs() < EPS && (r1_gnd.y - 10.0).abs() < EPS);

    // R2 (rotation 90)
    let r2_sig = point("SIG", (22.0, 10.9125));
    assert!((r2_sig.x - 22.0).abs() < EPS && (r2_sig.y - 10.9125).abs() < EPS);
    let r2_gnd = point("GND", (22.0, 9.0875));
    assert!((r2_gnd.x - 22.0).abs() < EPS && (r2_gnd.y - 9.0875).abs() < EPS);
}

/// Bounds come from the Edge.Cuts gr_rect: (0,0)..(30,20).
#[test]
fn bounds_match_outline() {
    let board = read_problem(&fixture()).unwrap();
    let b = &board.problem.bounds;
    assert!((b.min_x - 0.0).abs() < EPS);
    assert!((b.min_y - 0.0).abs() < EPS);
    assert!((b.max_x - 30.0).abs() < EPS);
    assert!((b.max_y - 20.0).abs() < EPS);
}

/// Every pad appears as an obstacle with the right `connected_to` net, and the
/// rotated-rect AABB matches hand-math (a 1.025×1.4 pad rotated 90° → 1.4×1.025).
#[test]
fn pads_appear_as_obstacles() {
    let board = read_problem(&fixture()).unwrap();

    // 4 pads → at least 4 obstacles (no segments/vias/zones in the fixture).
    assert_eq!(board.problem.obstacles.len(), 4);

    // Each pad obstacle is tagged with its net and sits on F.Cu (top).
    let sig_count = board
        .problem
        .obstacles
        .iter()
        .filter(|o| o.connected_to == vec!["SIG".to_owned()])
        .count();
    let gnd_count = board
        .problem
        .obstacles
        .iter()
        .filter(|o| o.connected_to == vec!["GND".to_owned()])
        .count();
    assert_eq!(sig_count, 2);
    assert_eq!(gnd_count, 2);

    for o in &board.problem.obstacles {
        assert_eq!(o.kind, "rect");
        assert_eq!(o.layers, vec![LayerRef::top()]);
    }

    // R1's SIG pad: unrotated 1.025 × 1.4 AABB at (7.0875, 10).
    let r1 = board
        .problem
        .obstacles
        .iter()
        .find(|o| (o.center.x - 7.0875).abs() < EPS && (o.center.y - 10.0).abs() < EPS)
        .expect("R1 SIG pad obstacle");
    assert!((r1.width - 1.025).abs() < EPS);
    assert!((r1.height - 1.4).abs() < EPS);
    assert_eq!(r1.connected_to, vec!["SIG".to_owned()]);

    // R2's SIG pad: 90°-rotated → width/height swap to 1.4 × 1.025 at (22, 10.9125).
    let r2 = board
        .problem
        .obstacles
        .iter()
        .find(|o| (o.center.x - 22.0).abs() < EPS && (o.center.y - 10.9125).abs() < EPS)
        .expect("R2 SIG pad obstacle");
    assert!((r2.width - 1.4).abs() < EPS, "width {}", r2.width);
    assert!((r2.height - 1.025).abs() < EPS, "height {}", r2.height);
    assert_eq!(r2.connected_to, vec!["SIG".to_owned()]);
}

// ── write-back (Task 5) ────────────────────────────────────────────────────

/// Writing the hand-built solution re-reads cleanly: zero diagnostics, the
/// emitted segment/via counts appear, and the original prefix bytes are intact.
#[test]
fn write_solution_round_trips() {
    let (_keep, path) = fixture_copy();
    let original = std::fs::read_to_string(&path).unwrap();

    let board = read_problem(&path).unwrap();
    let solution = hand_solution();
    write_solution(&path, &solution, &board).expect("write_solution");

    let doc = kiutils_kicad::PcbFile::read(&path).expect("re-read written board");
    assert!(
        doc.diagnostics().is_empty(),
        "expected zero diagnostics, got {:?}",
        doc.diagnostics()
    );
    // SIG = 3 segments (4-point L-with-jog), GND = 1 top + 1 bottom = 2 → 5.
    assert_eq!(doc.ast().segments.len(), 5);
    assert_eq!(doc.ast().vias.len(), 1);

    // Lossless: the written file STARTS WITH the original up to the splice point
    // (the splice is just before the final root paren), so the original prefix
    // is unchanged.
    let written = std::fs::read_to_string(&path).unwrap();
    let prefix = &original[..original.rfind(')').unwrap()];
    assert!(
        written.starts_with(prefix),
        "written file must preserve the original prefix bytes"
    );
}

/// Re-extracting the written board surfaces the new copper as obstacles tagged
/// `connectedTo` the right connection.
#[test]
fn written_copper_appears_as_obstacles() {
    let (_keep, path) = fixture_copy();
    let board = read_problem(&path).unwrap();
    write_solution(&path, &hand_solution(), &board).expect("write_solution");

    let reread = read_problem(&path).unwrap();
    // The obstacle count GREW by the copper we added: 5 segments + 1 via = 6 new
    // obstacles on top of the original 4 pads.
    assert_eq!(
        reread.problem.obstacles.len(),
        board.problem.obstacles.len() + 6
    );

    let sig_copper = reread
        .problem
        .obstacles
        .iter()
        .filter(|o| o.connected_to == vec!["SIG".to_owned()])
        .count();
    let gnd_copper = reread
        .problem
        .obstacles
        .iter()
        .filter(|o| o.connected_to == vec!["GND".to_owned()])
        .count();
    // SIG: 2 pads + 3 segments. GND: 2 pads + 2 segments + 1 via.
    assert_eq!(sig_copper, 5);
    assert_eq!(gnd_copper, 5);
}

/// Determinism: writing the same solution from two fresh copies yields
/// byte-identical output (content-derived UUIDs, minimal number formatting).
#[test]
fn write_solution_is_deterministic() {
    let (_k1, p1) = fixture_copy();
    let (_k2, p2) = fixture_copy();
    let b1 = read_problem(&p1).unwrap();
    let b2 = read_problem(&p2).unwrap();
    write_solution(&p1, &hand_solution(), &b1).unwrap();
    write_solution(&p2, &hand_solution(), &b2).unwrap();
    assert_eq!(
        std::fs::read_to_string(&p1).unwrap(),
        std::fs::read_to_string(&p2).unwrap(),
        "identical input must yield byte-identical output"
    );
}

/// The connectivity oracle is the authority: parse the written copper back into
/// a solution and check it against the ORIGINAL problem — the hand-built routes
/// must really connect the pads (empty violation set).
#[test]
fn oracle_confirms_written_copper_connects_pads() {
    let (_keep, path) = fixture_copy();
    let original_board = read_problem(&path).unwrap();
    write_solution(&path, &hand_solution(), &original_board).expect("write_solution");

    // "Solution as parsed back": map the board's segments/vias to traces/vias
    // with connection names via the net codes.
    let parsed = extract_copper(&path).unwrap();
    let violations = drc_lint::connectivity::check(&original_board.problem, &parsed);
    assert!(
        violations.is_empty(),
        "hand-built routes must connect the pads, got {violations:?}"
    );
}

/// `kicad-cli pcb drc` smoke: routing SIG+GND must DECREASE the unconnected-item
/// count versus the unrouted fixture (lib_footprint_mismatch violations are
/// expected on a hand-authored fixture and ignored). Gated on a KiCAD install.
#[test]
fn drc_unconnected_count_decreases_after_routing() {
    use kicad_cli_rs::env::KicadEnv;
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD installation detected");
        return;
    };

    // Baseline DRC on the unrouted fixture copy.
    let (_keep, path) = fixture_copy();
    let before = drc_unconnected_count(&env, &path);

    let board = read_problem(&path).unwrap();
    write_solution(&path, &hand_solution(), &board).expect("write_solution");
    let after = drc_unconnected_count(&env, &path);

    eprintln!("drc unconnected items: before={before} after={after}");
    assert!(
        after <= before,
        "routing must not increase unconnected items (before={before}, after={after})"
    );
    // SIG + GND were the only two airwires; routing both should clear them.
    assert_eq!(after, 0, "routing SIG+GND should clear both airwires");
}

/// Run `kicad-cli pcb drc --format json` and return the `unconnected_items`
/// count. Panics on execution failure (the test is already gated on detect()).
fn drc_unconnected_count(env: &kicad_cli_rs::env::KicadEnv, board: &std::path::Path) -> usize {
    let out = tempfile::Builder::new()
        .prefix("gordian-drc-")
        .suffix(".json")
        .tempfile()
        .unwrap();
    let status = std::process::Command::new(&env.cli_path)
        .args(["pcb", "drc", "--format", "json"])
        .arg("--output")
        .arg(out.path())
        .arg(board)
        .output()
        .expect("run kicad-cli pcb drc");
    // drc exits nonzero when violations exist; the report is still written.
    let json = std::fs::read_to_string(out.path()).unwrap_or_else(|e| {
        panic!(
            "drc report unreadable ({e}); stderr: {}",
            String::from_utf8_lossy(&status.stderr)
        )
    });
    let v: serde_json::Value = serde_json::from_str(&json).expect("parse drc json");
    v.get("unconnected_items")
        .and_then(|u| u.as_array())
        .map(|a| a.len())
        .unwrap_or(0)
}
