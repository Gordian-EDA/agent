//! One-call schematic repair through the public tool dispatcher.

use std::path::Path;

use gordian_core::AgentRuntime;
use gordian_core::tools::run_tool;
use serde_json::{Value, json};

fn tool(ctx: &AgentRuntime, name: &str, input: Value) -> Value {
    let output = run_tool(name, input, ctx).unwrap_or_else(|error| panic!("{name}: {error}"));
    assert!(output.get("error").is_none(), "{name} failed: {output:#}");
    output
}

#[test]
fn returned_fix_closes_a_broken_passive_connection_verbatim() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    let input =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../quality/cases/sch-replace-passive/input");
    std::fs::copy(input.join("design.kicad_sch"), ctx.sch_path()).unwrap();
    std::fs::copy(
        input.join("design.kicad_pro"),
        ctx.sch_path().with_extension("kicad_pro"),
    )
    .unwrap();
    ctx.begin_turn().unwrap();

    tool(&ctx, "delete_wires", json!({"pins": ["R2.1"]}));
    let broken = tool(&ctx, "check_schematic", json!({"detail": true}));
    let fix = broken["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|finding| {
            finding["classification"] == "introduced"
                && finding["severity"] == "error"
                && finding.pointer("/fix/tool") == Some(&json!("connect"))
        })
        .and_then(|finding| finding["fix"].as_object())
        .expect("broken connection must have an executable connect fix");
    let name = fix["tool"].as_str().unwrap();
    let args = fix["args"].clone();
    assert_eq!(name, "connect");
    assert_eq!(args, json!({"from": "R2.1", "to": "U1.3"}));
    assert!(
        broken["fix_groups"]
            .as_array()
            .unwrap()
            .iter()
            .any(|group| {
                group["fix"] == json!({"tool": name, "args": args})
                    && group["closes"].as_u64().is_some_and(|count| count >= 3)
            })
    );

    tool(&ctx, name, args.clone());
    let repaired = tool(&ctx, "check_schematic", json!({"detail": true}));

    assert_eq!(
        repaired["introduced"], 0,
        "verbatim fix left introduced findings: {repaired:#}"
    );
    assert_eq!(repaired["ok"], true, "repaired schematic is not clean");
}

#[test]
fn returned_fix_closes_reversed_led_polarity_verbatim() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    tool(
        &ctx,
        "place_parts",
        json!({"block": "indicator", "parts": [
            {
                "ref": "R1",
                "part": "Device:R",
                "value": "1k",
                "footprint": "Resistor_SMD:R_0603_1608Metric",
                "pins": {"1": "+3V3", "2": "LED_K"}
            },
            {
                "ref": "D1",
                "part": "Device:LED",
                "footprint": "LED_SMD:LED_0603_1608Metric",
                "pins": {"1": "LED_K", "2": "GND"}
            }
        ]}),
    );
    let broken = tool(&ctx, "check_schematic", json!({"detail": true}));
    let finding = broken["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|finding| finding["code"] == "led-polarity")
        .expect("reversed LED finding");
    assert_eq!(
        finding["fix"],
        json!({
            "tool": "move_symbols",
            "args": {"moves": [{"ref": "D1", "rot": 90.0}]}
        })
    );

    let applied = tool(
        &ctx,
        finding.pointer("/fix/tool").unwrap().as_str().unwrap(),
        finding.pointer("/fix/args").unwrap().clone(),
    );
    assert!(
        applied.pointer("/changed/moved/0/nudged_to").is_none(),
        "an in-place polarity fix must not move off its fixed nets: {applied:#}"
    );
    let repaired = tool(&ctx, "check_schematic", json!({"detail": true}));

    assert_eq!(
        repaired["erc_clean"], true,
        "repair was not clean: {repaired:#}"
    );
    assert!(
        repaired["findings"]
            .as_array()
            .unwrap()
            .iter()
            .all(|finding| finding["code"] != "led-polarity"),
        "verbatim fix left LED polarity finding: {repaired:#}"
    );
}
