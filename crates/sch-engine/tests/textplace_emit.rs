//! Integration tests for solved text positions in emitted documents.

use kicad_bridge::env::KicadEnv;
use sch_engine::emit::SchematicWriter;

fn detect_env() -> Option<KicadEnv> {
    match KicadEnv::detect() {
        Some(env) => Some(env),
        None => {
            eprintln!("SKIP: no KiCAD environment detected");
            None
        }
    }
}

/// Two resistors placed so close horizontally that R1's legacy right-of-body
/// fields would sit inside R2's body. The solver must move R1's fields
/// elsewhere (any non-right candidate), so the legacy Reference position
/// must NOT appear in the output.
#[test]
fn fields_dodge_neighbor_body() {
    let Some(env) = detect_env() else { return };
    let mut w = SchematicWriter::new();
    // Device:R approx size [10.16, 12.7] -> half extents [5.08, 6.35]. Both
    // positions are on the 1.27 grid (no snap drift). Legacy ref position for
    // R1 at (101.6, 101.6): x = 101.6+5.08+1.27 = 107.95. R2 at (111.76,
    // 101.6): body spans x in [106.68, 116.84] -> covers 107.95.
    w.add_symbol(&env, "Device:R", "R1", "1k", [101.6, 101.6], 0.0).unwrap();
    w.add_symbol(&env, "Device:R", "R2", "2k", [111.76, 101.6], 0.0).unwrap();
    let sch = w.finish();
    let r1_prop = sch
        .split("(property \"Reference\" \"R1\"")
        .nth(1)
        .expect("R1 Reference property present");
    assert!(
        !r1_prop.trim_start().starts_with("(at 107.95"),
        "R1 Reference must move off the legacy right-of-body spot:\n{sch}"
    );
}

/// With no neighbors, the first (conventional) candidate is chosen and the
/// output keeps the legacy right-of-body field placement byte-for-byte.
#[test]
fn lone_symbol_keeps_conventional_fields() {
    let Some(env) = detect_env() else { return };
    let mut w = SchematicWriter::new();
    w.add_symbol(&env, "Device:R", "R1", "1k", [101.6, 101.6], 0.0).unwrap();
    let sch = w.finish();
    assert!(
        sch.contains("(property \"Reference\" \"R1\"\n\t\t\t(at 107.95 100.33 0)"),
        "lone symbol keeps conventional field spot:\n{sch}"
    );
}
