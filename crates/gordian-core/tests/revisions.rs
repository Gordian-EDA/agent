//! Unified offline revision integration through the public tool dispatcher.

use gordian_core::AgentRuntime;
use gordian_core::tools::run_tool;
use kicad_board::BoardDoc;
use serde_json::{Value, json};

const R0805: &str = "Resistor_SMD:R_0805_2012Metric";

fn tool(ctx: &AgentRuntime, name: &str, input: Value) -> Value {
    let output = run_tool(name, input, ctx).unwrap_or_else(|error| panic!("{name}: {error}"));
    assert!(
        output.get("error").is_none() && output.get("ok").and_then(Value::as_bool) != Some(false),
        "{name} failed: {output:#}"
    );
    output
}

fn position(ctx: &AgentRuntime, reference: &str) -> ([f64; 2], f64) {
    let board = BoardDoc::parse(std::fs::read_to_string(ctx.pcb_path()).unwrap()).unwrap();
    let footprint = board
        .footprints()
        .into_iter()
        .find(|footprint| footprint.reference == reference)
        .unwrap();
    ([footprint.at.x, footprint.at.y], footprint.rotation)
}

#[test]
fn undo_and_history_span_schematic_and_board_mutators() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };

    tool(
        &ctx,
        "place_parts",
        json!({"block": "divider", "parts": [
            {"ref": "R1", "part": "Device:R", "value": "10k", "footprint": R0805,
             "pins": {"1": "VIN", "2": "SENSE"}},
            {"ref": "R2", "part": "Device:R", "value": "10k", "footprint": R0805,
             "pins": {"1": "SENSE", "2": "GND"}}
        ]}),
    );
    let fields = tool(
        &ctx,
        "set_fields",
        json!({"ref": "R1", "fields": {"Value": "47k"}}),
    );
    assert!(fields["revision"].as_u64().is_some());
    assert_eq!(
        sch_doc::SchDoc::read(ctx.sch_path())
            .unwrap()
            .symbol_by_ref("R1")
            .unwrap()
            .value(),
        "47k"
    );

    tool(&ctx, "undo", json!({}));
    assert_eq!(
        sch_doc::SchDoc::read(ctx.sch_path())
            .unwrap()
            .symbol_by_ref("R1")
            .unwrap()
            .value(),
        "10k"
    );

    let synced = tool(
        &ctx,
        "sync_board",
        json!({ "bounds": { "min_x": 0.0, "min_y": 0.0, "max_x": 60.0, "max_y": 60.0 } }),
    );
    assert!(synced["revision"].as_u64().is_some());
    let board_before_move = std::fs::read(ctx.pcb_path()).unwrap();
    let position_before_move = position(&ctx, "R1");
    let moved = tool(
        &ctx,
        "move_parts",
        json!({"moves": [{"reference": "R1", "by": [0.0, 5.0]}]}),
    );
    let move_revision = moved["revision"].as_u64().unwrap();
    assert_ne!(position(&ctx, "R1"), position_before_move);
    assert_ne!(std::fs::read(ctx.pcb_path()).unwrap(), board_before_move);

    tool(&ctx, "undo", json!({"revision": move_revision}));
    assert_eq!(position(&ctx, "R1"), position_before_move);
    assert_eq!(std::fs::read(ctx.pcb_path()).unwrap(), board_before_move);

    let history = tool(&ctx, "history", json!({"limit": 20}));
    let history = history.as_str().unwrap();
    for tool in [
        "place_parts",
        "set_fields",
        "sync_board",
        "move_parts",
        "undo",
    ] {
        assert!(history.contains(tool), "history omitted {tool}:\n{history}");
    }
}

/// A bare `undo` after the very first `place_parts` would restore the state from
/// before the schematic existed — which DELETES it, and every later tool then
/// answers "no schematic yet". Meaning that costs a revision number.
#[test]
fn a_bare_undo_refuses_to_delete_the_only_schematic() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let _ = std::fs::remove_file(ctx.sch_path());
    tool(
        &ctx,
        "place_parts",
        json!({"block": "divider", "parts": [
            {"ref": "R1", "part": "Device:R", "value": "10k", "footprint": R0805,
             "pins": {"1": "VIN", "2": "SENSE"}},
            {"ref": "R2", "part": "Device:R", "value": "10k", "footprint": R0805,
             "pins": {"1": "SENSE", "2": "GND"}}
        ]}),
    );

    let refused = run_tool("undo", json!({}), &ctx).unwrap();

    let error = refused["error"].as_str().unwrap_or_default();
    let sch = ctx.sch_path().display().to_string();
    assert!(error.contains(&sch), "{refused:#}");
    assert!(
        refused["would_remove"]
            .as_array()
            .is_some_and(|files| !files.is_empty()),
        "the refusal did not list what it would remove: {refused:#}"
    );
    assert!(ctx.sch_path().exists(), "the schematic was deleted anyway");

    // Named explicitly, it is what the caller meant.
    let revision = refused["revision_offered"].as_u64().unwrap();
    let done = run_tool("undo", json!({ "revision": revision }), &ctx).unwrap();
    assert!(done.get("error").is_none(), "{done:#}");
    assert!(!ctx.sch_path().exists(), "the explicit undo did nothing");
}
