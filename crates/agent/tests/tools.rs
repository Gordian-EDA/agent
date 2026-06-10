//! Deterministic tests for the six-tool registry (no LLM, no network).
//!
//! Every test is SKIP-graceful: if no KiCAD installation is detected,
//! [`ToolCtx::detect_for_test`] returns `None` and the test prints `SKIP` and
//! returns rather than failing. The tests that touch real symbol libraries and
//! `kicad-cli` therefore only assert on machines with KiCAD installed (the
//! project's test environment has KiCAD 10.0.3).

use agent::tools::{ToolCtx, Tools};

/// A tiny self-contained valid design: one resistor between two named nets.
const TINY_YAML: &str =
    "version: 1\nblocks: {main: {components: {R1: {part: Device:R, pins: {1: A, 2: GND}}}}}";

#[test]
fn search_symbols_tool_finds_stm32() {
    let Some(ctx) = ToolCtx::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let tools = Tools::new();
    let out = tools
        .run(
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
fn validate_design_tool_reports_errors_for_bad_part() {
    let Some(ctx) = ToolCtx::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let tools = Tools::new();
    let yaml = "version: 1\nblocks: {main: {components: {U1: {part: No:Such, pins: {}}}}}";
    let out = tools
        .run("validate_design", serde_json::json!({ "yaml": yaml }), &ctx)
        .unwrap();
    let s = out.to_string();
    assert!(
        s.contains("unknown-part") || s.contains("not found"),
        "expected an unknown-part diagnostic, got: {s}"
    );
    assert_eq!(out["ok"], serde_json::json!(false));
}

#[test]
fn get_symbol_info_tool_returns_full_pin_table_for_stm32() {
    let Some(ctx) = ToolCtx::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let tools = Tools::new();
    let out = tools
        .run(
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
fn get_symbol_info_tool_suggests_for_unknown_part() {
    let Some(ctx) = ToolCtx::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let tools = Tools::new();
    let out = tools
        .run(
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
    let Some(ctx) = ToolCtx::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let tools = Tools::new();
    assert!(!ctx.sch_path().exists(), "fixture starts with no schematic");

    let out = tools
        .run(
            "apply_design",
            serde_json::json!({ "yaml": TINY_YAML }),
            &ctx,
        )
        .unwrap();

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
fn apply_design_commit_writes_file_and_runs_erc() {
    let Some(ctx) = ToolCtx::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let tools = Tools::new();
    assert!(!ctx.sch_path().exists(), "fixture starts with no schematic");

    let out = tools
        .run(
            "apply_design",
            serde_json::json!({ "yaml": TINY_YAML, "commit": true }),
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
}

#[test]
fn get_design_tool_notes_absent_schematic() {
    let Some(ctx) = ToolCtx::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let tools = Tools::new();
    let out = tools
        .run("get_design", serde_json::json!({}), &ctx)
        .unwrap();
    // No schematic yet: empty YAML with a note.
    assert_eq!(out["yaml"], serde_json::json!(""));
    assert!(out.get("note").is_some(), "expected a note: {out}");
}

#[test]
fn defs_lists_all_eight_tools() {
    let tools = Tools::new();
    let names: Vec<String> = tools.defs().into_iter().map(|d| d.name).collect();
    for expected in [
        "search_symbols",
        "get_symbol_info",
        "get_design",
        "validate_design",
        "apply_design",
        "run_erc",
        "project_info",
        "read_schematic",
    ] {
        assert!(names.contains(&expected.to_string()), "missing {expected}");
    }
}

#[test]
fn project_info_reports_paths_and_state() {
    let Some(ctx) = ToolCtx::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let tools = Tools::new();
    let out = tools
        .run("project_info", serde_json::json!({}), &ctx)
        .unwrap();
    assert_eq!(
        out["sch_path"],
        serde_json::json!(ctx.sch_path().display().to_string())
    );
    assert_eq!(out["sch_exists"], serde_json::json!(false));
    assert_eq!(
        out["project_dir"],
        serde_json::json!(ctx.project_dir().display().to_string())
    );
    assert!(out["snapshots"].is_number(), "snapshot count: {out}");

    // After a commit the same tool reports the file as present.
    tools
        .run(
            "apply_design",
            serde_json::json!({ "yaml": TINY_YAML, "commit": true }),
            &ctx,
        )
        .unwrap();
    let out = tools
        .run("project_info", serde_json::json!({}), &ctx)
        .unwrap();
    assert_eq!(out["sch_exists"], serde_json::json!(true), "got: {out}");
}

#[test]
fn read_schematic_lifts_an_external_file_by_absolute_path() {
    let Some(ctx) = ToolCtx::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let tools = Tools::new();
    // Write a real schematic into the project, then read it back as if it were
    // an arbitrary external path.
    tools
        .run(
            "apply_design",
            serde_json::json!({ "yaml": TINY_YAML, "commit": true }),
            &ctx,
        )
        .unwrap();
    let abs = ctx.sch_path().display().to_string();
    let out = tools
        .run("read_schematic", serde_json::json!({ "path": abs }), &ctx)
        .unwrap();
    let yaml = out["yaml"].as_str().expect("lifted yaml");
    assert!(yaml.contains("R1"), "lifted yaml carries R1: {yaml}");
    assert!(
        out.get("note").is_some(),
        "reading the project's own schematic is noted: {out}"
    );
}

#[test]
fn read_schematic_resolves_relative_to_the_project_dir() {
    let Some(ctx) = ToolCtx::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let tools = Tools::new();
    tools
        .run(
            "apply_design",
            serde_json::json!({ "yaml": TINY_YAML, "commit": true }),
            &ctx,
        )
        .unwrap();
    let rel = ctx.sch_path().file_name().unwrap().to_string_lossy();
    let out = tools
        .run("read_schematic", serde_json::json!({ "path": rel }), &ctx)
        .unwrap();
    assert!(
        out["yaml"].as_str().is_some_and(|y| y.contains("R1")),
        "relative path resolves against the project dir: {out}"
    );
}

#[test]
fn read_schematic_errors_cleanly_for_missing_or_wrong_files() {
    let Some(ctx) = ToolCtx::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let tools = Tools::new();

    let out = tools
        .run(
            "read_schematic",
            serde_json::json!({ "path": "/no/such/file.kicad_sch" }),
            &ctx,
        )
        .unwrap();
    assert!(
        out["error"].as_str().is_some_and(|e| e.contains("no file")),
        "missing file is a structured error: {out}"
    );

    let not_sch = ctx.project_dir().join("readme.txt");
    std::fs::write(&not_sch, "hello").unwrap();
    let out = tools
        .run(
            "read_schematic",
            serde_json::json!({ "path": not_sch.display().to_string() }),
            &ctx,
        )
        .unwrap();
    assert!(
        out["error"]
            .as_str()
            .is_some_and(|e| e.contains(".kicad_sch")),
        "wrong extension is a structured error: {out}"
    );
}

#[test]
fn apply_design_commit_reports_the_written_path() {
    let Some(ctx) = ToolCtx::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let tools = Tools::new();
    let out = tools
        .run(
            "apply_design",
            serde_json::json!({ "yaml": TINY_YAML, "commit": true }),
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
    let Some(ctx) = ToolCtx::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let tools = Tools::new();
    assert!(
        tools
            .run("no_such_tool", serde_json::json!({}), &ctx)
            .is_err()
    );
}

#[test]
fn render_schematic_returns_png_and_image_path() {
    let Some(ctx) = agent::tools::ToolCtx::detect_for_test() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let tools = agent::tools::Tools::new();

    // No schematic yet -> structured error, no crash.
    let out = tools
        .run("render_schematic", serde_json::json!({}), &ctx)
        .unwrap();
    assert!(out.get("error").is_some());

    // Write a minimal schematic via apply_design, then render it.
    let yaml = "
version: 1
name: t
rails: [GND]
blocks:
  a:
    components:
      R1: {part: Device:R, value: 1k, between: [N1, GND]}
";
    let applied = tools
        .run(
            "apply_design",
            serde_json::json!({ "yaml": yaml, "commit": true }),
            &ctx,
        )
        .unwrap();
    assert_eq!(applied["written"], serde_json::json!(true));

    let out = tools
        .run("render_schematic", serde_json::json!({}), &ctx)
        .unwrap();
    let png_path = out["_image_path"].as_str().expect("image path");
    let bytes = std::fs::read(png_path).unwrap();
    assert_eq!(&bytes[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
    assert!(png_path.contains(".autopcb/renders/render-001.png"));
}
