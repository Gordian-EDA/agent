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
    assert!(!reason.contains("more"), "{reason}");
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
fn ambiguous_decouple_sugar_places_the_part_and_reports_the_gap() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: KiCad 10 not configured");
        return;
    };

    let result = call(
        &ctx,
        "place_parts",
        json!({"parts": [
            {
                "ref": "U1",
                "part": "Regulator_Linear:L7805",
                "pins": {"1": "RAW9", "2": "GND", "3": "OUT"},
                "decouple": {"100nF": 1}
            },
            {"ref": "R1", "part": "Device:R", "pins": {"1": "OUT", "2": "GND"}}
        ]}),
    );

    assert!(result.get("error").is_none(), "{result}");
    assert_ne!(result.get("ok"), Some(&json!(false)), "{result}");
    let unresolved = result["decouple_unresolved"].as_array().unwrap();
    assert_eq!(unresolved.len(), 1, "{result}");
    assert_eq!(unresolved[0]["ref"], "U1", "{result}");
    assert!(
        unresolved[0]["why"]
            .as_str()
            .is_some_and(|why| { why.contains("needs supply and ground candidates") })
    );
    assert!(
        unresolved[0]["how"]
            .as_str()
            .unwrap()
            .contains("explicitly")
    );
    assert!(
        result["gaps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|gap| { gap["kind"] == "decouple_unresolved" && gap["ref"] == "U1" })
    );
    let doc = sch_doc::SchDoc::read(ctx.sch_path()).unwrap();
    assert!(doc.symbol_by_ref("U1").is_some(), "{result}");
    assert_eq!(
        doc.symbols()
            .filter(|symbol| symbol.refdes().starts_with('C'))
            .count(),
        0,
        "ambiguous sugar must not invent a capacitor: {result}"
    );
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
                "rails": {"3V3": "right"}
            }
        }),
    );
    assert!(result.get("error").is_none(), "placement failed: {result}");
    let warnings = result["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 3, "{result}");
    for path in ["parts[0].", "intent.ports.J1_PIN2", "intent.rails.3V3"] {
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

/// A payload with an empty `parts` list used to answer "nothing could be placed",
/// naming no part and no reason because there was none to name.
#[test]
fn a_layout_only_payload_says_place_parts_creates_parts() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: KiCad 10 not configured");
        return;
    };
    let result = call(
        &ctx,
        "place_parts",
        json!({
            "parts": [],
            "block": "cleanup",
            "layout": {"cleanup": {"row": [{"part": "J1"}, {"part": "C1"}]}}
        }),
    );
    assert_eq!(result["code"], "no_parts", "{result:#}");
    let error = result["error"].as_str().unwrap();
    assert!(error.contains("J1") && error.contains("C1"), "{error}");
    assert!(error.contains("arrange"), "{error}");
}

/// A layout node saying both `row` and `col`, and a bare `{gap}` between siblings,
/// are the two near misses the model keeps writing; both have one honest reading.
#[test]
fn near_miss_layout_nodes_are_rewritten_rather_than_refused() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: KiCad 10 not configured");
        return;
    };
    let result = call(
        &ctx,
        "place_parts",
        json!({
            "parts": [
                {"ref": "R1", "part": "Device:R", "pins": {"1": "IN", "2": "MID"}},
                {"ref": "R2", "part": "Device:R", "pins": {"1": "MID", "2": "OUT"}},
                {"ref": "C1", "part": "Device:C", "pins": {"1": "MID", "2": "GND"}}
            ],
            "block": "divider",
            "layout": {"divider": {
                "row": [{"part": "R1"}, {"gap": 6}, {"part": "R2"}],
                "col": [{"part": "C1"}]
            }}
        }),
    );
    assert!(result.get("error").is_none(), "{result:#}");
    let warnings = result["warnings"].as_array().expect("{result:#}").clone();
    let text = warnings
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(text.contains("said both"), "{text}");
    assert!(text.contains("names no part, row or col"), "{text}");
    let doc = sch_doc::SchDoc::read(ctx.sch_path()).unwrap();
    for refdes in ["R1", "R2", "C1"] {
        assert!(
            doc.symbols().any(|symbol| symbol.refdes() == refdes),
            "`{refdes}` was not drawn"
        );
    }
}

/// The layout tree is where the author states the drawing; a part it places in
/// another region joins that region instead of refusing the whole payload.
#[test]
fn a_layout_tree_adopts_a_part_declared_in_another_region() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: KiCad 10 not configured");
        return;
    };
    let result = call(
        &ctx,
        "place_parts",
        json!({
            "parts": [
                {"ref": "R1", "part": "Device:R", "block": "input", "pins": {"1": "IN", "2": "MID"}},
                {"ref": "R2", "part": "Device:R", "block": "input", "pins": {"1": "MID", "2": "OUT"}},
                {"ref": "C1", "part": "Device:C", "block": "output", "pins": {"1": "OUT", "2": "GND"}}
            ],
            "layout": {
                "input": {"row": [{"part": "R1"}]},
                "output": {"row": [{"part": "R2"}, {"part": "C1"}]}
            }
        }),
    );
    assert!(result.get("error").is_none(), "{result:#}");
    let warnings = result["warnings"].as_array().expect("{result:#}").clone();
    assert!(
        warnings
            .iter()
            .filter_map(Value::as_str)
            .any(|warning| warning.contains("layout-adopted-part") && warning.contains("R2")),
        "the move was never reported: {result:#}"
    );
    let doc = sch_doc::SchDoc::read(ctx.sch_path()).unwrap();
    assert!(doc.symbols().any(|symbol| symbol.refdes() == "R2"));
}

/// A `layout` key no part declares is the payload naming its one region twice, not a
/// second region: adopting there would rename the block out from under `arrange`.
#[test]
fn a_layout_key_no_part_declares_does_not_move_the_parts() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: KiCad 10 not configured");
        return;
    };
    let result = call(
        &ctx,
        "place_parts",
        json!({
            "parts": [
                {"ref": "R1", "part": "Device:R", "block": "power_entry", "pins": {"1": "IN", "2": "MID"}},
                {"ref": "R2", "part": "Device:R", "block": "power_entry", "pins": {"1": "MID", "2": "GND"}}
            ],
            "blocks": {"power_entry": {"title": "Power entry"}},
            "layout": {"power": {"row": [{"part": "R1"}, {"part": "R2"}]}}
        }),
    );
    assert!(result.get("error").is_none(), "{result:#}");
    let arranged = call(&ctx, "arrange", json!({"block": "power_entry"}));
    assert!(arranged.get("error").is_none(), "{arranged:#}");
    assert_ne!(
        arranged["changed"], "no arrangeable parts selected",
        "the declared block was renamed away: {arranged:#}"
    );
}

/// `@R1.2` is how every tool result spells the net on a pin, so the model writes it
/// back as an endpoint. As an endpoint the pin and its net are the same place.
#[test]
fn connect_accepts_the_at_prefix_a_tool_result_taught_it() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: KiCad 10 not configured");
        return;
    };
    let placed = call(
        &ctx,
        "place_parts",
        json!({"parts": [
            {"ref": "R1", "part": "Device:R", "pins": {"1": "IN", "2": "MID"}},
            {"ref": "R2", "part": "Device:R", "pins": {"1": "OUT", "2": "GND"}}
        ]}),
    );
    assert!(placed.get("error").is_none(), "{placed:#}");
    let result = call(&ctx, "connect", json!({"from": "R1.2", "to": "@R2.1"}));
    assert!(result.get("error").is_none(), "{result:#}");
}

/// A pin address is what every tool result prints, so the model hands one back to a
/// tool that takes references. A designator never contains a `.`; it names one symbol.
#[test]
fn remove_symbols_reads_a_pin_address_as_its_symbol() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: KiCad 10 not configured");
        return;
    };
    let placed = call(
        &ctx,
        "place_parts",
        json!({"parts": [
            {"ref": "R1", "part": "Device:R", "pins": {"1": "IN", "2": "MID"}},
            {"ref": "R2", "part": "Device:R", "pins": {"1": "MID", "2": "GND"}}
        ]}),
    );
    assert!(placed.get("error").is_none(), "{placed:#}");
    let result = call(&ctx, "remove_symbols", json!({"refs": ["R2.1"]}));
    assert!(result.get("error").is_none(), "{result:#}");
    assert_eq!(result["changed"]["read_as"]["R2.1"], "R2", "{result:#}");
    let doc = sch_doc::SchDoc::read(ctx.sch_path()).unwrap();
    assert!(!doc.symbols().any(|symbol| symbol.refdes() == "R2"));
}

/// The pin suffix is read only when it is really one of that symbol's pins: a part is
/// far too much to delete on a name the sheet does not actually carry.
#[test]
fn remove_symbols_keeps_a_part_named_by_a_pin_it_does_not_have() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: KiCad 10 not configured");
        return;
    };
    let placed = call(
        &ctx,
        "place_parts",
        json!({"parts": [
            {"ref": "R1", "part": "Device:R", "pins": {"1": "IN", "2": "MID"}},
            {"ref": "R2", "part": "Device:R", "pins": {"1": "MID", "2": "GND"}}
        ]}),
    );
    assert!(placed.get("error").is_none(), "{placed:#}");
    let result = call(&ctx, "remove_symbols", json!({"refs": ["R2.9"]}));
    assert!(result["error"].is_string(), "{result:#}");
    let doc = sch_doc::SchDoc::read(ctx.sch_path()).unwrap();
    assert!(doc.symbols().any(|symbol| symbol.refdes() == "R2"));
}

/// A reference nothing on the sheet carries is refused with the sheet's nearest
/// designators, so the next call has somewhere to go.
#[test]
fn remove_symbols_names_the_closest_references_it_does_carry() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: KiCad 10 not configured");
        return;
    };
    let placed = call(
        &ctx,
        "place_parts",
        json!({"parts": [
            {"ref": "R1", "part": "Device:R", "pins": {"1": "IN", "2": "MID"}},
            {"ref": "R2", "part": "Device:R", "pins": {"1": "MID", "2": "GND"}}
        ]}),
    );
    assert!(placed.get("error").is_none(), "{placed:#}");
    let result = call(&ctx, "remove_symbols", json!({"refs": ["R12"]}));
    assert!(result["error"].is_string(), "{result:#}");
    let near = result["did_you_mean"]["R12"]
        .as_array()
        .unwrap_or_else(|| panic!("{result:#}"));
    assert!(!near.is_empty(), "{result:#}");
}

/// Every result prints a pin as `R1.2`, so the model asks for "the net on R1.2" by
/// that address. It is a question the netlist can answer.
#[test]
fn get_net_answers_a_pin_address() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: KiCad 10 not configured");
        return;
    };
    let placed = call(
        &ctx,
        "place_parts",
        json!({"parts": [
            {"ref": "R1", "part": "Device:R", "pins": {"1": "IN", "2": "MID"}},
            {"ref": "R2", "part": "Device:R", "pins": {"1": "MID", "2": "GND"}}
        ]}),
    );
    assert!(placed.get("error").is_none(), "{placed:#}");
    let result = call(&ctx, "get_net", json!({"name": "R1.2"}));
    assert!(result.get("error").is_none(), "{result:#}");
    assert_eq!(result["resolved_from"], "R1.2", "{result:#}");
    assert_eq!(result["name"], "Net-(R1-Pad2)", "{result:#}");
    assert!(
        result["report"].as_str().unwrap().contains("R2.1"),
        "{result:#}"
    );
}
