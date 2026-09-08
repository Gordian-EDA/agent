//! The render path against the real KiCad CLI: a sheet exports, rasterises, and
//! the gridded copy is a different picture of the same page.

use std::path::{Path, PathBuf};

use gordian_core::render;

/// The configured KiCad CLI, or `None` on a machine without KiCad 10.
fn kicad_cli() -> Option<PathBuf> {
    let config = gordian_core::platform::load_config().ok()?;
    config.kicad.cli_path.filter(|p| p.is_file())
}

fn a_schematic() -> Option<PathBuf> {
    std::env::var("GORDIAN_TEST_SCH")
        .ok()
        .map(PathBuf::from)
        .filter(|p| p.is_file())
}

#[test]
fn a_sheet_renders_plain_and_gridded() {
    let (Some(cli), Some(sch)) = (kicad_cli(), a_schematic()) else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let clean = dir.path().join("sheet.png");
    let grid = dir.path().join("sheet_grid.png");

    let sheet = render::sheet(&cli, &sch, &clean, &grid).expect("the sheet renders");

    assert!(sheet.clean.starts_with(&[0x89, b'P', b'N', b'G']));
    assert!(sheet.grid.len() > sheet.clean.len(), "the grid adds ink");
    assert!(Path::new(&clean).is_file() && Path::new(&grid).is_file());
}
