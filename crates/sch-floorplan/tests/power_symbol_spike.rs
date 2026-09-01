//! SPIKE (spec "day-one"): KiCAD power symbols drive nets by their Value.
//! If this fails, STOP and re-design Phase 2's power-symbol approach.

use kicad::KicadInstallation;
use sch_floorplan::write::SchematicWriter;

#[test]
fn power_symbol_value_names_the_net() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };

    let mut w = SchematicWriter::new();
    // R1 vertical at (127, 63.5): pin 1 endpoint (127, 59.69), pin 2 (127, 67.31).
    w.add_symbol(&env, "Device:R", "R1", "1k", [127.0, 63.5], 0.0)
        .unwrap();
    // Stock GND at R1 pin 2 (graphic extends down; connection point = origin).
    w.add_power_symbol(&env, "power:GND", "#PWR01", "GND", [127.0, 67.31], 0.0)
        .unwrap();
    // Donor power:VCC renamed to a custom rail at R1 pin 1.
    w.add_power_symbol(
        &env,
        "power:VCC",
        "#PWR02",
        "RAIL_CUSTOM",
        [127.0, 59.69],
        0.0,
    )
    .unwrap();
    // PWR_FLAG pin-coincident with the GND attach point (label-free attachment).
    w.add_power_flag_at(&env, "#FLG01", [127.0, 67.31], 0.0)
        .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let sch = tmp.path().join("spike.kicad_sch");
    std::fs::write(&sch, w.finish()).unwrap();

    let nl = env.netlist(&sch).expect("netlist");

    eprintln!("--- netlist components ---");
    for c in &nl.components {
        eprintln!("  {:?}", c);
    }
    eprintln!("--- netlist nets ---");
    for n in &nl.nets {
        eprintln!("  net {:?}: {:?}", n.name, n.nodes);
    }

    // (c) hidden refs are netlist-excluded.
    assert!(nl.components.iter().all(|c| !c.reference.starts_with('#')));
    assert_eq!(nl.components.len(), 1, "only R1: {:?}", nl.components);

    // (a)+(b) net names come from the power symbols' Values, globally (no '/').
    let net_of = |refdes: &str, pin: &str| {
        nl.nets
            .iter()
            .find(|n| n.nodes.contains(&(refdes.to_string(), pin.to_string())))
            .map(|n| n.name.clone())
            .unwrap_or_default()
    };
    assert_eq!(
        net_of("R1", "2"),
        "GND",
        "R1 pin 2 should be on the GND net"
    );
    assert_eq!(
        net_of("R1", "1"),
        "RAIL_CUSTOM",
        "R1 pin 1 should be on the RAIL_CUSTOM net (Value-rename)"
    );

    // (d) the coincident PWR_FLAG drives GND -> zero ERC errors on that net.
    let erc = env.erc(&sch).expect("erc");

    eprintln!("--- ERC violations ---");
    for v in &erc.violations {
        eprintln!("  [{:?}] {:?}: {:?}", v.severity, v.kind, v.description);
        for item in &v.items {
            eprintln!("    item: {:?}", item.description);
        }
    }

    let gnd_errors: Vec<_> = erc
        .violations
        .iter()
        .filter(|v| v.severity == "error" && v.kind == "power_pin_not_driven")
        .filter(|v| v.items.iter().any(|i| i.description.contains("GND")))
        .collect();
    assert!(
        gnd_errors.is_empty(),
        "GND undriven despite PWR_FLAG: {gnd_errors:?}"
    );
}
