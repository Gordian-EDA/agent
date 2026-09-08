//! A board identity is derived from a sheet's content, not its path, so a board
//! already routed can be kept when a later build only re-draws the same circuit —
//! and must be thrown away when the wiring actually moved.
//!
//! Skipped unless `GORDIAN_TEST_SCH` names a sheet; the second half also needs
//! `GORDIAN_TEST_SCH2` naming a sheet of a DIFFERENT netlist.

use std::path::PathBuf;

use gordian_core::board;
use kicad::KicadInstallation;

fn kicad() -> Option<KicadInstallation> {
    let config = gordian_core::platform::load_config().ok()?;
    KicadInstallation::detect_with(
        config.kicad.symbol_dir.as_deref(),
        config.kicad.footprint_dir.as_deref(),
        config.kicad.cli_path.as_deref(),
    )
    .ok()
}

fn sheet(key: &str) -> Option<PathBuf> {
    std::env::var(key)
        .ok()
        .map(PathBuf::from)
        .filter(|p| p.is_file())
}

/// The identity follows the circuit, so the same sheet delivered from a different
/// directory is the same board — which is what makes keeping a routed board safe.
#[test]
fn the_identity_is_the_circuit_not_the_path() {
    let (Some(kicad), Some(first)) = (kicad(), sheet("GORDIAN_TEST_SCH")) else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let elsewhere = dir.path().join("delivered.kicad_sch");
    std::fs::copy(&first, &elsewhere).unwrap();

    let here = board::identity(&kicad, &first).expect("the sheet exports a netlist");
    let there = board::identity(&kicad, &elsewhere).expect("the copy exports a netlist");

    assert_eq!(here, there);
}

/// A sheet whose wiring moved is a different board, however similar it looks.
#[test]
fn a_rewired_sheet_is_a_different_board() {
    let (Some(kicad), Some(first), Some(second)) = (
        kicad(),
        sheet("GORDIAN_TEST_SCH"),
        sheet("GORDIAN_TEST_SCH2"),
    ) else {
        return;
    };

    let one = board::identity(&kicad, &first).expect("the first sheet exports a netlist");
    let two = board::identity(&kicad, &second).expect("the second sheet exports a netlist");

    assert_ne!(one, two);
}
