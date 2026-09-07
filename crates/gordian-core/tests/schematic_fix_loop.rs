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
    tool(&ctx, "delete_wires", json!({"pins": ["R2.1"]}));
    let broken = tool(&ctx, "check_schematic", json!({"detail": true}));
    let fix = broken["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|finding| {
            finding["severity"] == "error"
                && finding.pointer("/fix/tool") == Some(&json!("connect"))
        })
        .and_then(|finding| finding["fix"].as_object())
        .expect("broken connection must have an executable connect fix");
    let broken_errors = broken["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|finding| finding["severity"] == "error")
        .count();
    let name = fix["tool"].as_str().unwrap();
    let args = fix["args"].clone();
    assert_eq!(name, "connect");
    assert_eq!(args["from"], "R2.1");
    assert!(
        broken["fix_groups"]
            .as_array()
            .unwrap()
            .iter()
            .any(|group| group["fix"] == json!({"tool": name, "args": args}))
    );

    tool(&ctx, name, args.clone());
    let repaired = tool(&ctx, "check_schematic", json!({"detail": true}));

    let repaired_errors = repaired["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|finding| finding["severity"] == "error")
        .count();
    assert!(repaired_errors < broken_errors, "{repaired:#}");
}

#[test]
fn a_reversed_led_is_repaired_by_the_half_turn_the_finding_names() {
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
    let fix = finding["fix"].clone();
    assert_eq!(fix["tool"], "swap_symbol", "{finding:#}");
    assert_eq!(fix["args"]["ref"], "D1", "{finding:#}");
    assert_eq!(fix["args"]["pin_map"], json!({"1": "2", "2": "1"}), "{finding:#}");

    let swapped = tool(&ctx, "swap_symbol", fix["args"].clone());
    assert!(swapped.get("error").is_none(), "{swapped:#}");
    let repaired = tool(&ctx, "check_schematic", json!({"detail": true}));
    assert!(
        !repaired["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["code"] == "led-polarity"),
        "the named fix must clear the finding it was attached to: {repaired:#}"
    );
}

/// An electrical rule the planner cannot turn into a call must be reported, not
/// blocked on — a blocking finding with no fix is what drove the agent to delete
/// the parts and rails the request named.
#[test]
fn an_unrepairable_electrical_rule_is_reported_instead_of_blocking() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    tool(
        &ctx,
        "place_parts",
        json!({"block": "shorted", "parts": [
            {
                "ref": "C1",
                "part": "Device:C",
                "value": "100n",
                "footprint": "Capacitor_SMD:C_0603_1608Metric",
                "pins": {"1": "GND", "2": "GND"}
            }
        ]}),
    );
    let report = tool(&ctx, "check_schematic", json!({"detail": true}));
    let reported = report["reported_not_blocking"]
        .as_array()
        .expect("a fix-less electrical rule is listed as reported")
        .clone();
    assert!(
        reported
            .iter()
            .any(|finding| finding["code"] == "dangling-passive"),
        "{report:#}"
    );
    assert!(
        !report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["code"] == "dangling-passive" && finding["severity"] == "error"),
        "{report:#}"
    );
    assert!(
        reported[0]["why"]
            .as_str()
            .unwrap()
            .contains("not a repair")
            || reported[0]["why"]
                .as_str()
                .unwrap()
                .contains("do not delete"),
        "{reported:#?}"
    );
}
