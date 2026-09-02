//! Adversarial cases for schematic mutators: the edits that slip past
//! the connectivity guard or corrupt the sheet's identity table.
//!
//! Skips when no KiCAD is installed: the mutators embed library definitions.

use gordian_runtime::AgentRuntime;
use serde_json::{Value, json};

const EMPTY_SHEET: &str = "(kicad_sch\n\
\t(version 20250114)\n\
\t(generator \"eeschema\")\n\
\t(generator_version \"9.0\")\n\
\t(uuid \"4a1c0f2e-0000-4000-8000-0000000000aa\")\n\
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
        .unwrap_or_else(|e| panic!("`{name}` failed: {e}"))
}

/// The whole sheet as `read_schematic` prints it.
fn listing(ctx: &AgentRuntime) -> String {
    match call(ctx, "read_schematic", json!({})) {
        Value::String(text) => text,
        other => panic!("read_schematic returned {other}"),
    }
}

#[test]
fn place_parts_refuses_a_reference_already_on_the_sheet() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let seeded = call(
        &ctx,
        "add_symbols",
        json!({"parts": [
            {"lib_id": "Device:C", "ref": "C1", "value": "1uF"},
            {"lib_id": "Device:C", "ref": "C2", "value": "1uF", "near": "C1", "side": "right"}
        ]}),
    );
    assert!(seeded.get("error").is_none(), "fixture failed: {seeded}");

    let result = call(
        &ctx,
        "place_parts",
        json!({"parts": [{
            "ref": "C2",
            "part": "Device:C",
            "pins": {"1": "VIN", "2": "GND"}
        }]}),
    );

    assert_eq!(result["code"], "invalid_payload");
    assert_eq!(
        result["duplicate_refs"],
        json!([{"ref": "C2", "next_free": "C3"}])
    );
}

#[test]
fn place_parts_reports_an_auto_assigned_reference_as_placed() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };

    let result = call(
        &ctx,
        "place_parts",
        json!({"parts": [{
            "part": "Device:R",
            "value": "10k",
            "pins": {"1": "VIN", "2": "GND"}
        }]}),
    );

    assert!(result.get("error").is_none(), "placement failed: {result}");
    assert_eq!(result["changed"]["placed"], json!(["R1"]));
}

#[test]
fn wrong_footprint_is_refused_and_its_suggestion_closes_the_loop() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    let wrong = "Capacitor_SMD:C_1206_3216Metric";
    let payload = |footprint: &str| {
        json!({"parts": [{
            "ref": "C1",
            "part": "Device:C_Polarized",
            "value": "10uF",
            "footprint": footprint,
            "pins": {"1": "VIN", "2": "GND"}
        }]})
    };

    let refused = call(&ctx, "place_parts", payload(wrong));
    assert_eq!(refused["code"], "invalid_payload");
    let mismatch = &refused["footprint_mismatch"][0];
    assert_eq!(mismatch["ref"], "C1");
    assert_eq!(mismatch["symbol"], "Device:C_Polarized");
    assert_eq!(mismatch["footprint"], wrong);
    let suggestion = mismatch["suggestion"]
        .as_str()
        .expect("refusal must include a compatible footprint")
        .to_string();
    assert!(!listing(&ctx).contains("C1"), "refusal wrote the part");

    let placed = call(&ctx, "place_parts", payload(&suggestion));
    assert!(placed.get("error").is_none(), "placement failed: {placed}");
    let reassignment = call(
        &ctx,
        "assign_footprints",
        json!({"assignments": [{"reference": "C1", "footprint": wrong}]}),
    );
    assert_eq!(reassignment["code"], "invalid_payload");
    let repaired = reassignment["footprint_mismatch"][0]["suggestion"]
        .as_str()
        .expect("assignment refusal must include a compatible footprint");
    let assigned = call(
        &ctx,
        "assign_footprints",
        json!({"assignments": [{"reference": "C1", "footprint": repaired}]}),
    );
    assert!(
        assigned.get("error").is_none(),
        "assignment failed: {assigned}"
    );

    let source = std::fs::read_to_string(ctx.sch_path()).unwrap();
    let broken = source.replacen(repaired, wrong, 1);
    assert_ne!(broken, source, "fixture footprint was not written");
    std::fs::write(ctx.sch_path(), broken).unwrap();
    let checked = call(&ctx, "check_schematic", json!({"detail": true}));
    let finding = checked["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|finding| finding["code"] == "footprint-pins")
        .expect("checker missed the incompatible footprint");
    assert_eq!(finding["fix"]["tool"], "assign_footprints");
    assert_eq!(finding["fix"]["args"]["assignments"][0]["reference"], "C1");
    let fixed = call(
        &ctx,
        finding["fix"]["tool"].as_str().unwrap(),
        finding["fix"]["args"].clone(),
    );
    assert!(fixed.get("error").is_none(), "inline fix failed: {fixed}");

    let checked = call(&ctx, "check_schematic", json!({"detail": true}));
    assert!(
        checked["findings"]
            .as_array()
            .unwrap()
            .iter()
            .all(|finding| finding["code"] != "footprint-pins"),
        "compatible repair left a footprint-pins finding: {checked}"
    );
}

#[test]
fn add_symbols_cannot_bypass_footprint_compatibility() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    let result = call(
        &ctx,
        "add_symbols",
        json!({"parts": [{
            "lib_id": "Connector:Barrel_Jack",
            "ref": "J1",
            "footprint": "Connector_BarrelJack:BarrelJack_Horizontal"
        }]}),
    );

    assert_eq!(result["code"], "invalid_payload");
    assert_eq!(result["footprint_mismatch"][0]["ref"], "J1");
    assert!(result["footprint_mismatch"][0]["suggestion"].is_string());
    assert!(!listing(&ctx).contains("J1"), "refusal wrote the symbol");
}

#[test]
fn place_parts_reports_footprints_with_other_payload_faults() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    let seeded = call(
        &ctx,
        "add_symbols",
        json!({"parts": [{"lib_id": "Device:R", "ref": "J1"}]}),
    );
    assert!(seeded.get("error").is_none(), "fixture failed: {seeded}");
    let result = call(
        &ctx,
        "place_parts",
        json!({"parts": [{
            "ref": "J1",
            "part": "Connector:Barrel_Jack",
            "footprint": "Connector_BarrelJack:BarrelJack_Horizontal",
            "pins": {"bad-pin": "SIG"}
        }]}),
    );

    assert_eq!(result["code"], "invalid_payload");
    assert!(!result["duplicate_refs"].as_array().unwrap().is_empty());
    assert!(!result["unknown_pins"].as_array().unwrap().is_empty());
    assert!(!result["footprint_mismatch"].as_array().unwrap().is_empty());
}

/// Renaming a part onto a reference another part already holds must be refused:
/// two symbols answering to `R2` is a corrupt sheet — `uuid_of` can no longer
/// resolve it, so every later tool call on `R2` is ambiguous, and KiCAD's own
/// annotation is broken.
#[test]
fn set_fields_refuses_to_rename_a_part_onto_a_taken_reference() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    call(
        &ctx,
        "add_symbols",
        json!({"parts": [
            {"lib_id": "Device:R", "ref": "R1", "value": "10k"},
            {"lib_id": "Device:R", "ref": "R2", "value": "22k", "near": "R1", "side": "right"}
        ]}),
    );
    let result = call(
        &ctx,
        "set_fields",
        json!({"ref": "R1", "fields": {"Reference": "R2"}}),
    );
    assert!(
        result.get("error").is_some(),
        "renaming R1 to the taken reference R2 must be refused, got {result}"
    );
    let text = std::fs::read_to_string(ctx.sch_path()).unwrap();
    assert_eq!(
        text.matches("\"R2\"").count(),
        1,
        "the sheet must not end up with two parts called R2:\n{}",
        listing(&ctx)
    );
}

/// `swap_symbol`'s advertised use for a reversed pinout — `pin_map` transposing
/// two pins — must move each net to its new pin, not fuse them.
///
/// Each mapped pin's copper is dragged to its new home one pin at a time, so
/// the first drag piles both nets onto one point and the second carries the pile
/// on. The guard permits it because `Allow` names every net the swapped part
/// touches, and a merge whose sources are all named reads as intentional.
#[test]
fn swapping_a_reversed_pinout_must_not_short_the_two_nets_together() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    call(
        &ctx,
        "add_symbols",
        json!({"parts": [
            {"lib_id": "Device:R", "ref": "R1", "value": "10k"},
            {"lib_id": "Device:R", "ref": "R2", "value": "1k", "near": "R1", "side": "above"},
            {"lib_id": "Device:R", "ref": "R3", "value": "1k", "near": "R1", "side": "below"}
        ]}),
    );
    call(
        &ctx,
        "connect",
        json!({"from": "R1.1", "to": "R2.2", "net": "NETA"}),
    );
    call(
        &ctx,
        "connect",
        json!({"from": "R1.2", "to": "R3.1", "net": "NETB"}),
    );
    let before = listing(&ctx);
    assert!(
        before.contains("NETA") && before.contains("NETB"),
        "the fixture needs two distinct nets on R1:\n{before}"
    );

    let result = call(
        &ctx,
        "swap_symbol",
        json!({"ref": "R1", "lib_id": "Device:R", "pin_map": {"1": "2", "2": "1"}}),
    );
    let after = listing(&ctx);
    // R2 and R3 were never connected to each other and the call named neither.
    let shorted = after
        .lines()
        .any(|line| line.contains("R2.") && line.contains("R3.") && line.starts_with("NET"));
    assert!(
        !shorted && after.contains("NETA") && after.contains("NETB"),
        "the transposing swap fused NETA and NETB.\nresult: {result}\nafter:\n{after}"
    );
}

/// Connector families often spell the same logical pins with package-specific
/// pad numbers. A swap maps those names without making the caller transcribe a
/// pin map.
#[test]
fn swap_symbol_maps_differently_numbered_connector_pins_by_name() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let placed = call(
        &ctx,
        "add_symbols",
        json!({"parts": [{"lib_id": "Connector:USB_B", "ref": "J1"}]}),
    );
    assert!(placed.get("error").is_none(), "fixture failed: {placed}");
    for (pin, net) in [
        ("1", "VBUS_NET"),
        ("2", "DM_NET"),
        ("3", "DP_NET"),
        ("4", "GND_NET"),
        ("SH", "SHIELD_NET"),
    ] {
        let labeled = call(
            &ctx,
            "label",
            json!({"pin": format!("J1.{pin}"), "net": net}),
        );
        assert!(labeled.get("error").is_none(), "fixture failed: {labeled}");
    }

    let result = call(
        &ctx,
        "swap_symbol",
        json!({"ref": "J1", "lib_id": "Connector:USB_C_Plug_USB2.0"}),
    );
    assert!(result.get("error").is_none(), "swap failed: {result}");
    assert_eq!(
        result["changed"]["mapped_by_name"],
        json!({"1": "A4", "2": "A7", "3": "A6", "4": "A1"}),
        "the response must expose every automatic name mapping: {result}"
    );
    assert_eq!(
        result["changed"]["dropped_pins"],
        json!([]),
        "remapped pins must not also be reported as dropped: {result}"
    );
    let after = listing(&ctx);
    for net in ["VBUS_NET", "DM_NET", "DP_NET", "GND_NET", "SHIELD_NET"] {
        assert!(after.contains(net), "{net} was lost:\n{after}");
    }
    assert!(
        after.contains("Connector:USB_C_Plug_USB2.0"),
        "the new connector must be committed:\n{after}"
    );
}

/// An old pin with no net is not connectivity, so a narrowing swap may drop it —
/// refusing over unwired pins made every narrowing swap impossible. The drop is
/// reported, and the pins the new symbol brings are marked no-connect rather than
/// left bare for ERC to fault.
#[test]
fn swap_symbol_drops_an_unwired_pin_and_marks_what_it_gains() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let placed = call(
        &ctx,
        "add_symbols",
        json!({"parts": [{"lib_id": "Connector:USB_B_Micro", "ref": "J1"}]}),
    );
    assert!(placed.get("error").is_none(), "fixture failed: {placed}");

    let result = call(
        &ctx,
        "swap_symbol",
        json!({"ref": "J1", "lib_id": "Connector:USB_C_Plug_USB2.0"}),
    );
    assert!(result.get("error").is_none(), "swap must succeed: {result}");
    assert_eq!(
        result["changed"]["dropped_pins"],
        json!(["4"]),
        "the dropped unwired pin must be reported: {result}"
    );
    let warnings = serde_json::to_string(&result["warnings"]).unwrap();
    assert!(
        warnings.contains("marked no-connect"),
        "gained pins must be no-connected, not left for ERC: {warnings}"
    );
}

/// A pin that carries a net still blocks the swap: dropping it would delete a
/// branch, which is what the guard is for.
#[test]
fn swap_symbol_refuses_when_a_wired_pin_has_no_counterpart() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let seeded = call(
        &ctx,
        "place_parts",
        json!({"parts": [
            {"ref": "J1", "part": "Connector:USB_B_Micro",
             "pins": {"1": "VBUS", "2": "USB_DM", "3": "USB_DP", "4": "USB_ID", "5": "GND"}},
            {"ref": "R1", "part": "Device:R", "value": "10k",
             "pins": {"1": "USB_ID", "2": "GND"}}
        ]}),
    );
    assert!(seeded.get("error").is_none(), "fixture failed: {seeded}");
    let before = std::fs::read(ctx.sch_path()).unwrap();

    let result = call(
        &ctx,
        "swap_symbol",
        json!({"ref": "J1", "lib_id": "Connector:USB_C_Plug_USB2.0"}),
    );
    let error = result["error"].as_str().unwrap_or_default();
    assert!(error.contains("carry nets"), "{result}");
    assert!(error.contains('4'), "the wired pin must be named: {result}");
    assert_eq!(
        std::fs::read(ctx.sch_path()).unwrap(),
        before,
        "a refused swap must not write the schematic"
    );
}

/// A `pin_map` target the new symbol does not have is a bad map, and must be
/// reported as one rather than as the old pin having "no counterpart".
#[test]
fn swap_symbol_names_a_pin_map_target_that_does_not_exist() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let placed = call(
        &ctx,
        "add_symbols",
        json!({"parts": [{"lib_id": "Connector:USB_B_Micro", "ref": "J1"}]}),
    );
    assert!(placed.get("error").is_none(), "fixture failed: {placed}");

    let result = call(
        &ctx,
        "swap_symbol",
        json!({"ref": "J1", "lib_id": "Connector:USB_C_Plug_USB2.0",
               "pin_map": {"4": "NOT_A_PIN"}}),
    );
    let error = result["error"].as_str().unwrap_or_default();
    assert!(error.contains("NOT_A_PIN"), "{result}");
    assert!(error.contains("pin_map"), "{result}");
    assert!(
        result["new_pins"].as_array().is_some_and(|p| !p.is_empty()),
        "the refusal must list the pins that do exist: {result}"
    );
}

/// A part placed by `lib_id` must arrive whole. A dual op-amp is three units —
/// two amplifiers and a power unit — and only unit 1 is ever written, so the
/// second amplifier and the supply pins never reach the sheet: ERC cannot see
/// pins that are not there, and no tool on this surface can add them.
#[test]
fn adding_a_multi_unit_part_places_every_unit() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let result = call(
        &ctx,
        "add_symbols",
        json!({"parts": [{"lib_id": "Amplifier_Operational:LM358", "ref": "U1"}]}),
    );
    assert!(
        result.get("error").is_none(),
        "the fixture symbol must exist: {result}"
    );
    let after = listing(&ctx);
    let units = after
        .lines()
        .filter(|line| line.starts_with("  unit "))
        .count();
    assert!(
        units > 1,
        "LM358 is a multi-unit part; only {units} unit reached the sheet:\n{after}"
    );
}

/// A swap that keeps every pin number but turns one into a supply pin passes the
/// connectivity guard untouched — the net partition is identical — while KiCAD
/// gains a `power_pin_not_driven` error the sheet has no way to answer. Swapping
/// a plain 2-pin part for a power symbol is the smallest form of that change.
#[test]
fn a_swap_that_makes_a_mapped_pin_a_supply_pin_is_refused() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let placed = call(
        &ctx,
        "add_symbols",
        json!({"parts": [{"lib_id": "Connector_Generic:Conn_01x01", "ref": "J1"}]}),
    );
    assert!(placed.get("error").is_none(), "fixture failed: {placed}");
    let before = std::fs::read(ctx.sch_path()).unwrap();

    let result = call(
        &ctx,
        "swap_symbol",
        json!({"ref": "J1", "lib_id": "power:GND"}),
    );
    let error = result["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("supply pin"),
        "the swap must name the newly undriven supply pin: {result}"
    );
    assert_eq!(
        std::fs::read(ctx.sch_path()).unwrap(),
        before,
        "a refused swap must not write the schematic"
    );
}

/// `read_schematic` prints the names KiCAD generates for unnamed nets, and they
/// read like identities. Labelling another node with one forks the net instead
/// of joining it — KiCAD renames the original to `…_1` — and no connectivity
/// guard sees a break, because on paper both nets still exist.
#[test]
fn labelling_a_node_with_a_generated_net_name_is_refused() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let placed = call(
        &ctx,
        "add_symbols",
        json!({"parts": [
            {"lib_id": "Device:R", "ref": "R1"},
            {"lib_id": "Device:R", "ref": "R2"},
            {"lib_id": "Device:R", "ref": "R3"},
        ]}),
    );
    assert!(placed.get("error").is_none(), "fixture failed: {placed}");
    let joined = call(&ctx, "connect", json!({"from": "R1.2", "to": "R2.1"}));
    assert!(joined.get("error").is_none(), "fixture failed: {joined}");

    let text = listing(&ctx);
    let start = text
        .find("Net-(")
        .expect("an unnamed net gets a generated name");
    let generated = &text[start..start + text[start..].find(')').unwrap() + 1];

    let result = call(&ctx, "label", json!({"pin": "R3.1", "net": generated}));
    let error = result["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("generates for an unnamed net"),
        "labelling `{generated}` must be refused: {result}"
    );
    assert!(
        !listing(&ctx).contains(&format!("{generated}_1")),
        "the original net was forked anyway"
    );
}

#[test]
fn place_parts_joins_a_net_named_only_by_a_pin_reference() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    // P3's pins land on nets KiCAD names itself, so there is no text a later call
    // could write to join them. `@P3.1` is how the caller says which net it means.
    let seeded = call(
        &ctx,
        "place_parts",
        json!({"parts": [
            {"ref": "P3", "part": "Device:R", "value": "10k", "pins": {"1": "RAW", "2": "GND"}},
            {"ref": "R9", "part": "Device:R", "value": "1k", "pins": {"1": "RAW", "2": "GND"}}
        ]}),
    );
    assert!(seeded.get("error").is_none(), "fixture failed: {seeded}");

    let joined = call(
        &ctx,
        "place_parts",
        json!({"parts": [
            {"ref": "C7", "part": "Device:C", "value": "100nF",
             "pins": {"1": "@P3.1", "2": "GND"}}
        ]}),
    );
    assert!(joined.get("error").is_none(), "{joined}");
    assert_ne!(joined["code"], "invalid_payload", "{joined}");

    // C7 pin 1 must now share a net with P3 pin 1 — not sit on a second one.
    let nets = call(&ctx, "get_net", json!({"name": "N_P3_1"}));
    let text = serde_json::to_string(&nets).unwrap();
    assert!(
        text.contains("C7") && text.contains("P3"),
        "not one net: {text}"
    );
}

#[test]
fn place_parts_explains_the_relation_shapes_when_one_is_malformed() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let error = gordian_tools_sch::run(
        "place_parts",
        json!({"parts": [{"ref": "R1", "part": "Device:R", "pins": {"1": "A", "2": "B"}}],
               "intent": {"relations": [["R1", "U1", "left"]]}}),
        &ctx,
    )
    .expect("place_parts is a schematic tool")
    .expect_err("a malformed relation must be refused");
    let message = format!("{error:#}");

    assert!(message.contains("relations"), "{message}");
    assert!(message.contains("\"kind\":\"left_of\""), "{message}");
    assert!(message.contains("\"kind\":\"group\""), "{message}");
    assert!(message.contains("\"kind\":\"align\""), "{message}");
}

#[test]
fn place_parts_accepts_the_engine_its_own_refusal_recommends() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    // The placement-failure refusal names an engine override as the way out, so
    // the payload has to accept one; `deny_unknown_fields` used to reject it.
    let result = call(
        &ctx,
        "place_parts",
        json!({"engine": "anneal", "parts": [
            {"ref": "R1", "part": "Device:R", "value": "10k", "pins": {"1": "A", "2": "B"}},
            {"ref": "R2", "part": "Device:R", "value": "10k", "pins": {"1": "A", "2": "B"}}
        ]}),
    );
    assert!(result.get("error").is_none(), "{result}");

    let unknown = call(
        &ctx,
        "place_parts",
        json!({"engine": "nonsense", "parts": [
            {"ref": "R3", "part": "Device:R", "pins": {"1": "A", "2": "B"}}
        ]}),
    );
    let message = unknown["error"].as_str().unwrap_or_default();
    assert!(
        message.contains("nonsense") && message.contains("anneal"),
        "{unknown}"
    );
}

#[test]
fn connect_puts_one_pin_on_a_named_net() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let seeded = call(
        &ctx,
        "place_parts",
        json!({"parts": [
            {"ref": "R1", "part": "Device:R", "value": "10k", "pins": {"1": "VCC", "2": "GND"}},
            {"ref": "R2", "part": "Device:R", "value": "10k", "pins": {"1": "VCC", "2": "GND"}}
        ]}),
    );
    assert!(seeded.get("error").is_none(), "fixture failed: {seeded}");

    // "Put this pin on that net" is a connect the model actually writes. It used to
    // come back as an argument complaint, costing a request every time; it now names
    // the net at that pin, which is what it was asking for.
    let result = call(&ctx, "connect", json!({"from": "R1.1", "net": "VCC"}));
    let error = result["error"].as_str().unwrap_or_default();
    assert!(
        !error.contains("needs `from` and `to`"),
        "one end plus a net is a real request, not a malformed call: {result}"
    );
    assert!(result.get("error").is_none(), "{result}");

    // A genuinely malformed call still says what is missing, and now says both forms.
    let bad = call(&ctx, "connect", json!({"from": "R1.1"}));
    let message = bad["error"].as_str().unwrap_or_default();
    assert!(message.contains("`net`"), "{bad}");
}
