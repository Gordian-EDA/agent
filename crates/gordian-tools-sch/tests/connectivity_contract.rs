//! Extractor-backed connectivity contracts for symbol-creating mutators.

use std::path::Path;

use gordian_runtime::AgentRuntime;
use serde_json::{Value, json};
use std::collections::BTreeSet;

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
        "place_parts",
        json!({"parts": [{"lib_id": "Device:R", "ref": "R5", "value": "10k"}]}),
    );
    assert_success("place_parts", &added);
    assert_eq!(added["connectivity"], json!(["R5:"]));
    assert_eq!(added["unconnected"], json!(["R5.1", "R5.2"]));
    assert_eq!(
        added["text"],
        json!("PLACED  R5\nCONNECTIVITY\nR5:\nUNCONNECTED  R5.1 R5.2")
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
    assert_eq!(placed["connectivity"], json!(["R5: 1=TEST_OUT 2=GND"]));
    assert_eq!(placed["unconnected"], json!([]));
    assert!(
        placed["text"]
            .as_str()
            .is_some_and(|text| text.contains("CONNECTIVITY\nR5: 1=TEST_OUT 2=GND")),
        "{placed:#}"
    );
}

#[test]
fn connector_swap_reflows_fields_without_moving_the_part() {
    let Some(ctx) = passive_fixture() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    let collisions = |ctx: &AgentRuntime| {
        sch_floorplan::visual::measure(&sch_doc::SchDoc::read(ctx.sch_path()).unwrap())
            .text_collisions
            .into_iter()
            .map(|collision| (collision.reference, collision.field, collision.with))
            .collect::<BTreeSet<_>>()
    };
    let before_doc = sch_doc::SchDoc::read(ctx.sch_path()).unwrap();
    let before_at = before_doc.symbol_by_ref("P1").unwrap().at;
    let before_nets = sch_doc::connect::extract(&before_doc);
    let before = collisions(&ctx);
    let swapped = tool(
        &ctx,
        "swap_symbol",
        json!({
            "ref": "P1",
            "lib_id": "Connector:Conn_01x03_Pin",
            "value": "IN",
            "pin_map": {"1": "1", "2": "2"}
        }),
    );
    assert_success("swap_symbol", &swapped);
    let after_swap: Vec<_> = collisions(&ctx).difference(&before).cloned().collect();
    assert!(
        after_swap.is_empty(),
        "the swap must not smear text: {after_swap:?}"
    );
    // Every re-seated label is re-anchored and re-oriented, which is exactly
    // where a silent short is manufactured: a label binds to the point it sits
    // on. The swap preserves the pins it maps, so the nets must survive it.
    let swapped_doc = sch_doc::SchDoc::read(ctx.sch_path()).unwrap();
    let delta =
        sch_doc::connect::Netlist::diff(&before_nets, &sch_doc::connect::extract(&swapped_doc));
    assert!(
        delta.is_empty(),
        "the swap must preserve every net: {delta:?}"
    );

    let powered = tool(&ctx, "connect", json!({"net": "GND", "pin": "P1.3"}));
    assert_success("connect", &powered);
    let after_doc = sch_doc::SchDoc::read(ctx.sch_path()).unwrap();
    assert_eq!(after_doc.symbol_by_ref("P1").unwrap().at, before_at);
    let added: Vec<_> = collisions(&ctx).difference(&before).cloned().collect();
    assert!(added.is_empty(), "new collisions: {added:?}");
}

/// A name put on a pin by `connect` reads outward from the pin, the way the pin's own
/// label pose has it — never back across the symbol — and asking twice writes it once.
#[test]
fn a_named_pin_reads_outward_and_a_repeat_writes_no_second_label() {
    let Some(ctx) = passive_fixture() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    let placed = tool(
        &ctx,
        "place_parts",
        json!({
            "block": "probe",
            "parts": [{
                "ref": "R5",
                "part": "Device:R",
                "value": "10k",
                "pins": {"1": "N$9", "2": "GND"}
            }]
        }),
    );
    assert_success("place_parts", &placed);
    let named = tool(&ctx, "connect", json!({"from": "R5.1", "net": "PROBE_A"}));
    assert_success("connect", &named);
    // Asked again the tool may decline (the pin already carries the name) or comply;
    // either way the sheet holds one label.
    let _ = tool(&ctx, "connect", json!({"from": "R5.1", "net": "PROBE_A"}));
    let mut doc = sch_doc::SchDoc::read(ctx.sch_path()).unwrap();
    let at = doc
        .labels()
        .find(|label| sch_doc::unescape(&label.text) == "PROBE_A")
        .map(|label| label.at)
        .expect("PROBE_A is labelled");
    doc.add_label(sch_doc::LabelKind::Local, "PROBE_A", at);
    let pin = sch_doc::placed_pins(&doc)
        .into_iter()
        .find(|pin| pin.refdes == "R5" && pin.number == "1")
        .expect("R5.1 is on the sheet");
    let labels: Vec<&sch_doc::Label> = doc
        .labels()
        .filter(|label| sch_doc::unescape(&label.text) == "PROBE_A")
        .collect();
    assert_eq!(labels.len(), 1, "{labels:?}");
    assert_eq!(labels[0].at.rot, pin.label_pose().rot, "label reads back over the body");
}
