//! SKIP-graceful smoke tests for the fabrication-export wrappers
//! ([`export_gerbers`] / `export_drill` / `export_pos`).
//!
//! Each detects a real `kicad-cli` via [`KicadInstallation::detect`] and SKIPs (prints +
//! returns) when none is installed, so they only assert on a machine with KiCAD
//! on PATH. They drive a tiny vendored two-resistor board through the wrappers
//! and assert real, non-empty deliverables land on disk — the unit the
//! higher-level `export_fab` tool depends on.

use std::path::PathBuf;

use kicad::KicadInstallation;

/// The vendored two-resistor board, embedded so the test is self-contained.
const TWO_RES: &str = include_str!("fixtures/two_res.kicad_pcb");

/// Write the fixture into a fresh tempdir and return `(tempdir, board_path)`.
/// The `TempDir` must stay alive for the board to exist.
fn fixture_board() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let board = dir.path().join("two_res.kicad_pcb");
    std::fs::write(&board, TWO_RES).expect("write board");
    (dir, board)
}

#[test]
fn export_pcb_svg_produces_single_board_plot() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no kicad detected");
        return;
    };
    let cli = env;
    let (dir, board) = fixture_board();
    let svg = dir.path().join("board.svg");

    let written = cli
        .export_pcb_svg(&board, &svg, "F.Cu,B.Cu,F.SilkS,Edge.Cuts", false)
        .expect("export PCB SVG");
    assert!(written.is_file(), "PCB SVG not written");
    let body = std::fs::read_to_string(&written).expect("read PCB SVG");
    assert!(body.contains("<svg"), "PCB SVG has no root element");
    assert!(body.len() > 100, "PCB SVG is suspiciously small");
}

#[test]
fn export_gerbers_produces_layer_files() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no kicad detected");
        return;
    };
    let cli = env;
    let (dir, board) = fixture_board();
    let out = dir.path().join("fab");

    let gerbers = cli.export_gerbers(&board, &out).expect("export gerbers");
    assert!(!gerbers.is_empty(), "expected at least one Gerber file");
    for g in &gerbers {
        assert_eq!(g.extension().and_then(|e| e.to_str()), Some("gbr"));
        let len = std::fs::metadata(g).expect("gerber metadata").len();
        assert!(len > 0, "Gerber {} is empty", g.display());
    }
}

#[test]
fn export_drill_produces_excellon_file() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no kicad detected");
        return;
    };
    let cli = env;
    let (dir, board) = fixture_board();
    let out = dir.path().join("fab");

    let drills = cli.export_drill(&board, &out).expect("export drill");
    assert!(!drills.is_empty(), "expected at least one drill file");
    for d in &drills {
        assert_eq!(d.extension().and_then(|e| e.to_str()), Some("drl"));
        let len = std::fs::metadata(d).expect("drill metadata").len();
        assert!(len > 0, "drill file {} is empty", d.display());
    }
}

#[test]
fn export_pos_produces_csv() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no kicad detected");
        return;
    };
    let cli = env;
    let (dir, board) = fixture_board();
    let pos = dir.path().join("fab").join("two_res-pos.csv");

    let written = cli.export_pos(&board, &pos).expect("export pos");
    assert!(written.is_file(), "position file not written");
    let body = std::fs::read_to_string(&written).expect("read pos");
    assert!(!body.trim().is_empty(), "position CSV is empty");
}

#[test]
fn refill_zones_can_persist_the_fill_cache() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no kicad detected");
        return;
    };
    let (_dir, board) = fixture_board();
    let mut body = std::fs::read_to_string(&board).expect("read board");
    assert!(body.ends_with(")\n"));
    body.truncate(body.len() - 2);
    body.push_str(
        "\t(zone\n\
         \t\t(net 1)\n\
         \t\t(net_name \"GND\")\n\
         \t\t(layer \"F.Cu\")\n\
         \t\t(uuid \"00000000-0000-0000-0000-000000000030\")\n\
         \t\t(hatch edge 0.5)\n\
         \t\t(connect_pads (clearance 0.2))\n\
         \t\t(min_thickness 0.25)\n\
         \t\t(fill yes (thermal_gap 0.3) (thermal_bridge_width 0.3))\n\
         \t\t(polygon\n\
         \t\t\t(pts (xy 1 1) (xy 29 1) (xy 29 19) (xy 1 19))\n\
         \t\t)\n\
         \t)\n\
         )\n",
    );
    std::fs::write(&board, body).expect("write zoned board");

    env.refill_zones(&board, true)
        .expect("refill and save board");

    let saved = std::fs::read_to_string(&board).expect("read refilled board");
    assert!(
        saved.contains("(filled_polygon"),
        "saved board did not retain KiCad's zone fill cache"
    );
}
