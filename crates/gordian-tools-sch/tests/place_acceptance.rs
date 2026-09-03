//! Acceptance coverage for non-blocking `place_parts` repairs.

use gordian_runtime::AgentRuntime;
use serde_json::{Value, json};

const EMPTY_SHEET: &str = "(kicad_sch\n\
\t(version 20250114)\n\
\t(generator \"eeschema\")\n\
\t(generator_version \"10.0\")\n\
\t(uuid \"4a1c0f2e-0000-4000-8000-0000000000bb\")\n\
\t(paper \"A4\")\n\
\t(lib_symbols)\n\
\t(sheet_instances\n\
\t\t(path \"/\"\n\
\t\t\t(page \"1\")\n\
\t\t)\n\
\t)\n\
)\n";

fn sheet() -> Option<AgentRuntime> {
    let ctx = AgentRuntime::detect_for_test()?;
    std::fs::write(ctx.sch_path(), EMPTY_SHEET).unwrap();
    Some(ctx)
}

fn call(ctx: &AgentRuntime, name: &str, input: Value) -> Value {
    gordian_tools_sch::run(name, input, ctx)
        .unwrap_or_else(|| panic!("`{name}` is not a schematic tool"))
        .unwrap_or_else(|error| panic!("`{name}` failed: {error}"))
}

#[test]
fn stm32_pin_keys_resolve_alternate_functions_and_rank_misses() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: KiCad 10 not configured");
        return;
    };
    let placed = call(
        &ctx,
        "place_parts",
        json!({"parts": [
            {
                "ref": "U1",
                "part": "MCU_ST_STM32F4:STM32F405RGTx",
                "pins": {"osc_in": "HSE_IN", "PH1-OSC_OUT": "HSE_OUT"}
            },
            {"ref": "R1", "part": "Device:R", "pins": {"1": "HSE_IN", "2": "GND"}},
            {"ref": "R2", "part": "Device:R", "pins": {"1": "HSE_OUT", "2": "GND"}}
        ]}),
    );
    assert!(placed.get("error").is_none(), "placement failed: {placed}");
    assert_ne!(placed["code"], "invalid_payload", "{placed}");
    let listing = call(&ctx, "read_schematic", json!({})).to_string();
    assert!(
        listing.contains("R1.1 U1.5"),
        "alternate pin was not wired: {listing}"
    );
    assert!(
        listing.contains("R2.1 U1.6"),
        "composite pin was not wired: {listing}"
    );

    let missed = call(
        &ctx,
        "place_parts",
        json!({"parts": [{
            "ref": "U2",
            "part": "MCU_ST_STM32F4:STM32F405RGTx",
            "pins": {"OSC_INPUT": "HSE_IN"}
        }]}),
    );
    assert_eq!(missed["code"], "nothing_placed", "{missed}");
    let unplaced = &missed["unplaced"][0];
    assert_eq!(unplaced["ref"], "U2", "{missed}");
    assert!(
        unplaced["did_you_mean"]
            .as_array()
            .is_some_and(|names| names.iter().any(|n| n == "RCC_OSC_IN")),
        "{missed}"
    );
    let reason = unplaced["reason"].as_str().unwrap();
    assert!(!reason.contains("available pins"), "{reason}");
    assert!(!diagnostic.contains("more"), "{diagnostic}");
}

#[test]
fn unresolved_and_incompatible_footprints_place_with_repair_reports() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: KiCad 10 not configured");
        return;
    };
    let result = call(
        &ctx,
        "place_parts",
        json!({"parts": [
            {
                "ref": "SW1",
                "part": "Switch:SW_SPST",
                "footprint": "Switch:SW_SPST",
                "pins": {"1": "GND", "2": "BUTTON"}
            },
            {
                "ref": "C1",
                "part": "Device:C_Polarized",
                "footprint": "Capacitor_SMD:C_1206_3216Metric",
                "pins": {"1": "VIN", "2": "GND"}
            }
        ]}),
    );
    assert!(result.get("error").is_none(), "placement failed: {result}");
    let unresolved = result["footprints_unresolved"].as_array().unwrap();
    assert_eq!(unresolved.len(), 2, "{result}");
    assert!(unresolved.iter().any(|issue| {
        issue["ref"] == "SW1"
            && issue["requested"] == "Switch:SW_SPST"
            && issue["did_you_mean"].is_array()
    }));
    assert!(unresolved.iter().any(|issue| issue["ref"] == "C1"));
    assert!(
        result["gaps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|gap| { gap["kind"] == "footprint_unresolved" && gap["refdes"] == "SW1" })
    );

    let doc = sch_doc::SchDoc::read(ctx.sch_path()).unwrap();
    for reference in ["SW1", "C1"] {
        let symbol = doc
            .symbols()
            .find(|symbol| symbol.refdes() == reference)
            .unwrap();
        assert_eq!(symbol.fields["Footprint"].value, "", "{reference}");
    }
}

#[test]
fn malformed_layout_intent_is_dropped_but_parts_remain_strict() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: KiCad 10 not configured");
        return;
    };
    let result = call(
        &ctx,
        "place_parts",
        json!({
            "parts": [{
                "ref": "R1",
                "part": "Device:R",
                "pins": {"1": "GND", "2": "SIG"},
                "": true
            }],
            "intent": {
                "ports": {"J1_PIN2": "GND"},
                "rails": {"3V3": "right"},
                "relations": [{"kind": "group", "name": "input"}]
            }
        }),
    );
    assert!(result.get("error").is_none(), "placement failed: {result}");
    let warnings = result["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 4, "{result}");
    for path in [
        "parts[0].",
        "intent.ports.J1_PIN2",
        "intent.rails.3V3",
        "intent.relations[0]",
    ] {
        assert!(
            warnings
                .iter()
                .any(|warning| warning.as_str().unwrap().contains(path)),
            "missing warning for {path}: {result}"
        );
    }

    let strict = gordian_tools_sch::run(
        "place_parts",
        json!({"parts": [{"part": "Device:R", "pins": {"1": 42}}]}),
        &ctx,
    )
    .unwrap()
    .unwrap_err()
    .to_string();
    assert!(strict.contains("parts[0].pins.1"), "{strict}");
}

#[test]
fn audio_jack_no_connect_policy_matches_sync_board() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: KiCad 10 not configured");
        return;
    };
    let placed = call(
        &ctx,
        "place_parts",
        json!({"parts": [{
            "ref": "J2",
            "part": "Connector_Audio:AudioJack2_Switch",
            "footprint": "Connector_Audio:Jack_3.5mm_QingPu_WQP-PJ398SM_Vertical_CircularHoles",
            "pins": {"S": "GND", "SN": "nc", "T": "AUDIO", "TN": "AUDIO_DETECT"}
        }]}),
    );
    assert!(placed.get("error").is_none(), "placement failed: {placed}");
    assert!(placed.get("footprints_unresolved").is_none(), "{placed}");

    let synced = pcb_workflow::sync_board(json!({}), &ctx).unwrap();
    assert!(
        synced.get("footprint_pin_mismatches").is_none(),
        "sync rejected the placement-compatible pair: {synced}"
    );
    assert!(synced.get("error").is_none(), "sync failed: {synced}");
}

#[test]
fn place_parts_contract_advertises_repairable_inputs() {
    let definition = gordian_tools_sch::tool_defs()
        .into_iter()
        .find(|tool| tool.name.as_str() == "place_parts")
        .unwrap();
    let description = definition.description.unwrap();
    for phrase in [
        "alternate functions",
        "footprints_unresolved",
        "Rails and ports accept left, right, top, or bottom",
    ] {
        assert!(description.contains(phrase), "missing {phrase}");
    }
    assert_eq!(
        definition.schema.unwrap()["properties"]["intent"]["properties"]["rails"]["additionalProperties"]
            ["enum"],
        json!(["left", "right", "top", "bottom"])
    );
}
