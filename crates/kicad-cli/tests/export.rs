//! SKIP-graceful smoke tests for the fabrication-export wrappers
//! ([`KicadCli::export_gerbers`] / `export_drill` / `export_pos`).
//!
//! Each detects a real `kicad-cli` via [`KicadEnv::detect`] and SKIPs (prints +
//! returns) when none is installed, so they only assert on a machine with KiCAD
//! on PATH. They drive a tiny vendored two-resistor board (shared with
//! `kicad-sexpr`) through the wrappers and assert real, non-empty deliverables
//! land on disk — the unit the higher-level `export_fab` tool depends on.

use std::path::PathBuf;

use kicad_cli::cli::KicadCli;
use kicad_cli::env::KicadEnv;

/// The vendored two-resistor board, embedded so the test is self-contained.
const TWO_RES: &str = include_str!("../../kicad-sexpr/tests/fixtures/two_res.kicad_pcb");

/// Write the fixture into a fresh tempdir and return `(tempdir, board_path)`.
/// The `TempDir` must stay alive for the board to exist.
fn fixture_board() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let board = dir.path().join("two_res.kicad_pcb");
    std::fs::write(&board, TWO_RES).expect("write board");
    (dir, board)
}

#[test]
fn export_gerbers_produces_layer_files() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no kicad-cli detected");
        return;
    };
    let cli = KicadCli::new(&env);
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
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no kicad-cli detected");
        return;
    };
    let cli = KicadCli::new(&env);
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
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no kicad-cli detected");
        return;
    };
    let cli = KicadCli::new(&env);
    let (dir, board) = fixture_board();
    let pos = dir.path().join("fab").join("two_res-pos.csv");

    let written = cli.export_pos(&board, &pos).expect("export pos");
    assert!(written.is_file(), "position file not written");
    let body = std::fs::read_to_string(&written).expect("read pos");
    assert!(!body.trim().is_empty(), "position CSV is empty");
}
