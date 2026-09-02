//! Contract tests for source-complete schematic findings and KiCad ERC counts.

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

#[test]
fn check_schematic_matches_kicad_warning_count_and_names_each_finding() {
    let Some(ctx) = passive_fixture() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };

    let direct = ctx.env().erc(ctx.sch_path()).expect("direct KiCad ERC");
    let checked = gordian_tools_sch::run("check_schematic", json!({"detail": true}), &ctx)
        .expect("registered tool")
        .expect("check_schematic result");

    assert_eq!(
        checked.pointer("/erc/warnings").and_then(Value::as_u64),
        Some(direct.warning_count() as u64)
    );
    assert_eq!(
        checked.get("warnings").and_then(Value::as_u64),
        Some(direct.warning_count() as u64)
    );
    let findings = checked["findings"].as_array().expect("structured findings");
    assert_eq!(
        findings
            .iter()
            .filter(|finding| {
                finding["source"] == "kicad_erc" && finding["severity"] == "warning"
            })
            .count(),
        direct.warning_count(),
        "every KiCad warning must remain represented: {checked}"
    );
    for finding in findings {
        for field in [
            "severity", "source", "code", "message", "refs", "nets", "fix",
        ] {
            assert!(
                finding.get(field).is_some(),
                "{field} missing from {finding}"
            );
        }
        assert!(
            finding["fix"].as_str().is_some_and(|fix| !fix.is_empty()),
            "finding has no fix: {finding}"
        );
    }
    let p1 = findings
        .iter()
        .find(|finding| {
            finding["code"] == "footprint-unknown"
                && finding["refs"] == json!(["P1"])
        })
        .expect("P1 footprint finding");
    assert_eq!(p1["refs"], json!(["P1"]));
    assert_eq!(
        checked["text"].as_str().unwrap().lines().count(),
        findings.len(),
        "detailed output must have one model-facing line per finding"
    );

    let compact = gordian_tools_sch::run("check_schematic", json!({}), &ctx)
        .expect("registered tool")
        .expect("compact check_schematic result");
    if findings.len() > 40 {
        assert_eq!(compact["findings"].as_array().unwrap().len(), 40);
        assert!(
            compact["text"]
                .as_str()
                .unwrap()
                .lines()
                .last()
                .is_some_and(|line| line.starts_with('+') && line.contains("detail")),
            "compact result must say how to retrieve omitted findings: {compact}"
        );
    }
}
