//! Pin numbers win globally over pin names during floorplan emission.

use std::collections::BTreeMap;
use std::path::Path;

use kicad::KicadInstallation;
use kicad_symbol::SymbolTable;
use sch_check::model::{Block, Component, Design, PinTarget};
use sch_doc::SchDoc;
use sch_floorplan::{floorplan, live};

const SYMBOL_LIBRARY: &str = r#"(kicad_symbol_lib
	(version 20231120)
	(generator "pin-shadow-test")
	(symbol "ThreePin"
		(property "Reference" "U" (at 0 6.35 0)
			(effects (font (size 1.27 1.27))))
		(property "Value" "ThreePin" (at 0 3.81 0)
			(effects (font (size 1.27 1.27))))
		(property "Footprint" "" (at 0 0 0)
			(effects (font (size 1.27 1.27)) hide))
		(property "Datasheet" "" (at 0 0 0)
			(effects (font (size 1.27 1.27)) hide))
		(property "Description" "Pin shadow regression fixture" (at 0 0 0)
			(effects (font (size 1.27 1.27)) hide))
		(symbol "ThreePin_0_1"
			(rectangle (start -2.54 2.54) (end 2.54 -2.54)
				(stroke (width 0) (type default))
				(fill (type background))))
		(symbol "ThreePin_1_1"
			(pin passive line (at -5.08 1.27 0) (length 2.54)
				(name "2" (effects (font (size 1.27 1.27))))
				(number "1" (effects (font (size 1.27 1.27)))))
			(pin passive line (at -5.08 0 0) (length 2.54)
				(name "B" (effects (font (size 1.27 1.27))))
				(number "2" (effects (font (size 1.27 1.27)))))
			(pin passive line (at -5.08 -1.27 0) (length 2.54)
				(name "C" (effects (font (size 1.27 1.27))))
				(number "3" (effects (font (size 1.27 1.27)))))))
)
"#;

fn test_environment(symbol_dir: &Path) -> Option<KicadInstallation> {
    let Some(installed) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return None;
    };
    KicadInstallation::detect_with(
        Some(symbol_dir),
        Some(installed.footprint_dir()),
        Some(installed.cli_path()),
        Some(installed.pcbnew_path()),
    )
}

fn cli_pin_nets(
    env: &KicadInstallation,
    schematic: &Path,
    refdes: &str,
) -> BTreeMap<String, String> {
    env.netlist(schematic)
        .expect("kicad-cli netlist")
        .nets
        .into_iter()
        .flat_map(|net| {
            net.nodes.into_iter().filter_map(move |(found, pin)| {
                (found == refdes).then(|| (pin, net.name.trim_start_matches('/').to_owned()))
            })
        })
        .collect()
}

fn install_fixture(dir: &Path) {
    std::fs::write(dir.join("PinShadow.kicad_sym"), SYMBOL_LIBRARY).unwrap();
}

#[test]
fn place_parts_keeps_numbered_pin_assignments_distinct() {
    let dir = tempfile::tempdir().unwrap();
    install_fixture(dir.path());
    let Some(env) = test_environment(dir.path()) else {
        return;
    };
    let provider = SymbolTable::from_symbol_dir(env.symbol_dir().to_path_buf());
    let input: sch_check::PlacePartsInput = serde_json::from_value(serde_json::json!({
        "parts": [
            { "ref": "U1", "part": "PinShadow:ThreePin", "pins": {"1": "A", "2": "B", "3": "C"} },
            { "ref": "U2", "part": "PinShadow:ThreePin", "pins": {"1": "A", "2": "B", "3": "C"} }
        ]
    }))
    .unwrap();
    let (design, diagnostics, _) = sch_check::into_design(&input, &provider, &Default::default());
    assert!(!diagnostics.has_errors(), "{diagnostics:#?}");

    let mut doc = live::blank_sheet().unwrap();
    let report = live::place_parts(&env, &mut doc, &input, &cluster_place::ClusterPlace).unwrap();
    assert!(report.committed, "{:?}", report.mismatch);
    assert!(live::verify(&doc, &design).is_empty());

    let schematic = dir.path().join("place-parts.kicad_sch");
    doc.write(&schematic).unwrap();
    // KiCAD may auto-name a two-pin net, so compare the partition, not the names:
    // each physical pin of U1 shares a net with the same pin of U2, and the
    // three nets are distinct — pin 1 (named `2`) never lands on pin 2's net.
    let u1 = cli_pin_nets(&env, &schematic, "U1");
    let u2 = cli_pin_nets(&env, &schematic, "U2");
    assert_eq!(u1, u2, "U1 and U2 must share nets pin for pin");
    let distinct: std::collections::BTreeSet<&String> = u1.values().collect();
    assert_eq!(
        distinct.len(),
        3,
        "each physical pin on its own net: {u1:?}"
    );
    assert_eq!(u1["2"], "B");
}

#[test]
fn an_unassigned_pin_name_cannot_shadow_a_physical_number() {
    let dir = tempfile::tempdir().unwrap();
    install_fixture(dir.path());
    let Some(env) = test_environment(dir.path()) else {
        return;
    };
    let mut component = Component {
        part: "PinShadow:ThreePin".to_owned(),
        ..Component::default()
    };
    component
        .pins
        .insert("2".to_owned(), PinTarget::Net("B".to_owned()));
    component
        .pins
        .insert("3".to_owned(), PinTarget::Net("C".to_owned()));
    let mut block = Block::default();
    block.components.insert("U2".to_owned(), component);
    let mut design = Design::default();
    design.blocks.insert("main".to_owned(), block);
    sch_check::nets::derive_attrs(&mut design);

    let emitted =
        floorplan::emit_strategy(&env, &design, Box::new(cluster_place::ClusterPlace), None)
            .unwrap();
    let schematic = dir.path().join("raw-design.kicad_sch");
    std::fs::write(&schematic, emitted.sch).unwrap();
    let doc = SchDoc::read(&schematic).unwrap();
    assert!(live::verify(&doc, &design).is_empty());
    let nets = cli_pin_nets(&env, &schematic, "U2");
    assert_eq!(nets.get("2").map(String::as_str), Some("B"), "{nets:?}");
    assert_eq!(nets.get("3").map(String::as_str), Some("C"), "{nets:?}");
    assert_ne!(
        nets.get("1"),
        nets.get("2"),
        "physical pin 1 named `2` shadowed physical pin number 2: {nets:?}"
    );
}
