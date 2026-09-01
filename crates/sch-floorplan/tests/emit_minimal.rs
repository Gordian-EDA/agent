//! First emission milestone: a `.kicad_sch` with one placed symbol that KiCAD
//! loads. SKIP-graceful when no KiCAD is detected; runs against KiCAD 10 here.

use kicad::KicadInstallation;
use sch_floorplan::write::SchematicWriter;

#[test]
fn emits_single_symbol_that_kicad_loads() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };

    let mut w = SchematicWriter::new();
    w.add_symbol(&env, "Device:R", "R1", "1k", [127.0, 63.5], 0.0)
        .unwrap();
    let text = w.finish();

    // 1) Well-formedness: kiutils parses it.
    let tmp = tempfile::Builder::new()
        .suffix(".kicad_sch")
        .tempfile()
        .unwrap();
    std::fs::write(tmp.path(), &text).unwrap();
    kiutils_kicad::SchematicFile::read(tmp.path()).expect("kiutils must parse our output");

    // 2) KiCAD loads + ERC runs (single unconnected R: connectivity warnings are
    //    fine, but the schematic must LOAD for erc() to return Ok).
    let report = env.erc(tmp.path()).unwrap();

    // The netlist is the authoritative proof the component is present.
    let nl = env.netlist(tmp.path()).unwrap();
    assert_eq!(nl.components.len(), 1);
    assert_eq!(nl.components[0].reference, "R1");
    let _ = report;
}

#[test]
fn mounting_hole_instances_are_excluded_from_the_bom() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };

    let mut writer = SchematicWriter::new();
    writer
        .add_symbol(
            &env,
            "Mechanical:MountingHole",
            "MH1",
            "",
            [50.8, 50.8],
            0.0,
        )
        .unwrap();
    writer
        .add_symbol(&env, "Device:R", "R1", "1k", [76.2, 50.8], 0.0)
        .unwrap();
    let text = writer.finish();

    let mounting_hole = &text[text
        .find("\t\t(lib_id \"Mechanical:MountingHole\")")
        .unwrap()..];
    assert!(mounting_hole[..mounting_hole.find("\t)\n").unwrap()].contains("\t\t(in_bom no)"));
    let resistor = &text[text.find("\t\t(lib_id \"Device:R\")").unwrap()..];
    assert!(resistor[..resistor.find("\t)\n").unwrap()].contains("\t\t(in_bom yes)"));
}
