//! Integration tests for `KicadCli::netlist` against real `kicad-cli`.

use std::path::Path;

use kicad_cli::{cli::KicadCli, env::KicadEnv};

#[test]
fn netlist_export_runs_and_parses() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD environment");
        return;
    };
    let cli = KicadCli::new(&env);
    let nl = cli
        .netlist(Path::new("tests/fixtures/blank.kicad_sch"))
        .unwrap();
    assert!(nl.components.is_empty() && nl.nets.is_empty());
}

#[test]
fn netlist_export_parses_real_components_and_nets() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD environment");
        return;
    };
    let cli = KicadCli::new(&env);
    let nl = cli
        .netlist(Path::new("tests/fixtures/rc_pair.kicad_sch"))
        .unwrap();

    assert_eq!(nl.components.len(), 2);
    let r = nl
        .components
        .iter()
        .find(|c| c.reference == "R1")
        .expect("R1 present");
    assert_eq!(r.value, "10k");
    assert_eq!(r.lib_id, "Device:R");
    let c = nl
        .components
        .iter()
        .find(|c| c.reference == "C1")
        .expect("C1 present");
    assert_eq!(c.value, "100nF");
    assert_eq!(c.lib_id, "Device:C");
    assert_eq!(
        c.properties.get("Footprint").map(String::as_str),
        Some("Capacitor_SMD:C_0603_1608Metric")
    );

    let shared = nl
        .nets
        .iter()
        .find(|n| n.nodes.len() == 2)
        .expect("a 2-node net exists");
    let mut nodes = shared.nodes.clone();
    nodes.sort();
    assert_eq!(
        nodes,
        vec![
            ("C1".to_string(), "2".to_string()),
            ("R1".to_string(), "2".to_string()),
        ]
    );

    assert!(
        nl.nets
            .iter()
            .any(|n| n.name == "/VIN" && n.nodes.len() == 1)
    );
    assert!(
        nl.nets
            .iter()
            .any(|n| n.name == "/VOUT" && n.nodes.len() == 1)
    );
}
