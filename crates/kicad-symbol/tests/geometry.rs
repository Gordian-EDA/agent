//! Integration tests for pin geometry + lib_symbols definition extraction.
//!
//! Gated on a real KiCAD install (`KicadInstallation::detect()` → SKIP-graceful); these
//! RUN on the dev machine where KiCAD 10 and its symbol libraries are present.

use kicad::KicadInstallation;
use kicad_symbol::geometry::SymbolGeometry;

#[test]
fn device_r_pin_geometry() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCAD install detected");
        return;
    };
    let g = SymbolGeometry::load(env.symbol_dir(), "Device:R").unwrap();
    assert_eq!(g.pins.len(), 2);
    // Device:R pins are vertical at x=0, y=±3.81, length 1.27 (per spike).
    let ys: Vec<f64> = g.pins.iter().map(|p| p.at.y).collect();
    assert!(ys.contains(&3.81) && ys.contains(&-3.81), "{ys:?}");
    assert!(g.pins.iter().all(|p| (p.length - 1.27).abs() < 1e-9));
    // Device:R is single-unit: every pin carries unit identity 1.
    assert!(
        g.pins.iter().all(|p| p.unit == 1),
        "single-unit symbol must report unit 1 for all pins: {:?}",
        g.pins.iter().map(|p| p.unit).collect::<Vec<_>>()
    );
}

#[test]
fn approx_size_scales_with_symbol() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCAD install detected");
        return;
    };
    let r = SymbolGeometry::load(env.symbol_dir(), "Device:R")
        .unwrap()
        .approx_size();
    assert!(r.y > r.x, "R is taller than wide: {r:?}");
    assert!(r.y <= 15.0, "passive stays small: {r:?}");
}

/// Multi-unit symbols (op-amps, logic gates) flatten every unit's pins into the
/// one `pins` Vec, so `pins.len()` overcounts a single placed unit. Each pin
/// must carry its `unit` so the writer can filter per-unit. `LM358` is a
/// dual op-amp: its pins span unit 1 and unit 2.
#[test]
fn multi_unit_symbol_carries_unit_identity() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCAD install detected");
        return;
    };
    let g = SymbolGeometry::load(env.symbol_dir(), "Amplifier_Operational:LM358").unwrap();
    let max_unit = g.pins.iter().map(|p| p.unit).max().unwrap_or(0);
    assert!(
        max_unit >= 2,
        "LM358 is multi-unit; pins must span at least units 1 and 2, got max {max_unit} from {:?}",
        g.pins
            .iter()
            .map(|p| (p.number.clone(), p.unit))
            .collect::<Vec<_>>()
    );
    // Every pin must carry a real (1-based) unit, never 0.
    assert!(
        g.pins.iter().all(|p| p.unit >= 1),
        "all pins must carry a 1-based unit"
    );
}

#[test]
fn split_symbol_dir_geometry_resolves_extends() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("Device.kicad_symdir");
    std::fs::create_dir(&lib).unwrap();
    std::fs::write(
        lib.join("Base.kicad_sym"),
        r#"(kicad_symbol_lib
	(version 20251024)
	(generator "test")
	(symbol "Base"
		(symbol "Base_1_1"
			(pin passive line (at 0 3.81 270) (length 1.27)
				(name "A" (effects (font (size 1.27 1.27))))
				(number "1" (effects (font (size 1.27 1.27)))))
		)
	)
)
"#,
    )
    .unwrap();
    std::fs::write(
        lib.join("Alias.kicad_sym"),
        r#"(kicad_symbol_lib
	(version 20251024)
	(generator "test")
	(symbol "Alias"
		(extends "Base")
	)
)
"#,
    )
    .unwrap();

    let g = SymbolGeometry::load(dir.path(), "Device:Alias").unwrap();

    assert_eq!(g.pins.len(), 1);
    assert_eq!(g.pins[0].number, "1");
    assert_eq!(g.pins[0].name, "A");
    assert!(g.definition_sexpr().contains("\"Device:Alias\""));
    assert!(!g.definition_sexpr().contains("(extends"));
    assert!(!g.definition_sexpr().contains("(symbol \"Base_"));
}

#[test]
fn derived_definition_overlays_child_properties_across_chain() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Parts.kicad_sym"),
        r#"(kicad_symbol_lib
	(version 20231120)
	(generator "test")
	(symbol "Base"
		(property "Value" "BASE-1V5" (at 0 0 0))
		(property "Footprint" "Package:SOT-23-5" (at 0 0 0))
		(property "Datasheet" "https://example.test/base.pdf" (at 0 0 0))
		(property "Description" "150mA base regulator" (at 0 0 0))
		(symbol "Base_1_1"
			(pin power_in line (at 0 0 0) (length 2.54)
				(name "VIN" (effects (font (size 1.27 1.27))))
				(number "1" (effects (font (size 1.27 1.27)))))
		)
	)
	(symbol "Mid"
		(extends "Base")
		(property "Description" "400mA intermediate regulator" (at 0 0 0))
	)
	(symbol "Child"
		(extends "Mid")
		(property "Value" "CHILD-3V3" (at 0 0 0))
		(property "Datasheet" "https://example.test/child.pdf" (at 0 0 0))
		(property "Manufacturer" "Example Semi" (at 0 0 0))
	)
)
"#,
    )
    .unwrap();

    let g = SymbolGeometry::load(dir.path(), "Parts:Child").unwrap();
    let def = g.definition_sexpr();

    assert_eq!(g.pins.len(), 1);
    assert!(def.contains("\"Parts:Child\""));
    assert!(def.contains("\"CHILD-3V3\""), "{def}");
    assert!(def.contains("https://example.test/child.pdf"), "{def}");
    assert!(def.contains("400mA intermediate regulator"), "{def}");
    assert!(def.contains("\"Package:SOT-23-5\""), "{def}");
    assert!(def.contains("\"Manufacturer\" \"Example Semi\""), "{def}");
    assert!(!def.contains("BASE-1V5"), "{def}");
    assert!(!def.contains("150mA base regulator"), "{def}");
    assert!(!def.contains("example.test/base.pdf"), "{def}");
    assert!(!def.contains("(extends"), "{def}");
    assert!(!def.contains("(symbol \"Base_"), "{def}");
    assert_eq!(def.matches('(').count(), def.matches(')').count());
}

#[test]
fn installed_derived_definitions_match_child_library_properties() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCAD install detected");
        return;
    };
    let g = SymbolGeometry::load(env.symbol_dir(), "Regulator_Linear:AP2112K-3.3").unwrap();
    let def = g.definition_sexpr();

    assert!(def.contains("\"Value\" \"AP2112K-3.3\""), "{def}");
    assert!(def.contains("Datasheets/AP2112.pdf"), "{def}");
    assert!(def.contains("600mA low dropout linear regulator"), "{def}");
    assert!(!def.contains("AP2204K-1.5"), "{def}");
    assert!(!def.contains("Datasheets/AP2204.pdf"), "{def}");
    assert!(!def.contains("150mA low dropout linear regulator"), "{def}");

    // KiCad's own library parity check is the acceptance gate: the flattened
    // cached copy must compare equal to the installed derived symbol.
    let sch = build_schematic("Regulator_Linear:AP2112K-3.3", def, &g);
    let tmp = tempfile::Builder::new()
        .suffix(".kicad_sch")
        .tempfile()
        .unwrap();
    std::fs::write(tmp.path(), sch).unwrap();
    let erc = env.erc(tmp.path()).unwrap();
    let mismatches: Vec<_> = erc
        .violations
        .iter()
        .filter(|v| v.kind == "lib_symbol_mismatch")
        .collect();
    assert!(mismatches.is_empty(), "{mismatches:#?}");

    let g = SymbolGeometry::load(env.symbol_dir(), "Power_Protection:USBLC6-2SC6").unwrap();
    let def = g.definition_sexpr();
    assert!(def.contains("\"Value\" \"USBLC6-2SC6\""), "{def}");
    assert!(def.contains("Package_TO_SOT_SMD:SOT-23-6"), "{def}");
    assert!(def.contains("2 data-line, SOT-23-6"), "{def}");
    assert!(!def.contains("USBLC6-2P6"), "{def}");
    assert!(!def.contains("Package_TO_SOT_SMD:SOT-666"), "{def}");

    let sch = build_schematic("Power_Protection:USBLC6-2SC6", def, &g);
    std::fs::write(tmp.path(), sch).unwrap();
    let erc = env.erc(tmp.path()).unwrap();
    let mismatches: Vec<_> = erc
        .violations
        .iter()
        .filter(|v| v.kind == "lib_symbol_mismatch")
        .collect();
    assert!(mismatches.is_empty(), "{mismatches:#?}");
}

#[test]
fn lib_symbols_definition_is_embeddable() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCAD install detected");
        return;
    };
    let g = SymbolGeometry::load(env.symbol_dir(), "Device:R").unwrap();
    // The raw (symbol "Device:R" ...) block, ready to splice into (lib_symbols).
    let def = g.definition_sexpr();
    assert!(def.trim_start().starts_with("(symbol"));
    assert!(def.contains("\"Device:R\"") || def.contains("Device:R"));
    // Balanced parens.
    let opens = def.matches('(').count();
    let closes = def.matches(')').count();
    assert_eq!(opens, closes, "unbalanced lib_symbols block");
}

/// A derived symbol (`Device:Filter_EMI_C` `(extends "C_Feedthrough")`) carries
/// no body of its own; we must inline the parent's pins. The geometry must
/// surface the parent's pins, and the embedded definition must drive KiCAD to
/// resolve those pins in the netlist — the verbatim `(extends)` form yields
/// zero nodes, so this guards the flattening contract end-to-end.
#[test]
fn derived_symbol_inlines_parent_body() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCAD install detected");
        return;
    };
    let g = SymbolGeometry::load(env.symbol_dir(), "Device:Filter_EMI_C").unwrap();
    // C_Feedthrough has three pins; the derived symbol inherits all of them.
    assert_eq!(g.pins.len(), 3, "{:?}", g.pins);

    let def = g.definition_sexpr();
    assert!(def.trim_start().starts_with("(symbol"));
    assert!(
        def.contains("\"Device:Filter_EMI_C\""),
        "top-level name must be the fully-qualified derived id"
    );
    // No leftover (extends) clause — the parent body is inlined.
    assert!(
        !def.contains("(extends"),
        "derived definition must inline the parent, not keep (extends …)"
    );
    // Nested unit blocks are re-prefixed to the derived bare name.
    assert!(
        !def.contains("(symbol \"C_Feedthrough_"),
        "nested sub-blocks must be re-prefixed to the derived name"
    );

    // The real gate: embed the definition in a schematic and confirm KiCAD's
    // netlist resolves the inherited pins (3 nodes across 3 labels).
    let sch = build_schematic("Device:Filter_EMI_C", def, &g);
    let tmp = tempfile::Builder::new()
        .suffix(".kicad_sch")
        .tempfile()
        .unwrap();
    std::fs::write(tmp.path(), &sch).unwrap();
    let nl = env.netlist(tmp.path()).unwrap();
    assert_eq!(nl.components.len(), 1, "one component");
    let total_nodes: usize = nl.nets.iter().map(|n| n.nodes.len()).sum();
    assert_eq!(
        total_nodes, 3,
        "all three inherited pins must resolve in the netlist"
    );
}

/// Assemble a minimal one-symbol schematic that places `def` in `(lib_symbols)`
/// and a distinct label on each pin endpoint so every pin becomes a netlist
/// node. Endpoint = local `at` + `length` projected along `angle` (symbol Y is
/// flipped into schematic Y-down space at instance position 127,63.5).
fn build_schematic(lib_id: &str, def: &str, g: &SymbolGeometry) -> String {
    let (ix, iy) = (127.0_f64, 63.5_f64);
    let mut labels = String::new();
    for (i, p) in g.pins.iter().enumerate() {
        let rad = p.angle.to_radians();
        let tipx = p.at.x + p.length * rad.cos();
        let tipy = p.at.y + p.length * rad.sin();
        let (ex, ey) = (ix + tipx, iy - tipy);
        labels.push_str(&format!(
            "\t(label \"N{i}\" (at {ex} {ey} 0) (effects (font (size 1.27 1.27))) (uuid \"cccccccc-0000-4000-8000-00000000000{i}\"))\n"
        ));
    }
    format!(
        "(kicad_sch\n\t(version 20250114)\n\t(generator \"gordian-test\")\n\t(generator_version \"0.1\")\n\t(uuid \"aaaaaaaa-0000-4000-8000-000000000099\")\n\t(paper \"A4\")\n\t(lib_symbols\n\t\t{def}\n\t)\n{labels}\t(symbol (lib_id \"{lib_id}\") (at {ix} {iy} 0) (unit 1) (exclude_from_sim no) (in_bom yes) (on_board yes) (dnp no) (uuid \"bbbbbbbb-0000-4000-8000-000000000099\")\n\t\t(property \"Reference\" \"FB1\" (at 130 62 0) (effects (font (size 1.27 1.27))))\n\t\t(property \"Value\" \"X\" (at 130 65 0) (effects (font (size 1.27 1.27))))\n\t\t(instances (project \"t\" (path \"/aaaaaaaa-0000-4000-8000-000000000099\" (reference \"FB1\") (unit 1)))))\n\t(sheet_instances (path \"/\" (page \"1\"))))\n"
    )
}
