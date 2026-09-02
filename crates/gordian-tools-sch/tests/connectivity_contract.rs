//! Extractor-backed connectivity contracts for symbol-creating mutators.

use std::path::Path;

use gordian_runtime::AgentRuntime;
use serde_json::{Value, json};

fn passive_fixture() -> Option<AgentRuntime> {
    let ctx = AgentRuntime::detect_for_test()?;
    let input =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../quality/cases/sch-replace-passive/input");
    std::fs::copy(input.join("design.kicad_sch"), ctx.sch_path()).unwrap();
    std::fs::copy(
        input.join("design.kicad_pro"),
        ctx.sch_path().with_extension("kicad_pro"),
    )
    .unwrap();
    Some(ctx)
}

fn tool(ctx: &AgentRuntime, name: &str, input: Value) -> Value {
    gordian_tools_sch::run(name, input, ctx)
        .unwrap_or_else(|| panic!("{name} is not registered"))
        .unwrap_or_else(|error| panic!("{name} failed: {error}"))
}

fn assert_success(name: &str, result: &Value) {
    assert!(
        result.get("error").is_none() && result.get("ok").and_then(Value::as_bool) != Some(false),
        "{name} failed: {result:#}"
    );
}

#[test]
fn add_and_swap_report_realized_connectivity() {
    let Some(ctx) = passive_fixture() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    let added = tool(
        &ctx,
        "add_symbols",
        json!({"parts": [{"lib_id": "Device:R", "ref": "R5", "value": "10k"}]}),
    );
    assert_success("add_symbols", &added);
    assert_eq!(added["connectivity"], json!(["R5:"]));
    assert_eq!(added["unconnected"], json!(["R5.1", "R5.2"]));
    assert_eq!(
        added["text"],
        json!("ADDED  R5\nCONNECTIVITY\nR5:\nUNCONNECTED  R5.1 R5.2")
    );

    let swapped = tool(
        &ctx,
        "swap_symbol",
        json!({"ref": "R1", "lib_id": "Device:R"}),
    );
    assert_success("swap_symbol", &swapped);
    let line = swapped["connectivity"][0].as_str().unwrap();
    assert!(line.starts_with("R1: 1="), "{swapped:#}");
    assert!(line.contains(" 2="), "{swapped:#}");
    assert_eq!(swapped["unconnected"], json!([]));
    assert!(
        swapped["text"]
            .as_str()
            .is_some_and(|text| text.contains("SWAPPED  R1 → Device:R\nCONNECTIVITY\nR1:")),
        "{swapped:#}"
    );
}

#[test]
fn place_parts_reports_extracted_pin_nets() {
    let Some(ctx) = passive_fixture() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    let placed = tool(
        &ctx,
        "place_parts",
        json!({
            "block": "test_point",
            "parts": [{
                "ref": "R5",
                "part": "Device:R",
                "value": "10k",
                "pins": {"1": "TEST_OUT", "2": "GND"}
            }]
        }),
    );
    assert_success("place_parts", &placed);
    assert_eq!(placed["connectivity"], json!(["R5: 2=GND"]));
    assert_eq!(placed["unconnected"], json!(["R5.1"]));
    assert!(
        placed["text"]
            .as_str()
            .is_some_and(|text| text.contains("CONNECTIVITY\nR5: 2=GND\nUNCONNECTED  R5.1")),
        "{placed:#}"
    );
}
