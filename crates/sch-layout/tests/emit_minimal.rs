//! First emission milestone: a `.kicad_sch` with one placed symbol that KiCAD
//! loads. SKIP-graceful when no KiCAD is detected; runs against KiCAD 10 here.

use kicad_cli_rs::cli::KicadCli;
use kicad_cli_rs::env::KicadEnv;
use sch_layout::emit::SchematicWriter;

#[test]
fn emits_single_symbol_that_kicad_loads() {
    let Some(env) = KicadEnv::detect() else {
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
    let report = KicadCli::new(&env).erc(tmp.path()).unwrap();

    // The netlist is the authoritative proof the component is present.
    let nl = KicadCli::new(&env).netlist(tmp.path()).unwrap();
    assert_eq!(nl.components.len(), 1);
    assert_eq!(nl.components[0].reference, "R1");
    let _ = report;
}
