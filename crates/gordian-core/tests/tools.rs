//! Deterministic tests for the six-tool registry (no LLM, no network).
//!
//! Every test is SKIP-graceful: if no KiCAD installation is detected,
//! [`AgentRuntime::detect_for_test`] returns `None` and the test prints `SKIP` and
//! returns rather than failing. The tests that touch real symbol libraries and
//! `kicad-cli` therefore only assert on machines with KiCAD installed (the
//! project's test environment has KiCAD 10.0.3).

use gordian_core::AgentRuntime;
use gordian_core::tools::{run_tool, tool_defs};
use kicad_env::KicadEnv;

/// A tiny self-contained valid design: one resistor between two named nets.
const TINY_YAML: &str =
    "version: 1\nblocks: {main: {components: {R1: {part: Device:R, pins: {1: A, 2: GND}}}}}";

fn seed_draft(ctx: &AgentRuntime, yaml: &str) {
    ctx.workspace().write_draft(yaml, None).unwrap();
}

#[test]
fn search_symbols_tool_finds_stm32() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let out = run_tool(
        "search_symbols",
        serde_json::json!({"query":"STM32H743VI"}),
        &ctx,
    )
    .unwrap();
    assert!(
        out.to_string().contains("STM32H743VITx"),
        "expected STM32H743VITx in hits, got: {out}"
    );
}

#[test]
fn search_symbols_batches_four_labeled_queries() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let out = run_tool(
        "search_symbols",
        serde_json::json!({"queries": [
            {"query": "Device:R", "limit": 1},
            {"query": "Device:C", "limit": 1},
            {"query": "2-pin header", "limit": 1},
            {"query": "power:GND", "limit": 1}
        ]}),
        &ctx,
    )
    .unwrap();
    let results = out["results"].as_array().expect("batched results");
    assert_eq!(results.len(), 4);
    assert_eq!(results[2]["query"], "2-pin header");
    assert_eq!(results[2]["hits"][0]["lib_id"], "Connector:Conn_01x02_Pin");
}

#[test]
fn search_symbols_canonicalizes_common_single_row_connectors() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    for (query, expected, pin_count) in [
        ("2-pin header", "Connector:Conn_01x02_Pin", 2),
        (
            "PinHeader_1x03_P2.54mm_Vertical",
            "Connector:Conn_01x03_Pin",
            3,
        ),
        ("1x04 connector", "Connector:Conn_01x04_Pin", 4),
        (
            "Connector_Generic:Conn_01x05",
            "Connector:Conn_01x05_Pin",
            5,
        ),
        ("Connector:Conn_01x06_Pin", "Connector:Conn_01x06_Pin", 6),
    ] {
        let out = run_tool(
            "search_symbols",
            serde_json::json!({ "query": query }),
            &ctx,
        )
        .unwrap();
        assert_eq!(
            out["hits"],
            serde_json::json!([{ "lib_id": expected, "pin_count": pin_count }]),
            "query {query:?}: {out}"
        );
        let note = out["note"].as_str().expect("connector guidance note");
        assert!(note.contains("separate"), "query {query:?}: {out}");
        assert!(note.contains("search_footprints"), "query {query:?}: {out}");
    }
}

#[test]
fn validate_design_tool_reports_errors_for_bad_part() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let yaml = "version: 1\nblocks: {main: {components: {U1: {part: No:Such, pins: {}}}}}";
    let out = run_tool("validate_design", serde_json::json!({ "yaml": yaml }), &ctx).unwrap();
    let s = out.to_string();
    assert!(
        s.contains("unknown-part") || s.contains("not found"),
        "expected an unknown-part diagnostic, got: {s}"
    );
    assert_eq!(out["ok"], serde_json::json!(false));
}

#[test]
fn authoring_results_include_compact_sorted_design_state() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let yaml = r#"
version: 1
blocks:
  z:
    components:
      R2: {part: Device:R, pins: {1: ZETA, 2: GND}}
  a:
    components:
      C1: {part: Device:C, pins: {1: ALPHA, 2: GND}}
      R1: {part: Device:R, pins: {1: ALPHA, 2: ZETA}}
"#;
    let expected = serde_json::json!({
        "component_count": 3,
        "refdes": ["C1", "R1", "R2"],
        "net_count": 3,
        "net_names": ["ALPHA", "GND", "ZETA"],
    });

    let created = run_tool("create_design", serde_json::json!({ "yaml": yaml }), &ctx).unwrap();
    assert_eq!(created["design_state"], expected, "{created}");

    let validated = run_tool("validate_design", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(validated["design_state"], expected, "{validated}");

    let edited = run_tool(
        "edit_design",
        serde_json::json!({ "old_string": "R2:", "new_string": "R3:" }),
        &ctx,
    )
    .unwrap();
    assert_eq!(
        edited["design_state"]["refdes"],
        serde_json::json!(["C1", "R1", "R3"]),
        "{edited}"
    );
}

#[test]
fn apply_preview_and_commit_include_design_state() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let expected = serde_json::json!({
        "component_count": 1,
        "refdes": ["R1"],
        "net_count": 2,
        "net_names": ["A", "GND"],
    });

    seed_draft(&ctx, TINY_YAML);
    let preview = run_tool("apply_design", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(preview["design_state"], expected, "{preview}");

    let committed = run_tool(
        "apply_design",
        serde_json::json!({ "__commit": true }),
        &ctx,
    )
    .unwrap();
    assert_eq!(committed["design_state"], expected, "{committed}");
}

#[test]
fn empty_design_is_never_ready_or_written() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let empty = "version: 1\nname: empty\nblocks: {main: {components: {}}}\nnets: {}";

    let created = run_tool("create_design", serde_json::json!({"yaml": empty}), &ctx).unwrap();
    assert_eq!(created["ok"], false, "{created}");
    assert_eq!(created["draft_written"], false, "{created}");
    assert_eq!(created["draft_changed"], false, "{created}");
    assert!(created.to_string().contains("empty_design"), "{created}");
    assert!(
        !ctx.workspace().draft_path().exists(),
        "an empty create must not leave a draft file"
    );

    let preview = run_tool("apply_design", serde_json::json!({}), &ctx).unwrap();
    assert!(
        preview["error"]
            .as_str()
            .is_some_and(|e| e.contains("no draft"))
    );
    assert!(preview.get("would_write").is_none(), "{preview}");

    let committed = run_tool("apply_design", serde_json::json!({"__commit": true}), &ctx).unwrap();
    assert!(
        committed["error"]
            .as_str()
            .is_some_and(|e| e.contains("no draft"))
    );
    assert!(committed.get("written").is_none(), "{committed}");
    assert!(
        !ctx.sch_path().exists(),
        "empty schematic must not be written"
    );
}

#[test]
fn full_yaml_edit_seeds_a_new_project_and_reports_idempotence() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    assert_eq!(ctx.workspace().read_draft().unwrap(), None);

    let seeded = run_tool(
        "edit_design",
        serde_json::json!({ "yaml": TINY_YAML }),
        &ctx,
    )
    .unwrap();
    assert_eq!(seeded["ok"], true, "{seeded}");
    assert_eq!(seeded["draft_written"], true, "{seeded}");
    assert_eq!(seeded["draft_changed"], true, "{seeded}");
    assert_eq!(seeded["mode"], "full_create", "{seeded}");
    assert_eq!(
        ctx.workspace().read_draft().unwrap().as_deref(),
        Some(TINY_YAML)
    );

    let identical = run_tool(
        "edit_design",
        serde_json::json!({ "yaml": TINY_YAML }),
        &ctx,
    )
    .unwrap();
    assert_eq!(identical["draft_written"], true, "{identical}");
    assert_eq!(identical["draft_changed"], false, "{identical}");
    assert_eq!(identical["mode"], "full_replace", "{identical}");

    let cosmetic = format!("# formatting-only rewrite\n{TINY_YAML}");
    let cosmetic_result =
        run_tool("edit_design", serde_json::json!({ "yaml": cosmetic }), &ctx).unwrap();
    assert_eq!(cosmetic_result["draft_changed"], true, "{cosmetic_result}");
    assert_eq!(
        cosmetic_result["electrical_design_changed"], false,
        "{cosmetic_result}"
    );
}

#[test]
fn full_yaml_edit_rejects_silent_component_loss() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let substantial = r#"
version: 1
blocks:
  main:
    components:
      R1: {part: Device:R, pins: {1: A, 2: B}}
      R2: {part: Device:R, pins: {1: B, 2: GND}}
      R3: {part: Device:R, pins: {1: A, 2: GND}}
"#;
    run_tool(
        "edit_design",
        serde_json::json!({ "yaml": substantial }),
        &ctx,
    )
    .unwrap();

    let rejected = run_tool(
        "edit_design",
        serde_json::json!({ "yaml": TINY_YAML }),
        &ctx,
    )
    .unwrap();
    assert_eq!(
        rejected["code"], "component_removal_requires_confirmation",
        "{rejected}"
    );
    assert_eq!(rejected["current_component_count"], 3, "{rejected}");
    assert_eq!(rejected["candidate_component_count"], 1, "{rejected}");
    assert_eq!(rejected["draft_written"], false, "{rejected}");
    assert_eq!(rejected["draft_changed"], false, "{rejected}");
    assert_eq!(rejected["ok"], false, "{rejected}");
    assert_eq!(rejected["warnings"], 2, "{rejected}");
    assert_eq!(
        rejected["diagnostics"].as_array().map(Vec::len),
        Some(2),
        "candidate diagnostics must survive the transactional rejection: {rejected}"
    );
    assert_eq!(
        rejected["current_design_state"]["component_count"], 3,
        "{rejected}"
    );
    assert_eq!(
        rejected["current_diagnostics"],
        serde_json::json!([]),
        "{rejected}"
    );
    assert_eq!(
        ctx.workspace().read_draft().unwrap().as_deref(),
        Some(substantial),
        "rejection must preserve the prior bytes"
    );
}

#[test]
fn full_yaml_edit_allows_confirmed_component_loss_and_same_count_repairs() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let pair = r#"version: 1
blocks: {main: {components: {
  R1: {part: Device:R, pins: {1: A, 2: B}},
  R2: {part: Device:R, pins: {1: B, 2: GND}}
}}}"#;
    let repaired_pair = pair.replace("R2:", "R3:");
    run_tool("edit_design", serde_json::json!({ "yaml": pair }), &ctx).unwrap();

    let repaired = run_tool(
        "edit_design",
        serde_json::json!({ "yaml": repaired_pair }),
        &ctx,
    )
    .unwrap();
    assert_eq!(repaired["draft_written"], true, "{repaired}");
    assert_eq!(repaired["draft_changed"], true, "{repaired}");

    let reduced = run_tool(
        "edit_design",
        serde_json::json!({
            "yaml": TINY_YAML,
            "allow_component_removal": true,
        }),
        &ctx,
    )
    .unwrap();
    assert_eq!(reduced["draft_written"], true, "{reduced}");
    assert_eq!(reduced["draft_changed"], true, "{reduced}");
    assert_eq!(
        ctx.workspace().read_draft().unwrap().as_deref(),
        Some(TINY_YAML)
    );
}

#[test]
fn full_yaml_edit_can_repair_an_invalid_prior_draft() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let invalid = "version: 1\nblocks: {main: {components: {U1: {part: No:Such}}}}";
    ctx.workspace().write_draft(invalid, None).unwrap();

    let repaired = run_tool(
        "edit_design",
        serde_json::json!({ "yaml": TINY_YAML }),
        &ctx,
    )
    .unwrap();
    assert_eq!(repaired["ok"], true, "{repaired}");
    assert_eq!(repaired["draft_written"], true, "{repaired}");
    assert_eq!(repaired["draft_changed"], true, "{repaired}");
    assert_eq!(
        ctx.workspace().read_draft().unwrap().as_deref(),
        Some(TINY_YAML)
    );
}

#[test]
fn invalid_full_yaml_replacement_preserves_a_valid_prior_draft() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    run_tool(
        "edit_design",
        serde_json::json!({ "yaml": TINY_YAML }),
        &ctx,
    )
    .unwrap();
    let invalid = "version: 1\nblocks: {main: {components: {U1: {part: No:Such}}}}";

    let rejected = run_tool("edit_design", serde_json::json!({ "yaml": invalid }), &ctx).unwrap();
    assert_eq!(
        rejected["code"], "invalid_replacement_preserved_draft",
        "{rejected}"
    );
    assert_eq!(rejected["draft_written"], false, "{rejected}");
    assert_eq!(rejected["draft_changed"], false, "{rejected}");
    assert!(
        rejected["diagnostics"]
            .as_array()
            .is_some_and(|diagnostics| !diagnostics.is_empty()),
        "candidate diagnostics must be returned: {rejected}"
    );
    assert_eq!(
        rejected["current_design_state"]["component_count"], 1,
        "{rejected}"
    );
    assert_eq!(
        ctx.workspace().read_draft().unwrap().as_deref(),
        Some(TINY_YAML)
    );
}

#[test]
fn empty_full_edits_and_overwrites_preserve_the_prior_draft() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let empty = "version: 1\nblocks: {main: {components: {}}}";

    let rejected_initial =
        run_tool("edit_design", serde_json::json!({ "yaml": empty }), &ctx).unwrap();
    assert_eq!(
        rejected_initial["draft_written"], false,
        "{rejected_initial}"
    );
    assert_eq!(
        rejected_initial["draft_changed"], false,
        "{rejected_initial}"
    );
    assert_eq!(
        rejected_initial["mode"], "full_create",
        "{rejected_initial}"
    );
    assert!(!ctx.workspace().draft_path().exists());

    let seeded = run_tool(
        "edit_design",
        serde_json::json!({ "yaml": TINY_YAML }),
        &ctx,
    )
    .unwrap();
    assert_eq!(seeded["draft_written"], true, "{seeded}");

    for (tool, input) in [
        ("edit_design", serde_json::json!({ "yaml": empty })),
        (
            "create_design",
            serde_json::json!({ "yaml": empty, "overwrite": true }),
        ),
    ] {
        let rejected = run_tool(tool, input, &ctx).unwrap();
        assert_eq!(rejected["draft_written"], false, "{tool}: {rejected}");
        assert_eq!(rejected["draft_changed"], false, "{tool}: {rejected}");
        assert!(
            rejected["next"]
                .as_str()
                .is_some_and(|next| next.contains("existing draft was preserved")),
            "{tool}: {rejected}"
        );
        assert_eq!(
            ctx.workspace().read_draft().unwrap().as_deref(),
            Some(TINY_YAML)
        );
    }
}

#[test]
fn authoring_reports_real_patch_and_create_changes() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let created = run_tool(
        "create_design",
        serde_json::json!({ "yaml": TINY_YAML }),
        &ctx,
    )
    .unwrap();
    assert_eq!(created["draft_changed"], true, "{created}");

    let recreated = run_tool(
        "create_design",
        serde_json::json!({ "yaml": TINY_YAML, "overwrite": true }),
        &ctx,
    )
    .unwrap();
    assert_eq!(recreated["draft_written"], true, "{recreated}");
    assert_eq!(recreated["draft_changed"], false, "{recreated}");

    let no_op_patch = run_tool(
        "edit_design",
        serde_json::json!({ "old_string": "Device:R", "new_string": "Device:R" }),
        &ctx,
    )
    .unwrap();
    assert_eq!(no_op_patch["replacements"], 1, "{no_op_patch}");
    assert_eq!(no_op_patch["draft_changed"], false, "{no_op_patch}");

    let changed_patch = run_tool(
        "edit_design",
        serde_json::json!({ "old_string": "R1", "new_string": "R2" }),
        &ctx,
    )
    .unwrap();
    assert_eq!(changed_patch["draft_changed"], true, "{changed_patch}");
}

#[test]
fn invalid_patch_preserves_a_valid_draft() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    run_tool(
        "create_design",
        serde_json::json!({ "yaml": TINY_YAML }),
        &ctx,
    )
    .unwrap();

    let out = run_tool(
        "edit_design",
        serde_json::json!({
            "old_string": "R1: {part: Device:R",
            "new_string": "[search needed]        pins:"
        }),
        &ctx,
    )
    .unwrap();

    assert_eq!(out["code"], "invalid_patch_preserved_draft", "{out}");
    assert_eq!(out["draft_changed"], false, "{out}");
    assert_eq!(
        ctx.workspace().read_draft().unwrap().as_deref(),
        Some(TINY_YAML),
        "the last valid design must survive a malformed patch"
    );
}

#[test]
fn authoring_rejects_guessed_or_unknown_footprint_ids() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    for footprint in [
        "not_a_library_id",
        "Resistor_SMD:Definitely_Not_A_Real_Package",
    ] {
        let yaml = format!(
            "version: 1\nblocks: {{main: {{components: {{R1: {{part: Device:R, footprint: '{footprint}', pins: {{1: A, 2: GND}}}}}}}}}}"
        );
        let out = run_tool(
            "edit_design",
            serde_json::json!({ "yaml": yaml, "allow_component_removal": true }),
            &ctx,
        )
        .unwrap();
        assert_eq!(out["ok"], false, "{out}");
        let text = out.to_string();
        assert!(
            text.contains("footprint"),
            "the guessed assignment must be diagnosed: {out}"
        );
        assert!(
            !text.contains("suggestions: \"") && !text.contains("suggestions: ,"),
            "a diagnostic must never end in a dangling suggestions clause: {out}"
        );
    }
}

#[test]
fn unknown_footprint_diagnostic_suggests_the_right_library() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let yaml = "version: 1\nblocks: {main: {components: {SW1: {part: Device:R, footprint: 'Button_SMD_SW_SPST:SW_SPST_TL3342', pins: {1: A, 2: GND}}}}}";
    let out = run_tool(
        "edit_design",
        serde_json::json!({ "yaml": yaml, "allow_component_removal": true }),
        &ctx,
    )
    .unwrap();
    assert_eq!(out["ok"], false, "{out}");
    assert!(
        out.to_string()
            .contains("suggestions: Button_Switch_SMD:SW_SPST_TL3342"),
        "the wrong-library id must suggest the exact-name match first: {out}"
    );
}

#[test]
fn nonempty_invalid_full_edit_still_persists_for_repair() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let invalid = "version: 1\nblocks: {main: {components: {U1: {part: No:Such}}}}";

    let out = run_tool("edit_design", serde_json::json!({ "yaml": invalid }), &ctx).unwrap();

    assert_eq!(out["ok"], false, "{out}");
    assert_eq!(out["draft_written"], true, "{out}");
    assert_eq!(out["draft_changed"], true, "{out}");
    assert_eq!(
        ctx.workspace().read_draft().unwrap().as_deref(),
        Some(invalid)
    );
}

#[test]
fn get_symbol_info_tool_returns_full_pin_table_for_stm32() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let out = run_tool(
        "get_symbol_info",
        serde_json::json!({ "lib_id": "MCU_ST_STM32H7:STM32H743VITx" }),
        &ctx,
    )
    .unwrap();
    let pins = out["pins"].as_array().expect("pins array");
    // The STM32H743VITx (LQFP-100) has 100 pins.
    assert!(
        pins.len() >= 90,
        "expected ~100 pins, got {}: {out}",
        pins.len()
    );
    // Each pin carries number/name/type/unit.
    let p0 = &pins[0];
    assert!(p0.get("number").is_some());
    assert!(p0.get("name").is_some());
    assert!(p0.get("type").is_some());
    assert!(p0.get("unit").is_some());
}

#[test]
fn get_symbol_info_surfaces_part_ratings_before_selection() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let out = run_tool(
        "get_symbol_info",
        serde_json::json!({ "lib_id": "Regulator_Linear:TPS73633DBV" }),
        &ctx,
    )
    .unwrap();

    assert!(
        out["description"]
            .as_str()
            .is_some_and(|description| description.contains("400mA")),
        "the model must see that this part cannot satisfy a >=500mA requirement: {out}"
    );
    assert!(out["datasheet"].as_str().is_some(), "{out}");
    assert_eq!(
        out["default_footprint"],
        serde_json::json!("Package_TO_SOT_SMD:SOT-23-5"),
        "{out}"
    );
}

#[test]
fn get_symbol_info_tool_suggests_for_unknown_part() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let out = run_tool(
        "get_symbol_info",
        serde_json::json!({ "lib_id": "Device:Resistorr" }),
        &ctx,
    )
    .unwrap();
    assert!(out.get("error").is_some(), "expected an error field: {out}");
    assert!(
        out.get("suggestions").is_some(),
        "expected suggestions: {out}"
    );
}

#[test]
fn apply_design_dry_run_returns_diff_without_writing() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    assert!(!ctx.sch_path().exists(), "fixture starts with no schematic");

    seed_draft(&ctx, TINY_YAML);
    let out = run_tool("apply_design", serde_json::json!({}), &ctx).unwrap();

    assert_eq!(out["ok"], serde_json::json!(true), "got: {out}");
    assert_eq!(out["would_write"], serde_json::json!(true), "got: {out}");
    // R1 is a brand-new refdes against an empty prior.
    let added = out["diff"]["added"].as_array().expect("diff.added");
    assert!(
        added.iter().any(|v| v == "R1"),
        "expected R1 in added, got: {out}"
    );
    // Dry-run must NOT create the schematic file.
    assert!(
        !ctx.sch_path().exists(),
        "dry-run must not write the schematic"
    );
}

#[test]
fn gated_apply_preview_defers_layout_until_commit() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    seed_draft(&ctx, TINY_YAML);

    let out = run_tool(
        "apply_design",
        serde_json::json!({
            "__commit": false,
            "__skip_layout_preview": true
        }),
        &ctx,
    )
    .unwrap();

    assert_eq!(out["ok"], serde_json::json!(true), "{out}");
    assert_eq!(out["would_write"], serde_json::json!(true), "{out}");
    assert_eq!(out["layout_pending"], serde_json::json!(true), "{out}");
    assert!(out.get("layout_warnings").is_none(), "{out}");
    assert!(!ctx.sch_path().exists(), "preview must not write");
}

#[test]
fn apply_design_rejects_inline_yaml_without_mutating_the_draft() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    seed_draft(&ctx, TINY_YAML);

    let out = run_tool(
        "apply_design",
        serde_json::json!({ "yaml": "version: 1\nblocks: {}" }),
        &ctx,
    )
    .unwrap();

    assert_eq!(out["code"], "inline_apply_yaml_removed", "{out}");
    assert_eq!(
        ctx.workspace().read_draft().unwrap().as_deref(),
        Some(TINY_YAML)
    );
    assert!(!ctx.sch_path().exists(), "inline apply must not write");
}

#[test]
fn repair_components_replaces_a1_a2_with_jp1_without_resending_the_draft() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let yaml = "version: 1\nname: repair\nblocks:\n  main:\n    note: keep me\n    layout: [[A1, A2, R9]]\n    components:\n      A1: {part: Device:R, pins: {1: VIN, 2: MID}}\n      A2: {part: Device:R, pins: {1: MID, 2: GND}}\n      R9: {part: Device:R, value: 10k, pins: {1: VIN, 2: GND}}\n";
    seed_draft(&ctx, yaml);

    let out = run_tool(
        "repair_components",
        serde_json::json!({
            "remove": ["A1", "A2"],
            "upsert": {
                "JP1": {"part": "Device:R", "value": "0R", "pins": {"1": "VIN", "2": "GND"}}
            }
        }),
        &ctx,
    )
    .unwrap();

    assert_eq!(out["ok"], true, "{out}");
    assert_eq!(out["mode"], "component_repair", "{out}");
    assert_eq!(out["added"], serde_json::json!(["JP1"]), "{out}");
    assert_eq!(out["removed"], serde_json::json!(["A1", "A2"]), "{out}");
    let repaired = ctx.workspace().read_draft().unwrap().unwrap();
    assert!(repaired.contains("note: 'keep me'"), "{repaired}");
    assert!(repaired.contains("JP1:"), "{repaired}");
    assert!(repaired.contains("R9:"), "{repaired}");
    assert!(repaired.contains("layout: [[~, ~, R9]]"), "{repaired}");
    assert!(!repaired.contains("A1:"), "{repaired}");
    assert!(!repaired.contains("A2:"), "{repaired}");
}

#[test]
fn repair_components_requires_confirmation_only_for_part_changes() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let yaml = "version: 1\nblocks:\n  main:\n    components:\n      R1: {part: Device:R, value: 1k, footprint: Resistor_SMD:R_0603_1608Metric, dnp: true, props: {role: load}, pins: {1: A, 2: GND}}\n  aux:\n    components:\n      U1: {part: Device:R, value: 2k, pins: {1: B, 2: GND}}\n";
    seed_draft(&ctx, yaml);
    let before = std::fs::read(ctx.workspace().draft_path()).unwrap();

    let denied = run_tool(
        "repair_components",
        serde_json::json!({
            "components": {"R1": {"part": "Device:C", "value": "1uF", "pins": {"1": "A", "2": "GND"}}}
        }),
        &ctx,
    )
    .unwrap();
    assert_eq!(
        denied["code"],
        "component_replacement_requires_confirmation"
    );
    assert_eq!(std::fs::read(ctx.workspace().draft_path()).unwrap(), before);

    let routed = run_tool(
        "repair_components",
        serde_json::json!({
            "upsert": {"U1": {"part": "Device:R", "value": "3k", "pins": {"1": "B", "2": "GND"}}},
            "replace_existing": true
        }),
        &ctx,
    )
    .unwrap();
    assert_eq!(routed["replaced"], serde_json::json!(["U1"]), "{routed}");
    let routed_draft = ctx.workspace().read_draft().unwrap().unwrap();
    assert!(routed_draft.contains("value: 3k"), "{routed_draft}");

    let replaced = run_tool(
        "repair_components",
        serde_json::json!({
            "components": {"R1": {"part": "Device:R", "value": "4.7k"}}
        }),
        &ctx,
    )
    .unwrap();
    assert_eq!(
        replaced["replaced"],
        serde_json::json!(["R1"]),
        "{replaced}"
    );
    let repaired = ctx.workspace().read_draft().unwrap().unwrap();
    assert!(repaired.contains("value: 4.7k"), "{repaired}");
    assert!(
        repaired.contains("footprint: Resistor_SMD:R_0603_1608Metric"),
        "{repaired}"
    );
    assert!(repaired.contains("dnp: true"), "{repaired}");
    assert!(repaired.contains("role: load"), "{repaired}");
    assert!(repaired.contains("pins: {1: A, 2: GND}"), "{repaired}");
    assert!(repaired.contains("U1:"), "{repaired}");
}

#[test]
fn repair_components_preserves_bytes_on_invalid_fragment_or_footprint() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    seed_draft(&ctx, TINY_YAML);
    let before = std::fs::read(ctx.workspace().draft_path()).unwrap();

    let invalid = run_tool(
        "repair_components",
        serde_json::json!({"upsert": {"R2": {"part": "Device:R", "pins": {"3": "A"}}}}),
        &ctx,
    )
    .unwrap();
    assert_eq!(invalid["draft_written"], false, "{invalid}");
    assert_eq!(std::fs::read(ctx.workspace().draft_path()).unwrap(), before);

    let bad_footprint = run_tool(
        "repair_components",
        serde_json::json!({
            "upsert": {"R2": {"part": "Device:R", "footprint": "Missing:Nope", "pins": {"1": "A", "2": "GND"}}}
        }),
        &ctx,
    )
    .unwrap();
    assert_eq!(
        bad_footprint["code"],
        "invalid_component_repair_preserved_draft"
    );
    assert_eq!(std::fs::read(ctx.workspace().draft_path()).unwrap(), before);
}

#[test]
fn repair_components_updates_d1_and_tp1_without_complete_component_objects() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let yaml = "version: 1\nblocks:\n  main:\n    components:\n      D1: {part: Device:D, value: OLD, props: {role: clamp}, pins: {A: GND, K: VCC}}\n      TP1: {part: Connector:TestPoint, pins: {1: GND}}\n      R9: {part: Device:R, value: 10k, pins: {1: VCC, 2: GND}}\n";
    seed_draft(&ctx, yaml);

    let out = run_tool(
        "repair_components",
        serde_json::json!({
            "upsert": {},
            "update": {
                "D1": {"pins": {"A": "VCC", "K": "GND"}, "value": "1N4148"},
                "TP1": {"pins": {"1": "VCC"}}
            },
            "remove": [],
            "replace_existing": false
        }),
        &ctx,
    )
    .unwrap();

    assert_eq!(out["ok"], true, "{out}");
    assert_eq!(out["updated"], serde_json::json!(["D1", "TP1"]), "{out}");
    let repaired = ctx.workspace().read_draft().unwrap().unwrap();
    assert!(repaired.contains("value: 1N4148"), "{repaired}");
    assert!(repaired.contains("props: {role: clamp}"), "{repaired}");
    assert!(repaired.contains("pins: {A: VCC, K: GND}"), "{repaired}");
    assert!(
        repaired.contains("TP1: {part: Connector:TestPoint, pins: {1: VCC}}"),
        "{repaired}"
    );
    assert!(repaired.contains("R9:"), "{repaired}");
}

#[test]
fn repair_components_accepts_common_components_wrappers() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    seed_draft(&ctx, TINY_YAML);

    let top_level = run_tool(
        "repair_components",
        serde_json::json!({"components": {
            "C1": {"part": "Device:C", "pins": {"1": "VIN", "2": "GND"}}
        }}),
        &ctx,
    )
    .unwrap();
    assert_eq!(top_level["ok"], true, "{top_level}");

    let nested = run_tool(
        "repair_components",
        serde_json::json!({"upsert": {"components": {
            "R2": {"part": "Device:R", "pins": {"1": "VIN", "2": "GND"}}
        }}}),
        &ctx,
    )
    .unwrap();
    assert_eq!(nested["ok"], true, "{nested}");
}

#[test]
fn repair_components_update_failures_and_overlaps_preserve_exact_bytes() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let yaml = "version: 1\nblocks: {main: {components: {D1: {part: Device:D, pins: {1: VCC, 2: GND}}, TP1: {part: Connector:TestPoint, pins: {1: VCC}}}}}";
    seed_draft(&ctx, yaml);
    let before = std::fs::read(ctx.workspace().draft_path()).unwrap();

    let alias = run_tool(
        "repair_components",
        serde_json::json!({"update": {"D1": {"pins": {"A": "VCC", "K": "GND"}}}}),
        &ctx,
    )
    .unwrap();
    assert_eq!(alias["code"], "unknown_repair_pin_key", "{alias}");
    assert_eq!(alias["unknown_pin_keys"], serde_json::json!(["A", "K"]));
    assert_eq!(alias["valid_pin_keys"], serde_json::json!(["1", "2"]));
    assert_eq!(std::fs::read(ctx.workspace().draft_path()).unwrap(), before);

    for input in [
        serde_json::json!({
            "upsert": {}, "update": {"D1": {"pins": {"BAD": "SIG"}}},
            "remove": [], "replace_existing": false
        }),
        serde_json::json!({
            "upsert": {}, "update": {"D1": {"footprint": "Missing:Nope"}},
            "remove": [], "replace_existing": false
        }),
        serde_json::json!({
            "upsert": {"D1": {"part": "Device:D", "pins": {"A": "VCC", "K": "GND"}}},
            "update": {"D1": {"value": "1N4148"}},
            "remove": [], "replace_existing": true
        }),
        serde_json::json!({
            "upsert": {}, "update": {"TP1": {"pins": {"1": "GND"}}},
            "remove": ["TP1"], "replace_existing": false
        }),
    ] {
        let out = run_tool("repair_components", input, &ctx).unwrap();
        assert_eq!(out["draft_written"], false, "{out}");
        assert_eq!(std::fs::read(ctx.workspace().draft_path()).unwrap(), before);
    }
}

#[test]
fn repair_components_replaces_parent_and_removes_its_synthesized_children() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let yaml = "version: 1\nblocks:\n  main:\n    components:\n      U1: {part: Interface_CAN_LIN:SN65HVD230, decouple: {100nF: 2}, pins: {VCC: VCC, GND: GND}}\n      R1: {part: Device:R, pins: {1: VCC, 2: GND}}\n";
    seed_draft(&ctx, yaml);

    let out = run_tool(
        "repair_components",
        serde_json::json!({
            "upsert": {"U1": {"part": "Interface_CAN_LIN:SN65HVD230", "pins": {"VCC": "VCC", "GND": "GND"}}},
            "replace_existing": true
        }),
        &ctx,
    )
    .unwrap();
    assert_eq!(out["ok"], true, "{out}");
    let repaired = ctx.workspace().read_draft().unwrap().unwrap();
    assert!(!repaired.contains("decouple:"), "{repaired}");
    assert!(!repaired.contains("100nF"), "{repaired}");
    assert!(repaired.contains("R1:"), "{repaired}");
}

#[test]
fn apply_design_commit_writes_file_and_runs_erc() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    assert!(!ctx.sch_path().exists(), "fixture starts with no schematic");

    seed_draft(&ctx, TINY_YAML);
    let out = run_tool(
        "apply_design",
        serde_json::json!({ "__commit": true }),
        &ctx,
    )
    .unwrap();

    assert_eq!(out["ok"], serde_json::json!(true), "got: {out}");
    assert_eq!(out["written"], serde_json::json!(true), "got: {out}");
    assert!(
        ctx.sch_path().exists(),
        "commit must write the schematic file"
    );
    // ERC ran and reported structured counts.
    assert!(
        out["erc"]["errors"].is_number(),
        "expected erc.errors: {out}"
    );
    assert!(
        out["erc"]["warnings"].is_number(),
        "expected erc.warnings: {out}"
    );
    assert_eq!(out["erc_checked"], serde_json::json!(true), "{out}");
    if out["erc"]["errors"] == 0 && out["erc"]["warnings"] == 0 {
        assert_eq!(out["erc_clean"], serde_json::json!(true), "{out}");
        assert!(
            out["next"]
                .as_str()
                .is_some_and(|next| next.contains("do not call run_erc again")),
            "{out}"
        );
    }
}

#[test]
fn apply_design_reports_a_written_commit_when_post_write_erc_cannot_run() {
    let Some(mut env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD libraries detected");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    env.cli_path = dir.path().join("missing-kicad-cli");
    let sch_path = dir.path().join("failure.kicad_sch");
    let ctx = AgentRuntime::new_with_config(
        env,
        dir.path().to_path_buf(),
        sch_path.clone(),
        gordian_core::GordianConfig::default(),
    )
    .unwrap();

    seed_draft(&ctx, TINY_YAML);
    let out = run_tool(
        "apply_design",
        serde_json::json!({ "__commit": true }),
        &ctx,
    )
    .expect("post-write ERC failure must remain a structured committed result");

    assert!(sch_path.is_file(), "the irreversible write occurred: {out}");
    assert_eq!(out["written"], serde_json::json!(true), "{out}");
    assert_eq!(out["ok"], serde_json::json!(false), "{out}");
    assert!(out["erc"]["error"].is_string(), "{out}");
    assert!(
        out["error"]
            .as_str()
            .is_some_and(|error| error.contains("schematic was written")),
        "{out}"
    );
}

#[test]
fn read_schematic_draft_notes_absent_schematic() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let out = run_tool(
        "read_schematic",
        serde_json::json!({ "source": "draft" }),
        &ctx,
    )
    .unwrap();
    let text = out.as_str().expect("plain text read_schematic result");
    assert!(text.contains("source: draft"), "{text}");
    assert!(text.contains("stale: false"), "{text}");
    assert!(text.contains("note: no schematic yet"), "{text}");
    assert!(text.contains("```yaml\n\n```"), "{text}");
}

#[test]
fn defs_lists_all_tools() {
    let names: Vec<String> = tool_defs()
        .into_iter()
        .map(|d| d.name.to_string())
        .collect();
    let expected_tools = [
        "search_symbols",
        "get_symbol_info",
        "validate_design",
        "apply_design",
        "run_erc",
        "project_info",
        "read_schematic",
        "render_schematic",
        "create_design",
        "edit_design",
        "search_footprints",
        "get_footprint_info",
        "get_board",
        "place_board",
        "route_board",
        "render_board",
        "check_board",
        "export_fab",
        "review_design",
        "regenerate_board",
        "assign_footprints",
        "open_board",
        "move_parts",
        "route_track",
        "delete_copper",
        "set_net_width",
        "update_board_outline",
    ];
    for expected in expected_tools {
        assert!(names.contains(&expected.to_string()), "missing {expected}");
    }
    assert_eq!(
        names.len(),
        expected_tools.len(),
        "expected exactly {} tools, got {}: {:?}",
        expected_tools.len(),
        names.len(),
        names
    );

    // Names are unique.
    let mut sorted = names.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        names.len(),
        "tool names must be unique: {names:?}"
    );

    // Every def's schema is a JSON object with a "type":"object" root — schema
    // sanity for the model-facing definitions.
    for def in tool_defs() {
        let schema = def.schema.expect("every tool carries a JSON schema");
        assert_eq!(
            schema["type"],
            serde_json::json!("object"),
            "{} schema root must be an object",
            def.name
        );
        if def.name.to_string() == "apply_design" {
            let props = schema["properties"].as_object().expect("properties object");
            assert!(props.is_empty(), "apply accepts only the durable draft");
            assert_eq!(schema["additionalProperties"], false);
        }
        if def.name.to_string() == "edit_design" {
            assert_eq!(schema["additionalProperties"], false);
            // Both authoring shapes are advertised: full `yaml` replacement and
            // the exact-match patch that spares a large draft the 30k-token
            // resend that truncates weaker providers.
            let props = schema["properties"].as_object().expect("properties object");
            assert!(props.contains_key("yaml"));
            assert!(props.contains_key("old_string"));
            assert!(props.contains_key("new_string"));
        }
        if def.name.to_string() == "route_track" {
            let props = schema["properties"].as_object().expect("properties object");
            assert!(props.contains_key("from"));
            assert!(props.contains_key("to"));
            assert!(props.contains_key("net"));
            assert!(!props.contains_key("start"));
            assert!(!props.contains_key("end"));
        }
        if def.name.to_string() == "delete_copper" {
            let props = schema["properties"].as_object().expect("properties object");
            assert!(props.contains_key("at"));
            assert!(props.contains_key("kinds"));
        }
        if def.name.to_string() == "place_board" {
            assert_eq!(
                schema["properties"]["groups"]["items"]["properties"]["rotation"]["enum"],
                serde_json::json!([0, 90, 180, 270])
            );
        }
    }
}

#[test]
fn tool_definitions_stay_within_static_context_budget() {
    let defs = tool_defs();
    let total: usize = defs.iter().map(|tool| tool.size()).sum();
    assert!(
        total <= 8_350,
        "tool definitions use {total} bytes; keep the always-on schemas concise"
    );
}

#[test]
fn project_info_reports_paths_and_state() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let out = run_tool("project_info", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(
        out["sch_path"],
        serde_json::json!(ctx.sch_path().display().to_string())
    );
    assert_eq!(out["sch_exists"], serde_json::json!(false));
    assert_eq!(
        out["project_dir"],
        serde_json::json!(ctx.project_dir().display().to_string())
    );
    // After a commit the same tool reports the file as present.
    seed_draft(&ctx, TINY_YAML);
    run_tool(
        "apply_design",
        serde_json::json!({ "__commit": true }),
        &ctx,
    )
    .unwrap();
    let out = run_tool("project_info", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(out["sch_exists"], serde_json::json!(true), "got: {out}");
}

/// The two-resistor fixture board, shared with `kicad-cli`.
const TWO_RES_PCB: &str = include_str!("fixtures/two_res.kicad_pcb");

#[test]
fn export_fab_errors_without_a_board() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    // No board yet → a recoverable error pointing at regenerate_board.
    let out = run_tool("export_fab", serde_json::json!({}), &ctx).unwrap();
    let err = out["error"].as_str().unwrap_or_default();
    assert!(
        err.contains("regenerate_board"),
        "expected a regenerate_board hint, got: {out}"
    );
}

#[test]
fn export_fab_refuses_unclean_board() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    // Stand in an unrouted board at the project's default .kicad_pcb path.
    // export_fab must refuse it instead of bundling a board house package with
    // known missing copper.
    std::fs::write(ctx.pcb_path(), TWO_RES_PCB).unwrap();
    let out = run_tool("export_fab", serde_json::json!({}), &ctx).unwrap();

    assert_eq!(out["ok"], serde_json::json!(false), "got: {out}");
    assert!(
        out["error"].as_str().is_some_and(|e| e.contains("DRC")),
        "expected DRC refusal: {out}"
    );
    assert!(
        out["top_unconnected"]
            .as_array()
            .is_some_and(|v| !v.is_empty()),
        "expected actionable unconnected details: {out}"
    );
}

#[test]
fn read_schematic_lifts_an_external_file_by_absolute_path() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    // Write a real schematic into the project, then read it back as if it were
    // an arbitrary external path.
    seed_draft(&ctx, TINY_YAML);
    run_tool(
        "apply_design",
        serde_json::json!({ "__commit": true }),
        &ctx,
    )
    .unwrap();
    let abs = ctx.sch_path().display().to_string();
    let out = run_tool("read_schematic", serde_json::json!({ "source": abs }), &ctx).unwrap();
    let text = out.as_str().expect("plain text read_schematic result");
    assert!(text.contains("source: path"), "{text}");
    assert!(text.contains("path:"), "{text}");
    assert!(text.contains("R1"), "lifted yaml carries R1: {text}");
}

#[test]
fn read_schematic_resolves_relative_to_the_project_dir() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    seed_draft(&ctx, TINY_YAML);
    run_tool(
        "apply_design",
        serde_json::json!({ "__commit": true }),
        &ctx,
    )
    .unwrap();
    let rel = ctx.sch_path().file_name().unwrap().to_string_lossy();
    let out = run_tool("read_schematic", serde_json::json!({ "source": rel }), &ctx).unwrap();
    let text = out.as_str().expect("plain text read_schematic result");
    assert!(
        text.contains("R1"),
        "relative path resolves against the project dir: {text}"
    );
}

#[test]
fn read_schematic_errors_cleanly_for_missing_or_wrong_files() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };

    let out = run_tool(
        "read_schematic",
        serde_json::json!({ "source": "/no/such/file.kicad_sch" }),
        &ctx,
    )
    .unwrap();
    let text = out.as_str().expect("plain text read_schematic result");
    assert!(
        text.contains("error: no file"),
        "missing file is a text error: {text}"
    );

    let not_sch = ctx.project_dir().join("readme.txt");
    std::fs::write(&not_sch, "hello").unwrap();
    let out = run_tool(
        "read_schematic",
        serde_json::json!({ "source": not_sch.display().to_string() }),
        &ctx,
    )
    .unwrap();
    let text = out.as_str().expect("plain text read_schematic result");
    assert!(
        text.contains(".kicad_sch"),
        "wrong extension is a text error: {text}"
    );
}

#[test]
fn apply_design_commit_reports_the_written_path() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    seed_draft(&ctx, TINY_YAML);
    let out = run_tool(
        "apply_design",
        serde_json::json!({ "__commit": true }),
        &ctx,
    )
    .unwrap();
    assert_eq!(
        out["path"],
        serde_json::json!(ctx.sch_path().display().to_string()),
        "commit result carries the written path: {out}"
    );
}

#[test]
fn unknown_tool_is_an_error() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    assert!(run_tool("no_such_tool", serde_json::json!({}), &ctx).is_err());
}

#[test]
fn draft_lifecycle_create_edit_apply() {
    let Some(ctx) = gordian_core::AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let yaml = "
version: 1
name: t
blocks:
  a:
    components:
      R1: {part: Device:R, value: 1k, between: [N1, GND]}
      PWR1: {part: power:GND, pins: {1: GND}}
";

    // edit before create -> structured error.
    let out = run_tool(
        "edit_design",
        serde_json::json!({"old_string": "x", "new_string": "y"}),
        &ctx,
    )
    .unwrap();
    assert!(out["error"].as_str().unwrap().contains("no draft"));

    // create seeds the draft and validates it.
    let out = run_tool("create_design", serde_json::json!({"yaml": yaml}), &ctx).unwrap();
    assert_eq!(out["ok"], serde_json::json!(true));
    // create again without overwrite -> error; with overwrite -> ok.
    let out = run_tool("create_design", serde_json::json!({"yaml": yaml}), &ctx).unwrap();
    assert!(
        out["error"]
            .as_str()
            .unwrap()
            .contains("draft already exists")
    );

    // Anchored edit: ambiguity and uniqueness rules.
    let out = run_tool(
        "edit_design",
        serde_json::json!({"old_string": "NOT-PRESENT", "new_string": "y"}),
        &ctx,
    )
    .unwrap();
    assert!(out["error"].as_str().unwrap().contains("not found"));
    let out = run_tool(
        "edit_design",
        serde_json::json!({"old_string": "value: 1k", "new_string": "value: 4.7k"}),
        &ctx,
    )
    .unwrap();
    assert_eq!(out["ok"], serde_json::json!(true));
    assert_eq!(out["replacements"], serde_json::json!(1));

    let replacement_yaml = yaml.replace("value: 1k", "value: 2.2k");
    let out = run_tool(
        "edit_design",
        serde_json::json!({"yaml": replacement_yaml}),
        &ctx,
    )
    .unwrap();
    assert_eq!(out["ok"], serde_json::json!(true));
    assert_eq!(out["mode"], serde_json::json!("full_replace"));

    // apply_design applies the durable draft when the gate's commit phase invokes it.
    let out = run_tool("apply_design", serde_json::json!({"__commit": true}), &ctx).unwrap();
    assert_eq!(out["written"], serde_json::json!(true));

    // read_schematic(draft) prefers the draft and returns plain YAML text.
    let out = run_tool(
        "read_schematic",
        serde_json::json!({ "source": "draft" }),
        &ctx,
    )
    .unwrap();
    let text = out.as_str().expect("plain text read_schematic result");
    assert!(text.contains("source: draft"), "{text}");
    assert!(text.contains("2.2k"), "{text}");
}

#[test]
fn read_schematic_draft_seeds_from_lift_and_flags_staleness() {
    let Some(ctx) = gordian_core::AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let yaml = "
version: 1
name: t
blocks:
  a:
    components:
      R1: {part: Device:R, value: 1k, between: [N1, GND]}
      PWR1: {part: power:GND, pins: {1: GND}}
";
    // Write a schematic, then remove its draft to exercise lift-and-seed.
    seed_draft(&ctx, yaml);
    run_tool("apply_design", serde_json::json!({"__commit": true}), &ctx).unwrap();
    let draft_path = ctx.workspace().draft_path();
    std::fs::remove_file(&draft_path).unwrap();
    std::fs::remove_file(draft_path.parent().unwrap().join("draft.meta.json")).unwrap();

    // read_schematic(draft) lifts AND seeds the draft.
    let out = run_tool(
        "read_schematic",
        serde_json::json!({ "source": "draft" }),
        &ctx,
    )
    .unwrap();
    let text = out.as_str().expect("plain text read_schematic result");
    assert!(text.contains("source: draft"), "{text}");
    assert!(text.contains("stale: false"), "{text}");
    assert!(text.contains("draft seeded from the schematic"), "{text}");

    let out2 = run_tool(
        "read_schematic",
        serde_json::json!({ "source": "draft" }),
        &ctx,
    )
    .unwrap();
    let text2 = out2.as_str().expect("plain text read_schematic result");
    assert!(text2.contains("source: draft"), "{text2}");
    assert!(text2.contains("stale: false"), "{text2}");

    // Out-of-band sch edit -> staleness surfaces.
    let sch = std::fs::read_to_string(ctx.sch_path()).unwrap();
    std::fs::write(ctx.sch_path(), format!("{sch}\n")).unwrap();
    let out3 = run_tool(
        "read_schematic",
        serde_json::json!({ "source": "draft" }),
        &ctx,
    )
    .unwrap();
    let text3 = out3.as_str().expect("plain text read_schematic result");
    assert!(text3.contains("stale: true"), "{text3}");
}

#[test]
fn render_schematic_returns_png_and_image_path() {
    let Some(ctx) = gordian_core::AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };

    // No schematic yet -> structured error, no crash.
    let out = run_tool("render_schematic", serde_json::json!({}), &ctx).unwrap();
    assert!(out.get("error").is_some());

    // Write a minimal schematic via apply_design, then render it.
    let yaml = "
version: 1
name: t
blocks:
  a:
    components:
      R1: {part: Device:R, value: 1k, between: [N1, GND]}
      PWR1: {part: power:GND, pins: {1: GND}}
";
    seed_draft(&ctx, yaml);
    let applied = run_tool(
        "apply_design",
        serde_json::json!({ "__commit": true }),
        &ctx,
    )
    .unwrap();
    assert_eq!(applied["written"], serde_json::json!(true));

    let out = run_tool("render_schematic", serde_json::json!({}), &ctx).unwrap();
    let png_path = out["_image_path"].as_str().expect("image path");
    let bytes = std::fs::read(png_path).unwrap();
    assert_eq!(
        &bytes[..8],
        &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]
    );
    assert!(png_path.contains(".gordian/renders/render-001.png"));
}

#[test]
fn apply_design_surfaces_layout_warnings() {
    let Some(ctx) = gordian_core::AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let yaml = "
version: 1
name: t
blocks:
  a:
    components:
      R1: {part: Device:R, value: 1k, between: [N1, GND]}
      PWR1: {part: power:GND, pins: {1: GND}}
";
    // Commit so a prior schematic exists for the re-apply below.
    seed_draft(&ctx, yaml);
    let out = run_tool(
        "apply_design",
        serde_json::json!({ "__commit": true }),
        &ctx,
    )
    .unwrap();
    assert_eq!(out["written"], serde_json::json!(true));
    // The EmitOutput layout_warnings field is present in the result JSON.
    assert!(
        out["layout_warnings"].is_array(),
        "layout_warnings present: {out}"
    );
    // A clean single-R layout has no collisions.
    assert_eq!(out["layout_warnings"].as_array().unwrap().len(), 0);

    // Dry-run path also carries them.
    let dry = run_tool("apply_design", serde_json::json!({}), &ctx).unwrap();
    assert!(
        dry["layout_warnings"].is_array(),
        "dry-run layout_warnings: {dry}"
    );
}

// ── PCB tools (slice 5, Task 1) ──────────────────────────────────────────────
//
// These tests source the footprint index from the THREE vendored kicad-bridge
// fixtures via the `with_footprint_dir_for_test` override, so they run without an
// installed KiCAD footprint library. The override expects a directory of
// `.pretty` libraries, so we stage the loose `.kicad_mod` fixtures into a
// temporary `Fixtures.pretty/` first.

/// Stage the three vendored `.kicad_mod` fixtures into a fresh temp dir laid out
/// as `<tmp>/Fixtures.pretty/<name>.kicad_mod`, and return the temp dir (kept
/// alive by the caller) plus its path. The lib nickname is therefore `Fixtures`.
fn staged_footprint_dir() -> (tempfile::TempDir, std::path::PathBuf) {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../kicad-footprint/tests/fixtures/footprints");
    let tmp = tempfile::tempdir().unwrap();
    let pretty = tmp.path().join("Fixtures.pretty");
    std::fs::create_dir_all(&pretty).unwrap();
    for name in [
        "R_0603_1608Metric.kicad_mod",
        "SOT-23.kicad_mod",
        "PinHeader_1x02_P2.54mm_Vertical.kicad_mod",
    ] {
        std::fs::copy(src.join(name), pretty.join(name)).unwrap();
    }
    // Deliberately small connector package used to prove a large schematic
    // connector cannot silently inherit a package with only four contacts.
    std::fs::write(
        pretty.join("PinHeader_1x04_Test.kicad_mod"),
        r#"(footprint "PinHeader_1x04_Test"
  (version 20240108)
  (generator "gordian-test")
  (layer "F.Cu")
  (fp_rect (start -1 -1) (end 1 9) (stroke (width 0.05) (type default)) (fill none) (layer "F.CrtYd"))
  (pad "1" thru_hole rect (at 0 0) (size 1.7 1.7) (drill 1) (layers "*.Cu" "*.Mask"))
  (pad "2" thru_hole circle (at 0 2.54) (size 1.7 1.7) (drill 1) (layers "*.Cu" "*.Mask"))
  (pad "3" thru_hole circle (at 0 5.08) (size 1.7 1.7) (drill 1) (layers "*.Cu" "*.Mask"))
  (pad "4" thru_hole circle (at 0 7.62) (size 1.7 1.7) (drill 1) (layers "*.Cu" "*.Mask"))
  (pad "" np_thru_hole circle (at 0 10) (size 1 1) (drill 1) (layers "*.Cu" "*.Mask"))
)"#,
    )
    .unwrap();
    let path = tmp.path().to_path_buf();
    (tmp, path)
}

/// A ctx whose footprint index is the staged vendored fixtures. The returned
/// TempDir guard must outlive the ctx (it holds the staged `.pretty` dir).
fn fixture_ctx() -> (AgentRuntime, tempfile::TempDir) {
    let (guard, dir) = staged_footprint_dir();
    let ctx = AgentRuntime::with_footprint_dir_for_test(dir).expect("fixture ctx");
    (ctx, guard)
}

#[test]
fn validate_design_uses_the_stored_draft_without_resending_yaml() {
    let (ctx, _guard) = fixture_ctx();
    let yaml = "version: 1\nblocks: {main: {components: {R1: {part: R, between: [A, GND]}, R2: {part: R, between: [A, GND]}}}}";
    let created = run_tool("create_design", serde_json::json!({ "yaml": yaml }), &ctx).unwrap();
    assert_eq!(created["ok"], serde_json::json!(true), "{created}");
    assert_eq!(created["validated"], serde_json::json!(true), "{created}");
    assert_eq!(
        created["next_tool"],
        serde_json::json!("apply_design"),
        "{created}"
    );
    assert!(
        created["next"]
            .as_str()
            .is_some_and(|next| next.contains("do not revalidate")),
        "{created}"
    );

    let validated = run_tool("validate_design", serde_json::json!({}), &ctx).unwrap();

    assert_eq!(validated["ok"], serde_json::json!(true), "{validated}");
    assert_eq!(validated["errors"], serde_json::json!(0), "{validated}");
}

const CONNECTOR_60_WITH_FOUR_PAD_FOOTPRINT: &str = r#"
version: 1
blocks:
  main:
    components:
      J1:
        part: Connector:Conn_15X4
        footprint: Fixtures:PinHeader_1x04_Test
"#;

#[test]
fn create_design_rejects_60_pin_symbol_with_four_pad_footprint() {
    let (ctx, _guard) = fixture_ctx();

    let out = run_tool(
        "create_design",
        serde_json::json!({ "yaml": CONNECTOR_60_WITH_FOUR_PAD_FOOTPRINT }),
        &ctx,
    )
    .unwrap();

    assert_eq!(out["ok"], serde_json::json!(false), "{out}");
    assert_eq!(out["errors"], serde_json::json!(1), "{out}");
    assert_eq!(out["next_tool"], serde_json::json!("edit_design"), "{out}");
    let mismatch = &out["footprint_pin_mismatches"][0];
    assert_eq!(mismatch["reference"], serde_json::json!("J1"), "{out}");
    assert_eq!(
        mismatch["symbol_pins_absent_from_footprint"]
            .as_array()
            .map(Vec::len),
        Some(60),
        "{out}"
    );
    assert_eq!(
        mismatch["footprint_pads_absent_from_symbol"],
        serde_json::json!(["1", "2", "3", "4"]),
        "{out}"
    );
    assert!(
        out["next"]
            .as_str()
            .is_some_and(|next| next.contains("pad numbers match")),
        "{out}"
    );
}

#[test]
fn create_design_rejects_unpolarized_symbol_with_electrolytic_footprint() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let yaml = r#"
version: 1
blocks:
  main:
    components:
      C1:
        part: Device:C
        footprint: Capacitor_SMD:CP_Elec_8x10.5
        pins: {1: VBUS, 2: GND}
"#;

    let out = run_tool("create_design", serde_json::json!({ "yaml": yaml }), &ctx).unwrap();

    assert_eq!(out["ok"], serde_json::json!(false), "{out}");
    assert_eq!(out["errors"], serde_json::json!(1), "{out}");
    let mismatch = &out["footprint_pin_mismatches"][0];
    assert_eq!(mismatch["reference"], serde_json::json!("C1"), "{out}");
    assert!(
        mismatch["polarity_mismatch"]
            .as_str()
            .is_some_and(|reason| reason.contains("Device:C_Polarized")),
        "{out}"
    );
    assert!(
        mismatch.get("symbol_pins_absent_from_footprint").is_none(),
        "matching pad numbers should not be reported as the cause: {out}"
    );
    assert!(
        out["diagnostics"]
            .as_array()
            .is_some_and(|diagnostics| diagnostics.iter().any(|diagnostic| diagnostic
                .as_str()
                .is_some_and(|diagnostic| diagnostic.contains("positive pad")))),
        "{out}"
    );
}

#[test]
fn create_design_accepts_polarized_symbol_alias_with_electrolytic_footprint() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let yaml = r#"
version: 1
blocks:
  main:
    components:
      C1:
        part: Device:C_Polarized_Small
        footprint: Capacitor_SMD:CP_Elec_8x10.5
        pins: {1: VBUS, 2: GND}
"#;

    let out = run_tool("create_design", serde_json::json!({ "yaml": yaml }), &ctx).unwrap();

    assert_eq!(out["ok"], serde_json::json!(true), "{out}");
    assert!(out.get("footprint_pin_mismatches").is_none(), "{out}");
}

#[test]
fn edit_and_apply_preview_reject_incompatible_footprint_before_compose() {
    let (ctx, _guard) = fixture_ctx();
    let valid = r#"
version: 1
blocks:
  main:
    components:
      R1:
        part: Device:R
        footprint: Fixtures:R_0603_1608Metric
        pins: {1: SIG, 2: GND}
"#;
    let created = run_tool("create_design", serde_json::json!({ "yaml": valid }), &ctx).unwrap();
    assert_eq!(created["ok"], serde_json::json!(true), "{created}");

    let edited = run_tool(
        "edit_design",
        serde_json::json!({ "yaml": CONNECTOR_60_WITH_FOUR_PAD_FOOTPRINT }),
        &ctx,
    )
    .unwrap();
    assert_eq!(edited["ok"], serde_json::json!(false), "{edited}");
    assert_eq!(
        edited["mode"],
        serde_json::json!("full_replace"),
        "{edited}"
    );
    assert_eq!(
        edited["footprint_pin_mismatches"][0]["reference"],
        serde_json::json!("J1"),
        "{edited}"
    );

    let preview = run_tool("apply_design", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(preview["ok"], serde_json::json!(false), "{preview}");
    assert!(preview.get("would_write").is_none(), "{preview}");
    assert!(
        !ctx.sch_path().exists(),
        "preview must not compose/write a schematic"
    );
}

#[test]
fn validate_design_without_yaml_or_draft_returns_recovery_guidance() {
    let (ctx, _guard) = fixture_ctx();

    let out = run_tool("validate_design", serde_json::json!({}), &ctx).unwrap();

    assert!(
        out["error"]
            .as_str()
            .is_some_and(|error| error.contains("no draft exists")),
        "{out}"
    );
}

#[test]
fn create_design_does_not_replace_an_unreadable_draft() {
    let (ctx, _guard) = fixture_ctx();
    let invalid = [0xff, 0xfe];
    std::fs::write(ctx.workspace().draft_path(), invalid).unwrap();

    let err = run_tool(
        "create_design",
        serde_json::json!({ "yaml": TINY_YAML }),
        &ctx,
    )
    .unwrap_err();

    assert!(
        err.to_string().contains("UTF-8"),
        "unexpected error: {err:#}"
    );
    assert_eq!(
        std::fs::read(ctx.workspace().draft_path()).unwrap(),
        invalid
    );
}

#[test]
fn apply_design_does_not_write_when_the_draft_is_unreadable() {
    let (ctx, _guard) = fixture_ctx();
    let invalid = [0xff, 0xfe];
    std::fs::write(ctx.workspace().draft_path(), invalid).unwrap();

    let err = run_tool(
        "apply_design",
        serde_json::json!({ "__commit": true }),
        &ctx,
    )
    .expect_err("an unreadable authoritative draft must block the write");

    assert!(
        err.to_string().contains("UTF-8"),
        "unexpected error: {err:#}"
    );
    assert!(!ctx.sch_path().exists(), "no schematic may be written");
    assert_eq!(
        std::fs::read(ctx.workspace().draft_path()).unwrap(),
        invalid
    );
}

#[test]
fn search_footprints_finds_vendored_fixture() {
    let (ctx, _guard) = fixture_ctx();
    let out = run_tool(
        "search_footprints",
        serde_json::json!({ "query": "R_0603" }),
        &ctx,
    )
    .unwrap();
    let hits = out["hits"].as_array().expect("hits array");
    assert!(
        hits.iter()
            .any(|h| h["lib_id"] == "Fixtures:R_0603_1608Metric"),
        "expected the R_0603 fixture in hits, got: {out}"
    );
    // The R_0603 footprint has 2 pads.
    let r0603 = hits
        .iter()
        .find(|h| h["lib_id"] == "Fixtures:R_0603_1608Metric")
        .unwrap();
    assert_eq!(r0603["pad_count"], serde_json::json!(2), "got: {out}");
}

#[test]
fn search_footprints_batches_labeled_queries() {
    let (ctx, _guard) = fixture_ctx();
    let out = run_tool(
        "search_footprints",
        serde_json::json!({"queries": [
            {"query": "R_0603", "limit": 2},
            {"query": "SOT23", "limit": 2}
        ]}),
        &ctx,
    )
    .unwrap();
    let results = out["results"].as_array().expect("batched results");
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["query"], "R_0603");
    assert!(!results[0]["hits"].as_array().unwrap().is_empty());
}

#[test]
fn search_footprints_guides_rp2040_to_qfn_not_bga() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let out = run_tool(
        "search_footprints",
        serde_json::json!({ "query": "RP2040 BGA", "limit": 8 }),
        &ctx,
    )
    .unwrap();
    assert!(
        out["note"]
            .as_str()
            .is_some_and(|note| note.contains("not substitute a BGA")),
        "expected RP2040 package guidance, got: {out}"
    );
    let hits = out["hits"].as_array().expect("hits array");
    assert!(
        hits.iter()
            .any(|h| h["lib_id"].as_str().is_some_and(|id| id.contains("QFN-56"))),
        "expected QFN-56 hits, got: {out}"
    );
}

#[test]
fn assign_footprints_edits_the_working_draft() {
    let (ctx, _guard) = fixture_ctx();
    let yaml = "\
version: 1
blocks:
  main:
    components:
      R1:
        part: R
        between: [NET_A, NET_B]
      R2:
        part: R
        between: [NET_A, NET_B]
";
    let created = run_tool(
        "create_design",
        serde_json::json!({ "yaml": yaml, "overwrite": true }),
        &ctx,
    )
    .unwrap();
    assert_eq!(
        created["draft_written"],
        serde_json::json!(true),
        "create draft: {created}"
    );

    let assigned = run_tool(
        "assign_footprints",
        serde_json::json!({
            "assignments": [
                { "reference": "R1", "footprint": "Fixtures:R_0603_1608Metric" }
            ],
        }),
        &ctx,
    )
    .unwrap();
    assert_eq!(
        assigned["ok"],
        serde_json::json!(true),
        "assign footprint: {assigned}"
    );
    assert_eq!(assigned["count"], serde_json::json!(1));
    let items = assigned["assigned"].as_array().expect("assigned array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["reference"], serde_json::json!("R1"));
    assert_eq!(
        items[0]["footprint"],
        serde_json::json!("Fixtures:R_0603_1608Metric")
    );
    assert_eq!(items[0]["edit"], serde_json::json!("inserted"));
    assert_eq!(
        assigned["next"],
        serde_json::json!("apply_design(), then regenerate_board")
    );
    let draft = ctx
        .workspace()
        .read_draft()
        .unwrap()
        .expect("draft after assignment");
    assert!(
        draft.contains("footprint: \"Fixtures:R_0603_1608Metric\""),
        "draft was not edited:\n{draft}"
    );
}

#[test]
fn assign_footprints_accepts_batch_assignments() {
    let (ctx, _guard) = fixture_ctx();
    let yaml = "\
version: 1
blocks:
  main:
    components:
      R1:
        part: R
        between: [NET_A, NET_B]
      R2:
        part: R
        between: [NET_A, NET_B]
";
    run_tool(
        "create_design",
        serde_json::json!({ "yaml": yaml, "overwrite": true }),
        &ctx,
    )
    .unwrap();

    let assigned = run_tool(
        "assign_footprints",
        serde_json::json!({
            "assignments": [
                { "reference": "R1", "footprint": "Fixtures:R_0603_1608Metric" },
                { "reference": "R2", "footprint": "Fixtures:R_0603_1608Metric" }
            ],
        }),
        &ctx,
    )
    .unwrap();
    assert_eq!(assigned["ok"], serde_json::json!(true), "{assigned}");
    assert_eq!(assigned["count"], serde_json::json!(2));
    assert_eq!(
        assigned["next"],
        serde_json::json!("apply_design(), then regenerate_board")
    );
    let items = assigned["assigned"].as_array().expect("assigned array");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["reference"], serde_json::json!("R1"));
    assert_eq!(items[1]["reference"], serde_json::json!("R2"));

    let draft = ctx
        .workspace()
        .read_draft()
        .unwrap()
        .expect("draft after batch");
    assert_eq!(
        draft
            .matches("footprint: \"Fixtures:R_0603_1608Metric\"")
            .count(),
        2,
        "draft was not batch-edited:\n{draft}"
    );
}

#[test]
fn regenerate_board_rejects_unapplied_draft_footprints() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };

    seed_draft(&ctx, TINY_YAML);
    let written = run_tool(
        "apply_design",
        serde_json::json!({ "__commit": true }),
        &ctx,
    )
    .unwrap();
    assert_eq!(written["written"], serde_json::json!(true), "{written}");

    let draft = run_tool(
        "read_schematic",
        serde_json::json!({ "source": "draft" }),
        &ctx,
    )
    .unwrap();
    let draft = draft.as_str().expect("plain text read_schematic result");
    assert!(
        draft.contains("R1"),
        "draft seeded from committed schematic: {draft}"
    );
    let assigned = run_tool(
        "assign_footprints",
        serde_json::json!({
            "assignments": [
                { "reference": "R1", "footprint": "Resistor_SMD:R_0603_1608Metric" }
            ],
        }),
        &ctx,
    )
    .unwrap();
    assert_eq!(assigned["ok"], serde_json::json!(true), "{assigned}");

    let out = run_tool("regenerate_board", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(out["ok"], serde_json::json!(false), "{out}");
    assert!(
        out["unapplied_draft_footprints"]
            .as_array()
            .is_some_and(|changes| changes.iter().any(|c| c["reference"] == "R1")),
        "regenerate_board should ask to apply draft footprint changes first: {out}"
    );
    assert!(
        out["next"]
            .as_str()
            .is_some_and(|n| n.contains("apply_design()")),
        "derive note should name the required commit: {out}"
    );
    assert_eq!(out["next_tool"], serde_json::json!("apply_design"));
}

#[test]
#[ignore = "live KiCAD IPC: regenerate_board opens the project board through the session manager"]
fn regenerate_board_seeds_board_from_schematic_then_assign_footprints() {
    // Needs a real KiCAD env (lift runs kicad-cli + resolves real footprints).
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP regenerate_board: no KiCAD env");
        return;
    };
    // Stage the RC-pair fixture as the project's schematic.
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../kicad-cli/tests/fixtures/rc_pair.kicad_sch");
    std::fs::copy(&fixture, ctx.sch_path()).unwrap();

    let bounds = serde_json::json!({ "min_x": 0, "max_x": 20, "min_y": 0, "max_y": 12 });

    // regenerate_board seeds the board directly from the schematic (no DSL/YAML).
    let seed = run_tool(
        "regenerate_board",
        serde_json::json!({ "bounds": bounds }),
        &ctx,
    )
    .unwrap();
    if seed.get("error").is_some() {
        eprintln!("SKIP regenerate_board: lift failed (kicad-cli unavailable?): {seed}");
        return;
    }
    assert_eq!(seed["ok"], serde_json::json!(true), "board seeded: {seed}");
    assert_eq!(
        seed["part_count"],
        serde_json::json!(2),
        "R1 + C1 seeded: {seed}"
    );

    // Fill any footprint the schematic symbol didn't carry, via one batch
    // assignment (the interactive replacement for the old DSL footprint field).
    let assignments: Vec<_> = seed["missing_footprints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            let reference = r.as_str().unwrap();
            let footprint = if reference.starts_with('R') {
                "Resistor_SMD:R_0603_1608Metric"
            } else {
                "Capacitor_SMD:C_0603_1608Metric"
            };
            serde_json::json!({ "reference": reference, "footprint": footprint })
        })
        .collect();
    if !assignments.is_empty() {
        let a = run_tool(
            "assign_footprints",
            serde_json::json!({ "assignments": assignments }),
            &ctx,
        )
        .unwrap();
        assert_eq!(a["ok"], serde_json::json!(true), "assign footprints: {a}");
    }

    // The seed board carries R1 + C1, derived (not retyped).
    let board = run_tool("get_board", serde_json::json!({}), &ctx).unwrap();
    let s = board.to_string();
    assert!(
        s.contains("R1") && s.contains("C1"),
        "board has R1 + C1: {board}"
    );
}

#[test]
fn get_footprint_info_returns_pads_courtyard_bbox() {
    let (ctx, _guard) = fixture_ctx();
    let out = run_tool(
        "get_footprint_info",
        serde_json::json!({ "lib_id": "Fixtures:SOT-23" }),
        &ctx,
    )
    .unwrap();
    // Lean shape: pad NUMBER list + a compact geometry summary (no per-pad coordinate dump).
    let nums = out["pad_numbers"].as_array().expect("pad_numbers array");
    assert_eq!(nums.len(), 3, "SOT-23 has 3 pads: {out}");
    assert!(
        nums.iter().all(|n| n.is_string()),
        "pad numbers are strings: {out}"
    );
    assert_eq!(out["pad_count"], 3);
    assert!(
        out["min_pitch_mm"].as_f64().is_some_and(|p| p > 0.0),
        "min_pitch present: {out}"
    );
    assert!(
        out.get("pad_min_dim_mm").is_some(),
        "pad dims present: {out}"
    );
    assert!(
        out.get("technologies").is_some(),
        "technologies present: {out}"
    );
    assert!(out.get("courtyard").is_some(), "courtyard present: {out}");
    assert!(out["courtyard"].get("width").is_some());
    assert!(out.get("bbox").is_some(), "bbox present: {out}");
    // Per-pad coordinate table is intentionally summarized away (model places nothing by coord).
    assert!(
        out.get("pads").is_none(),
        "per-pad table should be gone: {out}"
    );
}

#[test]
fn get_footprint_info_suggests_for_unknown_lib_id() {
    let (ctx, _guard) = fixture_ctx();
    let out = run_tool(
        "get_footprint_info",
        serde_json::json!({ "lib_id": "Fixtures:SOT-32" }),
        &ctx,
    )
    .unwrap();
    assert!(out.get("error").is_some(), "expected an error: {out}");
    let suggestions = out["suggestions"].as_array().expect("suggestions");
    assert!(
        suggestions.iter().any(|s| s == "Fixtures:SOT-23"),
        "expected SOT-23 suggested for the SOT-32 typo: {out}"
    );
}

// ── PCB tools: place / route / constraints / triage ──────────────────────────
//
// These use the same vendored-fixture footprint index as the Task 1 tests (no
// KiCAD install needed). The standard board is a small 3-part divider-ish board
// whose nets each have ≥2 pins, so it places legal and routes with zero failures.

/// Create the standard small board on a fresh
/// fixture ctx. Returns the ctx and its tempdir guard.
fn placed_board_ctx() -> (AgentRuntime, tempfile::TempDir) {
    let (ctx, guard) = fixture_ctx();
    write_fixture_board(&ctx, 30.0, 20.0);
    (ctx, guard)
}

fn write_fixture_board(ctx: &AgentRuntime, w: f64, h: f64) {
    let board = TWO_RES_PCB
        .replace(
            "Resistor_SMD:R_0805_2012Metric",
            "Fixtures:R_0603_1608Metric",
        )
        .replace("(end 30 20)", &format!("(end {w} {h})"));
    std::fs::write(ctx.pcb_path(), board).unwrap();
}

#[test]
fn update_board_outline_replaces_existing_rect_without_regeneration() {
    let (ctx, _guard) = fixture_ctx();
    write_fixture_board(&ctx, 30.0, 20.0);

    let out = run_tool(
        "update_board_outline",
        serde_json::json!({
            "bounds": { "min_x": 2.0, "max_x": 18.0, "min_y": 3.0, "max_y": 15.0 }
        }),
        &ctx,
    )
    .unwrap();
    assert_eq!(out["ok"], serde_json::json!(true), "got: {out}");
    assert_eq!(out["changed"], serde_json::json!(true), "got: {out}");
    let board = std::fs::read_to_string(ctx.pcb_path()).unwrap();
    assert!(board.contains("(start 2 3)"), "{board}");
    assert!(board.contains("(end 18 15)"), "{board}");
    assert!(!board.contains("(end 30 20)"), "{board}");
    assert!(board.contains("(footprint \"Fixtures:R_0603_1608Metric\""));

    let unchanged = run_tool(
        "update_board_outline",
        serde_json::json!({
            "bounds": { "min_x": 2.0, "max_x": 18.0, "min_y": 3.0, "max_y": 15.0 }
        }),
        &ctx,
    )
    .unwrap();
    assert_eq!(
        unchanged["changed"],
        serde_json::json!(false),
        "got: {unchanged}"
    );
}

#[test]
fn update_board_outline_accepts_polygon_points() {
    let (ctx, _guard) = fixture_ctx();
    write_fixture_board(&ctx, 30.0, 20.0);

    let out = run_tool(
        "update_board_outline",
        serde_json::json!({
            "outline": [[0.0, 0.0], [12.0, 0.0], [6.0, 8.0]]
        }),
        &ctx,
    )
    .unwrap();
    assert_eq!(out["ok"], serde_json::json!(true), "got: {out}");
    assert_eq!(out["outline_points"], serde_json::json!(3), "got: {out}");
    let board = std::fs::read_to_string(ctx.pcb_path()).unwrap();
    assert_eq!(board.matches("(layer \"Edge.Cuts\")").count(), 3);
    assert!(board.contains("(gr_line"));
}

fn skip_unstable_footprint_update(ctx: &AgentRuntime) -> bool {
    if std::env::var_os("GORDIAN_RUN_UNSTABLE_KICAD_IPC").is_some() {
        return false;
    }
    let mut nums = ctx
        .env()
        .cli_version
        .split('.')
        .take(3)
        .map(|part| part.parse::<u32>().unwrap_or(0));
    let major = nums.next().unwrap_or(0);
    let minor = nums.next().unwrap_or(0);
    let patch = nums.next().unwrap_or(0);
    if major == 9 && minor == 0 && patch <= 2 {
        eprintln!(
            "SKIP: KiCAD {} has unstable IPC footprint UpdateItems",
            ctx.env().cli_version
        );
        return true;
    }
    false
}

#[test]
#[ignore = "live KiCAD IPC: opens the project board through the global session manager"]
fn place_board_failure_suggests_a_larger_bounds() {
    // Three parts crammed into a 3x3 mm board cannot fit; the failure must hand the
    // agent a CONCRETE, larger min-bounds suggestion so it can retry deterministically.
    let (ctx, _g) = fixture_ctx();
    write_fixture_board(&ctx, 3.0, 3.0);
    let out = run_tool("place_board", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(
        out["legal"],
        serde_json::json!(false),
        "should not fit in 3x3: {out}"
    );
    assert_eq!(out["placement_applied"], serde_json::json!(false));
    assert!(
        out["error"]
            .as_str()
            .is_some_and(|message| message.contains("no positions were written")),
        "failed placement must be an explicit tool error: {out}"
    );
    let s = &out["suggested_min_bounds_mm"];
    let (w, h) = (s["w"].as_f64().unwrap(), s["h"].as_f64().unwrap());
    assert!(
        w > 3.0 && h > 3.0,
        "suggestion must exceed the failing bounds: {out}"
    );
    assert!(
        out["parts_courtyard_area_mm2"].as_f64().unwrap() > 0.0,
        "must report the parts' courtyard area: {out}"
    );
}

#[test]
#[ignore = "live KiCAD IPC: exercises place_board/route_board against the active session"]
fn full_flow_create_place_route_is_clean() {
    let (ctx, _g) = placed_board_ctx();
    if skip_unstable_footprint_update(&ctx) {
        return;
    }
    struct CloseKicad<'a>(&'a AgentRuntime);
    impl Drop for CloseKicad<'_> {
        fn drop(&mut self) {
            self.0.close_kicad_session();
        }
    }
    let _close_kicad = CloseKicad(&ctx);

    // route_board before any placement -> recoverable error.
    let out = run_tool("route_board", serde_json::json!({}), &ctx).unwrap();
    assert!(
        out["error"]
            .as_str()
            .is_some_and(|e| e.contains("place_board")),
        "route before place must tell the model to place first: {out}"
    );

    // place_board -> legal, positions for every part.
    let out = run_tool("place_board", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(out["legal"], serde_json::json!(true), "place_board: {out}");
    let positions = out["positions"].as_array().expect("positions");
    assert_eq!(positions.len(), 3, "one position per part: {out}");
    for p in positions {
        assert!(
            p.get("reference").is_some()
                && p.get("x").is_some()
                && p.get("y").is_some()
                && p.get("rotation").is_some(),
            "position shape: {p}"
        );
    }
    assert!(out["hpwl"].as_f64().unwrap() >= 0.0);

    // get_board now reports placed=true.
    let out = run_tool("get_board", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(out["summary"]["placed"], serde_json::json!(true), "{out}");

    // route_board -> zero failed, lint clean (no engine_bug), real metrics.
    let out = run_tool("route_board", serde_json::json!({}), &ctx).unwrap();
    assert!(
        out["failed"].as_array().unwrap().is_empty(),
        "the small board must route with zero failed nets: {out}"
    );
    assert!(
        out.get("engine_bug").is_none(),
        "a clean route must NOT flag an engine bug: {out}"
    );
    // lint_summary is an object with no counts (all zero).
    assert!(
        out["lint_summary"].as_object().unwrap().is_empty(),
        "lint_summary must be empty (zero violations): {out}"
    );
    assert!(
        out["metrics"]["wirelength"].as_f64().unwrap() > 0.0,
        "{out}"
    );
    assert!(out["metrics"]["traces"].as_u64().unwrap() > 0, "{out}");
    // Any portfolio engine may win (route_auto picks the fewer-failed result); just
    // assert the provenance tag is one of the known honest values.
    assert!(
        matches!(
            out["router"].as_str(),
            Some("direct") | Some("naive") | Some("detailed")
        ),
        "router must be direct, naive, or detailed: {out}"
    );

    // The solution persisted; get_board reports routed=true.
    let out = run_tool("get_board", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(out["summary"]["routed"], serde_json::json!(true), "{out}");
}

#[test]
#[ignore = "live KiCAD IPC: reproduces reopen-after-seed-snapshot placement lifecycle"]
fn place_board_after_seed_snapshot_reopen_is_ready() {
    let (ctx, _g) = placed_board_ctx();
    if skip_unstable_footprint_update(&ctx) {
        return;
    }
    struct CloseKicad<'a>(&'a AgentRuntime);
    impl Drop for CloseKicad<'_> {
        fn drop(&mut self) {
            self.0.close_kicad_session();
        }
    }
    let _close_kicad = CloseKicad(&ctx);

    let out = run_tool("route_board", serde_json::json!({}), &ctx).unwrap();
    assert!(
        out["error"]
            .as_str()
            .is_some_and(|e| e.contains("place_board")),
        "route before place must tell the model to place first: {out}"
    );

    let out = run_tool("place_board", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(out["legal"], serde_json::json!(true), "place_board: {out}");
}

#[test]
#[ignore = "live KiCAD IPC: verifies an interactive footprint move survives session restart"]
fn move_parts_persists_across_session_reopen() {
    let (ctx, _guard) = placed_board_ctx();
    if skip_unstable_footprint_update(&ctx) {
        return;
    }

    let moved = run_tool(
        "move_parts",
        serde_json::json!({
            "moves": [{"reference": "R1", "to": [12.0, 13.0], "rotation": 90.0}]
        }),
        &ctx,
    )
    .unwrap();
    assert!(moved.get("error").is_none(), "move failed: {moved}");

    ctx.close_kicad_session();
    let reopened = run_tool("get_board", serde_json::json!({}), &ctx).unwrap();
    let r1 = reopened["board"]["parts"]
        .as_array()
        .and_then(|parts| parts.iter().find(|part| part["reference"] == "R1"))
        .expect("R1 present after reopening the saved board");
    assert_eq!(r1["x"], serde_json::json!(12.0), "reopened: {reopened}");
    assert_eq!(r1["y"], serde_json::json!(13.0), "reopened: {reopened}");
    assert_eq!(
        r1["rotation"],
        serde_json::json!(90.0),
        "reopened: {reopened}"
    );
    ctx.close_kicad_session();
}

#[test]
#[ignore = "live KiCAD IPC: verifies net-class edits survive session restart"]
fn set_net_width_persists_across_session_reopen() {
    let (ctx, _guard) = placed_board_ctx();
    let mut version = ctx.env().cli_version.split('.').map(|part| {
        part.chars()
            .take_while(|ch| ch.is_ascii_digit())
            .collect::<String>()
            .parse::<u32>()
            .unwrap_or(0)
    });
    let (major, minor, patch) = (
        version.next().unwrap_or(0),
        version.next().unwrap_or(0),
        version.next().unwrap_or(0),
    );
    if major < 9 || (major == 9 && minor == 0 && patch < 3) {
        eprintln!(
            "SKIP: KiCAD {} does not reliably support SetNetClasses",
            ctx.env().cli_version
        );
        return;
    }
    let updated = run_tool(
        "set_net_width",
        serde_json::json!({
            "name": "Power",
            "width": 0.75,
            "clearance": 0.25,
            "nets": ["GND"]
        }),
        &ctx,
    )
    .unwrap();
    assert_eq!(updated["ok"], serde_json::json!(true), "update: {updated}");

    ctx.close_kicad_session();
    let reopened = run_tool("get_board", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(
        reopened["board"]["rules"]["net_widths"]["GND"],
        serde_json::json!(0.75),
        "reopened: {reopened}"
    );
    ctx.close_kicad_session();
}

#[test]
#[ignore = "live KiCAD IPC: requires place/route/check against an active board session"]
fn check_board_after_place_and_route_reports_drc() {
    let (ctx, _g) = placed_board_ctx();
    if skip_unstable_footprint_update(&ctx) {
        return;
    }

    run_tool("place_board", serde_json::json!({}), &ctx).unwrap();
    let routed = run_tool("route_board", serde_json::json!({}), &ctx).unwrap();
    assert!(
        routed["failed"].as_array().unwrap().is_empty(),
        "route: {routed}"
    );

    let out = run_tool("check_board", serde_json::json!({}), &ctx).unwrap();
    assert!(out["violations"].is_number(), "DRC status present: {out}");
}

/// KiCAD-gated check: the agent-tools flow
/// derive/build board → place_board → route_board → check_board on a small vendored
/// circuit must yield a board KiCAD's DRC finds zero copper-violation /
/// unconnected.
/// Skips visibly when no KiCAD >= 8 is installed.
#[test]
#[ignore = "live KiCAD IPC + KiCAD DRC"]
fn check_board_e2e_kicad_drc_clean() {
    // Build the fixture-footprint ctx; it carries a real KiCAD env when one is
    // installed (with_footprint_dir_for_test falls back to KicadEnv::detect).
    let (ctx, _g) = placed_board_ctx();
    let major: u32 = ctx
        .env()
        .cli_version
        .split('.')
        .next()
        .and_then(|m| m.parse().ok())
        .unwrap_or(0);
    if major < 8 {
        eprintln!("SKIP: no KiCAD >= 8 for check_board DRC e2e");
        return;
    }
    if skip_unstable_footprint_update(&ctx) {
        return;
    }

    run_tool("place_board", serde_json::json!({}), &ctx).unwrap();
    let routed = run_tool("route_board", serde_json::json!({}), &ctx).unwrap();
    assert!(
        routed["failed"].as_array().unwrap().is_empty(),
        "route: {routed}"
    );

    let out = run_tool("check_board", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(out["ok"], serde_json::json!(true), "check_board: {out}");
    assert_eq!(
        out["drc_clean"],
        serde_json::json!(true),
        "check_board: {out}"
    );
    assert_eq!(
        out["blocking_findings"],
        serde_json::json!(0),
        "check_board: {out}"
    );
    assert!(out["reported_findings"].is_number(), "check_board: {out}");
    assert!(
        out["next"]
            .as_str()
            .is_some_and(|next| next.contains("Do not regenerate")),
        "check_board: {out}"
    );
    // Strict: zero copper-layer violations of ANY severity (the detailed router's
    // spurious via_dangling vias are dropped at the stitch source, so this stays
    // clean — and now guards against that regression).
    assert_eq!(
        out["copper_violations"],
        serde_json::json!(0),
        "checked board must be copper-DRC-clean: {out}"
    );
    assert_eq!(
        out["unconnected_items"],
        serde_json::json!(0),
        "checked board must have zero unconnected items: {out}"
    );

    eprintln!(
        "check_board e2e OK (KiCAD {}): live board DRC copper-clean, 0 unconnected",
        ctx.env().cli_version
    );
}

// ── PCB tools (slice 5, Task 3): render_board ─────────────────────────────────

/// PNG magic bytes — the 8-byte header all valid PNG files start with.
const PNG_MAGIC: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

#[test]
fn render_board_before_create_is_recoverable_error() {
    let (ctx, _guard) = fixture_ctx();
    let out = run_tool("render_board", serde_json::json!({}), &ctx).unwrap();
    assert!(
        out["error"]
            .as_str()
            .is_some_and(|e| e.contains("no board")),
        "render_board before regenerate_board must be a recoverable error: {out}"
    );
}

#[test]
#[ignore = "live KiCAD IPC: render_board saves/imports the active board session"]
fn render_board_before_place_returns_ok_and_png_magic() {
    let (ctx, _guard) = fixture_ctx();
    // Create board but do NOT place.
    write_fixture_board(&ctx, 30.0, 20.0);

    // The current board preview should render even before placement.
    let out = run_tool("render_board", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(out["ok"], serde_json::json!(true), "render: {out}");
    let png_path = out["png_path"].as_str().expect("png_path present");
    let png_bytes = std::fs::read(png_path).expect("PNG file written");
    assert_eq!(&png_bytes[..8], PNG_MAGIC, "must be a valid PNG");
}

#[test]
#[ignore = "live KiCAD IPC: render_board saves/imports the active board session"]
fn render_board_after_place_returns_ok_and_png_magic() {
    let (ctx, _g) = placed_board_ctx();
    if skip_unstable_footprint_update(&ctx) {
        return;
    }

    // Place the board.
    let out = run_tool("place_board", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(out["legal"], serde_json::json!(true), "place: {out}");

    // Render the current board.
    let out = run_tool("render_board", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(
        out["ok"],
        serde_json::json!(true),
        "render_board after place: {out}"
    );

    let png_path = out["png_path"].as_str().expect("png_path present");
    assert!(
        png_path.contains(".gordian/renders/"),
        "path under renders/: {out}"
    );
    let png_bytes = std::fs::read(png_path).expect("PNG file written");
    assert_eq!(&png_bytes[..8], PNG_MAGIC, "must be a valid PNG");
    assert!(
        png_bytes.len() > 100,
        "PNG suspiciously small: {} bytes",
        png_bytes.len()
    );

    // IMAGE_PATH_KEY must be set to the same path (so the agent loop attaches it).
    assert_eq!(
        out[gordian_runtime::tool::IMAGE_PATH_KEY].as_str(),
        Some(png_path),
        "IMAGE_PATH_KEY must equal png_path"
    );
}

#[test]
#[ignore = "live KiCAD IPC: render_board saves/imports the active board session"]
fn render_board_after_route_returns_ok_and_png_magic() {
    let (ctx, _g) = placed_board_ctx();
    if skip_unstable_footprint_update(&ctx) {
        return;
    }

    // Full flow: place then route.
    run_tool("place_board", serde_json::json!({}), &ctx).unwrap();
    let route_out = run_tool("route_board", serde_json::json!({}), &ctx).unwrap();
    assert!(
        route_out["failed"].as_array().unwrap().is_empty(),
        "small board must route cleanly: {route_out}"
    );

    // Render the current board with routed copper visible.
    let out = run_tool("render_board", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(
        out["ok"],
        serde_json::json!(true),
        "render_board after route: {out}"
    );

    let png_path = out["png_path"].as_str().expect("png_path present");
    let png_bytes = std::fs::read(png_path).expect("PNG file written");
    assert_eq!(&png_bytes[..8], PNG_MAGIC, "must be a valid PNG");

    // IMAGE_PATH_KEY set.
    assert_eq!(
        out[gordian_runtime::tool::IMAGE_PATH_KEY].as_str(),
        Some(png_path),
        "IMAGE_PATH_KEY must equal png_path"
    );
}

#[test]
fn four_layer_plane_fanout_passes_kicad_drc() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    // Power-heavy mini board: enough VCC/GND pads for the plane assignment,
    // one signal net so routing still runs.
    let yaml = "version: 1\nblocks: {main: {components: {\
R1: {part: 'Device:R', footprint: 'Resistor_SMD:R_0603_1608Metric', pins: {1: VCC, 2: S}}, \
R2: {part: 'Device:R', footprint: 'Resistor_SMD:R_0603_1608Metric', pins: {1: S, 2: GND}}, \
C1: {part: 'Device:C', footprint: 'Capacitor_SMD:C_0603_1608Metric', pins: {1: VCC, 2: GND}}, \
C2: {part: 'Device:C', footprint: 'Capacitor_SMD:C_0603_1608Metric', pins: {1: VCC, 2: GND}}}}}";
    seed_draft(&ctx, yaml);
    let applied = run_tool(
        "apply_design",
        serde_json::json!({ "__commit": true }),
        &ctx,
    )
    .unwrap();
    assert_eq!(applied["ok"], serde_json::json!(true), "apply: {applied}");

    let out = run_tool(
        "regenerate_board",
        serde_json::json!({
            "bounds": {"min_x": 0.0, "min_y": 0.0, "max_x": 30.0, "max_y": 20.0},
            "rules": {"layer_count": 4},
        }),
        &ctx,
    )
    .unwrap();
    assert!(out["error"].is_null(), "{out}");
    let board_text = std::fs::read_to_string(ctx.pcb_path()).unwrap();
    assert!(
        board_text.contains("(layer \"In1.Cu\")") && board_text.contains("(layer \"In2.Cu\")"),
        "seed must carry plane zones on both inner layers"
    );

    run_tool("place_board", serde_json::json!({}), &ctx).unwrap();
    let routed = run_tool("route_board", serde_json::json!({}), &ctx).unwrap();
    assert!(
        routed["failed"].as_array().is_some_and(Vec::is_empty),
        "plane-fanout board must route cleanly: {routed}"
    );

    let checked = run_tool("check_board", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(
        checked["ok"],
        serde_json::json!(true),
        "kicad-cli DRC must accept via-to-plane power connectivity: {checked}"
    );
}
