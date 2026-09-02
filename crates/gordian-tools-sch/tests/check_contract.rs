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
            "classification",
            "severity",
            "source",
            "code",
            "message",
            "refs",
            "nets",
            "fix",
            "why",
        ] {
            assert!(
                finding.get(field).is_some(),
                "{field} missing from {finding}"
            );
        }
        assert!(
            finding["fix"].is_null()
                || finding["fix"].get("tool").and_then(Value::as_str).is_some()
                    && finding["fix"]
                        .get("args")
                        .and_then(Value::as_object)
                        .is_some(),
            "finding has no executable fix shape: {finding}"
        );
        assert!(
            finding["why"].as_str().is_some_and(|why| !why.is_empty()),
            "finding has no rationale: {finding}"
        );
    }
    let p1 = findings
        .iter()
        .find(|finding| finding["code"] == "footprint-unknown" && finding["refs"] == json!(["P1"]))
        .expect("P1 footprint finding");
    assert_eq!(p1["refs"], json!(["P1"]));
    assert_eq!(
        p1["fix"],
        json!({
            "tool": "assign_footprints",
            "args": {"assignments": [{
                "reference": "P1",
                "footprint": "TerminalBlock_Altech:Altech_AK300_1x02_P5.00mm_45-Degree"
            }]}
        })
    );
    assert_eq!(
        checked["fix_count"].as_u64().unwrap() as usize,
        checked["fix_groups"].as_array().unwrap().len()
    );
    assert!(
        checked["text"]
            .as_str()
            .unwrap()
            .lines()
            .next()
            .is_some_and(|line| line.contains("findings") && line.contains("fixes"))
    );
    assert_eq!(
        checked["text"].as_str().unwrap().lines().count(),
        findings.len() + 1,
        "detailed output must have a header plus one model-facing line per finding"
    );

    let compact = gordian_tools_sch::run("check_schematic", json!({}), &ctx)
        .expect("registered tool")
        .expect("compact check_schematic result");
    if findings.len() > 40 {
        assert_eq!(compact["findings"].as_array().unwrap().len(), 39);
        assert_eq!(compact["text"].as_str().unwrap().lines().count(), 41);
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

#[test]
fn check_schematic_uses_the_turn_snapshot_and_survives_undo() {
    let Some(ctx) = passive_fixture() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    ctx.begin_turn().unwrap();
    let revision = ctx
        .revisions()
        .capture(gordian_runtime::revisions::Capture::new(
            "test_edit",
            "capture turn baseline",
            &[ctx.sch_path().to_path_buf()],
        ))
        .unwrap();

    let unchanged = gordian_tools_sch::run("check_schematic", json!({"detail": true}), &ctx)
        .unwrap()
        .unwrap();
    assert_eq!(unchanged["baseline_revision"], json!(revision));
    assert_eq!(unchanged["introduced"], 0);
    assert_eq!(
        unchanged["pre_existing"],
        unchanged["findings"].as_array().unwrap().len()
    );

    let original = std::fs::read_to_string(ctx.sch_path()).unwrap();
    let edited = original.replacen(
        "Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm_P7.62mm_Horizontal",
        "Missing:Footprint",
        1,
    );
    assert_ne!(edited, original);
    std::fs::write(ctx.sch_path(), edited).unwrap();
    let changed = gordian_tools_sch::run("check_schematic", json!({"detail": true}), &ctx)
        .unwrap()
        .unwrap();
    assert!(
        changed["introduced"].as_u64().unwrap() > 0,
        "edited sheet should have introduced findings: {changed}"
    );

    ctx.revisions().restore(Some(revision)).unwrap();
    let undone = gordian_tools_sch::run("check_schematic", json!({"detail": true}), &ctx)
        .unwrap()
        .unwrap();
    assert_eq!(undone["introduced"], 0);
}
