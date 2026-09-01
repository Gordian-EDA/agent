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
    let units = after.lines().filter(|l| l.starts_with("U1 ")).count();
    assert!(
        units > 1,
        "LM358 is a multi-unit part; only {units} unit reached the sheet:\n{after}"
    );
}
