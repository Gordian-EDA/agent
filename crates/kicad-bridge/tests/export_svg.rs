//! `kicad-cli sch export svg` wrapper — exercised against the checked-in
//! bluepill fixture. SKIPs when no KiCAD environment is detected.

use kicad_bridge::cli::KicadCli;
use kicad_bridge::env::KicadEnv;
use std::path::Path;

#[test]
fn exports_svg_for_bluepill_fixture() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let sch = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/validation/bluepill.kicad_sch");
    assert!(sch.is_file(), "fixture missing: {}", sch.display());

    let out = tempfile::tempdir().unwrap();
    let svg_path = KicadCli::new(&env)
        .export_svg(&sch, out.path())
        .expect("svg export");

    let svg = std::fs::read_to_string(&svg_path).unwrap();
    assert!(svg.contains("<svg"), "not an SVG: {}", svg_path.display());
    assert!(svg.len() > 1000, "suspiciously small SVG ({} bytes)", svg.len());
}
