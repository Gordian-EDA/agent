//! Connectivity milestone: two resistors whose pins carry the same net name
//! are electrically connected via net-name labels (no wires). The netlist
//! proving two nodes share the `SIG` net is the real test — it confirms the
//! label endpoints actually land on the pins. SKIP-graceful when no KiCAD is
//! detected; runs against KiCAD 10 here.

use kicad_cli_rs::cli::KicadCli;
use kicad_cli_rs::env::KicadEnv;
use sch_io::write::SchematicWriter;

#[test]
fn two_pins_same_net_name_are_electrically_connected() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };

    let mut w = SchematicWriter::new();
    // Two resistors; pin "1" of each on net SIG, pin "2" on GND.
    w.add_symbol(&env, "Device:R", "R1", "1k", [127.0, 63.5], 0.0)
        .unwrap();
    w.add_symbol(&env, "Device:R", "R2", "1k", [152.4, 63.5], 0.0)
        .unwrap();
    w.add_pin_label(&env, "R1", "1", "SIG").unwrap();
    w.add_pin_label(&env, "R2", "1", "SIG").unwrap();
    w.add_pin_label(&env, "R1", "2", "GND").unwrap();
    w.add_pin_label(&env, "R2", "2", "GND").unwrap();
    let text = w.finish();

    let tmp = tempfile::Builder::new()
        .suffix(".kicad_sch")
        .tempfile()
        .unwrap();
    std::fs::write(tmp.path(), &text).unwrap();

    // The netlist is the authoritative proof: SIG joins R1.1 and R2.1 (2 nodes),
    // connectivity established purely by the same-named labels.
    let nl = KicadCli::new(&env).netlist(tmp.path()).unwrap();
    let sig = nl
        .nets
        .iter()
        .find(|n| n.name.contains("SIG"))
        .expect("SIG net present in netlist");
    assert_eq!(sig.nodes.len(), 2, "SIG net must join exactly 2 pins");

    // No off-grid endpoint ERC violations: the labels land cleanly on the grid.
    let report = KicadCli::new(&env).erc(tmp.path()).unwrap();
    assert_eq!(
        report
            .violations
            .iter()
            .filter(|v| v.kind == "endpoint_off_grid")
            .count(),
        0,
        "expected zero off-grid endpoint violations: {:?}",
        report.violations
    );
}

/// A rotated symbol's pin endpoints still land on the correct net: R1 at angle
/// 0, R2 at 90, R3 at 270 all share `SIG` on pin 1 and `GND` on pin 2. This
/// exercises the rotation branch of the endpoint transform — the netlist
/// proving all three pin-1s share one net is what confirms the rotated
/// endpoints are computed correctly.
#[test]
fn rotated_symbols_pins_land_on_the_same_nets() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };

    let mut w = SchematicWriter::new();
    w.add_symbol(&env, "Device:R", "R1", "1k", [127.0, 63.5], 0.0)
        .unwrap();
    w.add_symbol(&env, "Device:R", "R2", "1k", [152.4, 63.5], 90.0)
        .unwrap();
    w.add_symbol(&env, "Device:R", "R3", "1k", [177.8, 63.5], 270.0)
        .unwrap();
    for r in ["R1", "R2", "R3"] {
        w.add_pin_label(&env, r, "1", "SIG").unwrap();
        w.add_pin_label(&env, r, "2", "GND").unwrap();
    }
    let text = w.finish();

    let tmp = tempfile::Builder::new()
        .suffix(".kicad_sch")
        .tempfile()
        .unwrap();
    std::fs::write(tmp.path(), &text).unwrap();

    let nl = KicadCli::new(&env).netlist(tmp.path()).unwrap();
    let sig = nl
        .nets
        .iter()
        .find(|n| n.name.contains("SIG"))
        .expect("SIG net present");
    assert_eq!(sig.nodes.len(), 3, "SIG must join all three rotated pin-1s");
    let gnd = nl
        .nets
        .iter()
        .find(|n| n.name.contains("GND"))
        .expect("GND net present");
    assert_eq!(gnd.nodes.len(), 3, "GND must join all three rotated pin-2s");

    let report = KicadCli::new(&env).erc(tmp.path()).unwrap();
    assert_eq!(
        report
            .violations
            .iter()
            .filter(|v| v.kind == "endpoint_off_grid")
            .count(),
        0,
        "rotated endpoints must stay on-grid: {:?}",
        report.violations
    );
}
