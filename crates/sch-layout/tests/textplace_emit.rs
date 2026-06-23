//! Integration tests for solved text positions in emitted documents.

use kicad_cli_rs::env::KicadEnv;
use sch_layout::emit::SchematicWriter;

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

/// A 555's west-side trigger pin carries a signal label on a stub. The solver
/// must keep it at the stub end (its first candidate) — regression guard
/// against over-aggressive obstacle modeling force-retracting everything.
#[test]
fn timer_pin_labels_stay_on_stub_ends() {
    let Some(env) = detect_env() else { return };
    let mut w = SchematicWriter::new();
    w.add_symbol(&env, "Timer:NE555P", "U1", "NE555P", [152.4, 101.6], 0.0).unwrap();
    w.add_signal_label(&env, "U1", "2", "N_TR").unwrap();
    let sch = w.finish();
    // Pin 2 is on the west side; its stub extends west 3.81mm and the label
    // must stay there (dir West renders angle 180).
    assert!(
        sch.contains("(label \"N_TR\"") && sch.contains(" 180)"),
        "west-side stub label survives, reading west:\n{sch}"
    );
}

/// Two power symbols close together: their Value texts (rail names) must not
/// land in overlapping boxes (the VCCDVCC3V3 artifact). Values are solver-
/// moved fields, so assert the rendered boxes are disjoint.
#[test]
fn adjacent_power_rail_values_do_not_merge() {
    let Some(env) = detect_env() else { return };
    let mut w = SchematicWriter::new();
    w.add_power_symbol(&env, "power:VCC", "#PWR01", "VCCD", [101.6, 101.6], 0.0).unwrap();
    w.add_power_symbol(&env, "power:VCC", "#PWR02", "VCC3V3", [106.68, 101.6], 0.0).unwrap();
    let sch = w.finish();
    let pos = |val: &str| -> (f64, f64) {
        let seg = sch
            .split(&format!("(property \"Value\" \"{val}\""))
            .nth(1)
            .unwrap();
        let at = seg.split("(at ").next().map(|_| seg).unwrap();
        let at = at.split("(at ").nth(1).unwrap();
        let mut it = at.split_whitespace();
        (
            it.next().unwrap().parse().unwrap(),
            it.next().unwrap().parse().unwrap(),
        )
    };
    let (x1, y1) = pos("VCCD");
    let (x2, y2) = pos("VCC3V3");
    // Conservative center-justified extents: width 1.1/char, height 1.6,
    // bottom-anchored. Disjoint if x-ranges or y-ranges are.
    let w1 = 4.0 * 1.1;
    let w2 = 6.0 * 1.1;
    let overlap_x = (x1 - w1 / 2.0) < (x2 + w2 / 2.0) && (x2 - w2 / 2.0) < (x1 + w1 / 2.0);
    let overlap_y = (y1 - 1.6) < y2 && (y2 - 1.6) < y1;
    assert!(
        !(overlap_x && overlap_y),
        "power rail values must not overlap: VCCD at ({x1}, {y1}), VCC3V3 at ({x2}, {y2})\n{sch}"
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
