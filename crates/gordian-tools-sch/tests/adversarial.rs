//! Adversarial cases for the live-schematic mutators: the edits that slip past
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
        ("5", "SHIELD_NET"),
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
        json!({"1": "A4", "2": "A7", "3": "A6", "4": "A1", "5": "S1"}),
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

/// A partial name match is not permission to discard the remaining old pin.
/// The refusal returns all of the information needed to construct a retry.
#[test]
fn swap_symbol_refusal_suggests_pin_map_and_unmatched_pins() {
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
    let before = std::fs::read(ctx.sch_path()).unwrap();

    let result = call(
        &ctx,
        "swap_symbol",
        json!({"ref": "J1", "lib_id": "Connector:USB_C_Plug_USB2.0"}),
    );
    assert!(result.get("error").is_some(), "swap must refuse: {result}");
    assert_eq!(
        result["suggestion"]["pin_map"],
        json!({"1": "A4", "2": "A7", "3": "A6", "5": "A1", "6": "S1"}),
        "the inferred mappings must be copyable into the next call: {result}"
    );
    assert_eq!(
        result["suggestion"]["old_pins_without_counterpart"],
        json!([{"number": "4", "name": "ID", "type": "passive"}]),
        "the unmatched old pin needs structured details: {result}"
    );
    let unassigned = result["suggestion"]["new_symbol_unassigned_pins"]
        .as_array()
        .expect("new unassigned pins array");
    for (number, name, pin_type) in [
        ("A5", "CC", "bidirectional"),
        ("B5", "VCONN", "bidirectional"),
    ] {
        assert!(
            unassigned.iter().any(|pin| {
                pin["number"] == number && pin["name"] == name && pin["type"] == pin_type
            }),
            "missing unassigned pin {number} ({name}, {pin_type}): {result}"
        );
    }
    assert_eq!(
        std::fs::read(ctx.sch_path()).unwrap(),
        before,
        "a refused swap must not write the schematic"
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
    assert!(text.contains("C7") && text.contains("P3"), "not one net: {text}");
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
