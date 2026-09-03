//! Connectivity regressions for the placement fallback bench.

use kicad::KicadInstallation;
use kicad_symbol::SymbolTable;
use sch_check::{ExistingSheet, PlacePartsInput};
use serde_json::json;

fn regulator_payload() -> PlacePartsInput {
    serde_json::from_value(json!({
        "block": "regulator_fix",
        "intent": {
            "flow": "lr",
            "ports": {"3V3": "right"},
            "rails": {"GND": "bottom"},
            "relations": [{"a": "U1", "b": "R32", "kind": "above"}]
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
    let provider = SymbolTable::from_symbol_dir(env.symbol_dir().to_path_buf());
    let payload = regulator_payload();
    let (design, diagnostics, audit) =
        sch_check::into_design(&payload, &provider, &ExistingSheet::default());
    assert!(!diagnostics.has_errors(), "{diagnostics:?}");
    assert!(audit.is_valid(), "{audit:?}");

    let mut doc = sch_floorplan::live::blank_sheet().expect("blank sheet");
    let alias_point = sch_doc::Pose::new(25.4, 25.4, 0.0);
    doc.add_label(sch_doc::LabelKind::Global, "+5V_USB", alias_point);
    doc.add_label(sch_doc::LabelKind::Local, "GND", alias_point);
    let report =
        sch_floorplan::live::add_parts(&env, &mut doc, &payload, None, "placement fallback")
            .expect("bench draw");

    assert!(report.committed, "{:#?}", report.mismatch);
    assert_eq!(report.authored_nets.get("GND"), Some(&"N_U1_2".to_string()));
    assert!(sch_floorplan::live::verify(&doc, &design).is_empty());
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
        sch_floorplan::live::add_parts(&env, &mut doc, &payload, None, "placement fallback")
            .expect("bench draw");

    assert!(report.committed, "{:#?}", report.mismatch);
    assert_eq!(
        report.authored_nets.get("Net-(old)"),
        Some(&"N_R1_1".to_string())
    );
    let authored = sch_doc::connect::extract(&doc)
        .nets
        .into_iter()
        .find(|net| net.name == "N_R1_1")
        .expect("authored net");
    assert_eq!(authored.pins.len(), 2);
}
