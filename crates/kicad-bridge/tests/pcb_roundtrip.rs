//! Read-side tests for `kicad_bridge::pcb`: the hand-authored `two_res`
//! fixture must parse cleanly and translate into the expected routing problem.
//! (The write side extends this file in the next task.)

use std::path::PathBuf;

use kicad_bridge::pcb::read_problem;
use pcb_engine::problem::LayerRef;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/two_res.kicad_pcb")
}

const EPS: f64 = 1e-6;

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
