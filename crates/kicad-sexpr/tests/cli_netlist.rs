//! Integration tests for `KicadCli::netlist` against real `kicad-cli` 10.0.3.
//!
//! These run only when KiCAD is detected (otherwise they SKIP-gracefully, like
//! the ERC tests), and exercise the netlist exporter — the connectivity oracle
//! that Plan 3's "lift" (sch -> kernel YAML) consumes.

use kicad_cli_rs::{cli::KicadCli, env::KicadEnv};
use std::path::Path;

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
    assert!(nl.components.is_empty() && nl.nets.is_empty()); // blank sheet
}

/// A blank sheet can't prove the parser actually reads components and nets, so
/// this asserts on a fixture with real content: a resistor and a capacitor with
/// a wire joining their pin-2s (a shared net) and a named label on each pin-1.
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

    // Two components, with reconstructed lib_id and values.
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
    // Properties are captured (footprint is carried as a property).
    assert_eq!(
        c.properties.get("Footprint").map(String::as_str),
        Some("Capacitor_SMD:C_0603_1608Metric")
    );

    // The shared net joins R1.2 and C1.2 (the only 2-node net here).
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

    // The two label nets each have exactly one node.
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
