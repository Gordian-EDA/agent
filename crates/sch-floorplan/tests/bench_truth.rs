//! Connectivity regressions for the placement fallback bench.

use kicad::KicadInstallation;
use sch_check::PlacePartsInput;
use serde_json::json;

const SHORTED_PINS_LIBRARY: &str = r#"(kicad_symbol_lib
	(version 20231120)
	(generator "bench-truth-test")
	(symbol "ShortedPins"
		(property "Reference" "U" (at 0 5.08 0)
			(effects (font (size 1.27 1.27))))
		(property "Value" "ShortedPins" (at 0 2.54 0)
			(effects (font (size 1.27 1.27))))
		(property "Footprint" "" (at 0 0 0)
			(effects (font (size 1.27 1.27)) hide))
		(property "Datasheet" "" (at 0 0 0)
			(effects (font (size 1.27 1.27)) hide))
		(property "Description" "Coincident-pin verifier fixture" (at 0 0 0)
			(effects (font (size 1.27 1.27)) hide))
		(symbol "ShortedPins_0_1"
			(rectangle (start -2.54 2.54) (end 2.54 -2.54)
				(stroke (width 0) (type default))
				(fill (type background))))
		(symbol "ShortedPins_1_1"
			(pin passive line (at -5.08 0 0) (length 2.54)
				(name "A" (effects (font (size 1.27 1.27))))
				(number "1" (effects (font (size 1.27 1.27)))))
			(pin passive line (at -5.08 0 0) (length 2.54)
				(name "B" (effects (font (size 1.27 1.27))))
				(number "2" (effects (font (size 1.27 1.27)))))))
)
"#;

fn regulator_payload() -> PlacePartsInput {
    serde_json::from_value(json!({
        "block": "regulator_fix",
        "intent": {
            "ports": {"3V3": "right"},
            "rails": {"GND": "bottom"}
        },
        "name": "Corrected regulator symbol",
        "parts": [
            {
                "footprint": "Package_TO_SOT_SMD:SOT-23-5",
                "part": "Regulator_Linear:AP2112K-3.3",
                "pins": {"1": "+5V_USB", "2": "GND", "3": "EN_REG", "4": "nc", "5": "3V3"},
                "ref": "U1"
            },
            {
                "footprint": "Resistor_SMD:R_0603_1608Metric",
                "part": "Device:R",
                "pins": {"1": "3V3", "2": "EN_REG"},
                "ref": "R32",
                "value": "100k regulator enable"
            }
        ]
    }))
    .expect("campaign payload")
}

#[test]
fn campaign_regulator_payload_is_truthful_on_the_bench() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: KiCad 10 not configured");
        return;
    };
    let payload = regulator_payload();
    let mut doc = sch_floorplan::live::blank_sheet().expect("blank sheet");
    let existing: PlacePartsInput = serde_json::from_value(json!({
        "intent": {"rails": {"+5V_USB": "top", "GND": "bottom"}},
        "parts": [
            {"ref": "R10", "part": "Device:R", "pins": {"1": "GND", "2": "+5V_USB"}}
        ]
    }))
    .expect("existing sheet payload");
    let existing_report =
        sch_floorplan::live::bench(&env, &mut doc, &existing, None, "existing sheet")
            .expect("existing sheet draw");
    assert!(existing_report.committed, "{:#?}", existing_report.mismatch);
    let alias_point = sch_doc::Pose::new(25.4, 25.4, 0.0);
    doc.add_label(sch_doc::LabelKind::Global, "+5V_USB", alias_point);
    doc.add_label(sch_doc::LabelKind::Local, "GND", alias_point);
    let report =
        sch_floorplan::live::bench(&env, &mut doc, &payload, None, "placement fallback")
            .expect("bench draw");

    assert!(report.committed, "{:#?}", report.mismatch);
    assert!(report.minted_for_derived.is_empty());
    let u1_pin_2 = sch_doc::placed_pins(&doc)
        .into_iter()
        .find(|pin| pin.refdes == "U1" && pin.number == "2")
        .expect("U1 pin 2");
    assert!(doc.labels().any(|label| {
        sch_doc::unescape(&label.text) == "GND" && label.at.point() == u1_pin_2.at
    }));

    let dir = tempfile::tempdir().expect("tempdir");
    let schematic = dir.path().join("regulator-bench.kicad_sch");
    doc.write(&schematic).expect("write schematic");
    let cli = env
        .netlist(&schematic)
        .expect("kicad-cli sch export netlist");
    let ground = cli
        .nets
        .iter()
        .find(|net| {
            net.nodes
                .iter()
                .any(|node| node == &("U1".into(), "2".into()))
        })
        .expect("net containing U1.2");
    assert!(
        ground
            .nodes
            .iter()
            .any(|node| node == &("R10".into(), "1".into())),
        "U1.2 did not join the sheet's existing GND pin: {ground:?}"
    );
}

#[test]
fn derived_net_name_is_replaced_with_one_authored_name() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: KiCad 10 not configured");
        return;
    };
    let payload: PlacePartsInput = serde_json::from_value(json!({
        "parts": [
            {"ref": "R1", "part": "Device:R", "pins": {"1": "Net-(old)", "2": "GND"}},
            {"ref": "R2", "part": "Device:R", "pins": {"1": "VCC", "2": "Net-(old)"}}
        ]
    }))
    .expect("payload");
    let mut doc = sch_floorplan::live::blank_sheet().expect("blank sheet");

    let report =
        sch_floorplan::live::bench(&env, &mut doc, &payload, None, "placement fallback")
            .expect("bench draw");

    assert!(report.committed, "{:#?}", report.mismatch);
    assert_eq!(
        report.minted_for_derived.get("Net-(old)"),
        Some(&"N_R1_1".to_string())
    );
    let authored = sch_doc::connect::extract(&doc)
        .nets
        .into_iter()
        .find(|net| net.name == "N_R1_1")
        .expect("authored net");
    assert_eq!(authored.pins.len(), 2);
}

#[test]
fn a_bench_draw_that_merges_distinct_existing_nets_is_refused() {
    let Some(installed) = KicadInstallation::detect() else {
        eprintln!("SKIP: KiCad 10 not configured");
        return;
    };
    let mut doc = sch_floorplan::live::blank_sheet().expect("blank sheet");
    let existing: PlacePartsInput = serde_json::from_value(json!({
        "intent": {"rails": {"A": "top", "B": "bottom"}},
        "parts": [
            {"ref": "R10", "part": "Device:R", "pins": {"1": "A", "2": "B"}}
        ]
    }))
    .expect("existing sheet payload");
    let existing_report =
        sch_floorplan::live::bench(&installed, &mut doc, &existing, None, "existing sheet")
            .expect("existing sheet draw");
    assert!(existing_report.committed, "{:#?}", existing_report.mismatch);

    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("BenchTruth.kicad_sym"),
        SHORTED_PINS_LIBRARY,
    )
    .expect("write symbol fixture");
    let env = KicadInstallation::detect_with(
        Some(dir.path()),
        Some(installed.footprint_dir()),
        Some(installed.cli_path()),
    )
    .expect("fixture environment");
    let shorting: PlacePartsInput = serde_json::from_value(json!({
        "intent": {"rails": {"A": "top", "B": "bottom"}},
        "parts": [
            {"ref": "U2", "part": "BenchTruth:ShortedPins", "pins": {"1": "A", "2": "B"}}
        ]
    }))
    .expect("shorting payload");

    let report =
        sch_floorplan::live::bench(&env, &mut doc, &shorting, None, "adversarial bench draw")
            .expect("bench draw result");

    assert!(!report.committed, "shorting draw unexpectedly committed");
    assert!(
        report.mismatch.shorted == [("A".into(), "B".into())]
            || report.mismatch.shorted == [("B".into(), "A".into())],
        "{:#?}",
        report.mismatch
    );
    assert!(doc.symbols().all(|symbol| symbol.refdes() != "U2"));
}
