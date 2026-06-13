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
fn defs_lists_all_tools() {
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
        "render_schematic",
        "create_design",
        "edit_design",
        "search_footprints",
        "get_footprint_info",
        "create_board",
        "get_board",
    ] {
        assert!(names.contains(&expected.to_string()), "missing {expected}");
    }
    assert_eq!(names.len(), 15, "expected exactly 15 tools, got {}: {:?}", names.len(), names);

    // Names are unique.
    let mut sorted = names.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), names.len(), "tool names must be unique: {names:?}");

    // Every def's input_schema is a JSON object with a "type":"object" root —
    // schema sanity for the model-facing definitions.
    for def in Tools::new().defs() {
        assert_eq!(
            def.input_schema["type"], serde_json::json!("object"),
            "{} schema root must be an object", def.name
        );
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
fn draft_lifecycle_create_edit_apply() {
    let Some(ctx) = agent::tools::ToolCtx::detect_for_test() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let tools = agent::tools::Tools::new();
    let yaml = "
version: 1
name: t
rails: [GND]
blocks:
  a:
    components:
      R1: {part: Device:R, value: 1k, between: [N1, GND]}
";

    // edit before create -> structured error.
    let out = tools.run("edit_design",
        serde_json::json!({"old_string": "x", "new_string": "y"}), &ctx).unwrap();
    assert!(out["error"].as_str().unwrap().contains("no draft"));

    // create seeds the draft and validates it.
    let out = tools.run("create_design", serde_json::json!({"yaml": yaml}), &ctx).unwrap();
    assert_eq!(out["ok"], serde_json::json!(true));
    // create again without overwrite -> error; with overwrite -> ok.
    let out = tools.run("create_design", serde_json::json!({"yaml": yaml}), &ctx).unwrap();
    assert!(out["error"].as_str().unwrap().contains("draft already exists"));

    // Anchored edit: ambiguity and uniqueness rules.
    let out = tools.run("edit_design",
        serde_json::json!({"old_string": "NOT-PRESENT", "new_string": "y"}), &ctx).unwrap();
    assert!(out["error"].as_str().unwrap().contains("not found"));
    let out = tools.run("edit_design",
        serde_json::json!({"old_string": "value: 1k", "new_string": "value: 4.7k"}), &ctx).unwrap();
    assert_eq!(out["ok"], serde_json::json!(true));
    assert_eq!(out["replacements"], serde_json::json!(1));

    // apply_design with NO yaml applies the draft.
    let out = tools.run("apply_design", serde_json::json!({"commit": true}), &ctx).unwrap();
    assert_eq!(out["written"], serde_json::json!(true));

    // get_design now prefers the draft and reports its source.
    let out = tools.run("get_design", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(out["source"], serde_json::json!("draft"));
    assert!(out["yaml"].as_str().unwrap().contains("4.7k"));
}

#[test]
fn get_design_seeds_draft_from_lift_and_flags_staleness() {
    let Some(ctx) = agent::tools::ToolCtx::detect_for_test() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let tools = agent::tools::Tools::new();
    let yaml = "
version: 1
name: t
rails: [GND]
blocks:
  a:
    components:
      R1: {part: Device:R, value: 1k, between: [N1, GND]}
";
    // Write a schematic with explicit yaml (no draft involved).
    tools.run("apply_design",
        serde_json::json!({"yaml": yaml, "commit": true}), &ctx).unwrap();

    // get_design lifts AND seeds the draft.
    let out = tools.run("get_design", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(out["source"], serde_json::json!("lifted"));
    let out2 = tools.run("get_design", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(out2["source"], serde_json::json!("draft"));
    assert_eq!(out2.get("stale"), None);

    // Out-of-band sch edit -> staleness surfaces.
    let sch = std::fs::read_to_string(ctx.sch_path()).unwrap();
    std::fs::write(ctx.sch_path(), format!("{sch}\n")).unwrap();
    let out3 = tools.run("get_design", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(out3["stale"], serde_json::json!(true));
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

#[test]
fn apply_design_surfaces_layout_warnings_and_relayout_blocks() {
    let Some(ctx) = agent::tools::ToolCtx::detect_for_test() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let tools = agent::tools::Tools::new();
    let yaml = "
version: 1
name: t
rails: [GND]
blocks:
  a:
    components:
      R1: {part: Device:R, value: 1k, between: [N1, GND]}
";
    // Commit so a prior schematic exists for the re-apply below.
    let out = tools.run("apply_design",
        serde_json::json!({ "yaml": yaml, "commit": true }), &ctx).unwrap();
    assert_eq!(out["written"], serde_json::json!(true));
    // Both new EmitOutput fields are present in the result JSON.
    assert!(out["layout_warnings"].is_array(), "layout_warnings present: {out}");
    assert!(out["relayout_blocks"].is_object(), "relayout_blocks present: {out}");
    // A clean single-R layout has no collisions.
    assert_eq!(out["layout_warnings"].as_array().unwrap().len(), 0);

    // Dry-run path also carries them.
    let dry = tools.run("apply_design", serde_json::json!({ "yaml": yaml }), &ctx).unwrap();
    assert!(dry["layout_warnings"].is_array(), "dry-run layout_warnings: {dry}");
    assert!(dry["relayout_blocks"].is_object(), "dry-run relayout_blocks: {dry}");
}

#[test]
fn apply_design_relayout_argument_branches() {
    let Some(ctx) = agent::tools::ToolCtx::detect_for_test() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let tools = agent::tools::Tools::new();
    let yaml = "
version: 1
name: t
rails: [GND]
blocks:
  a:
    components:
      R1: {part: Device:R, value: 1k, between: [N1, GND]}
";
    // relayout="all" is accepted (dry-run, no commit needed).
    let out = tools.run("apply_design",
        serde_json::json!({ "yaml": yaml, "relayout": "all" }), &ctx).unwrap();
    assert!(out.get("error").is_none(), "relayout=all should be accepted: {out}");
    assert!(out["relayout_blocks"].is_object());

    // relayout as a block list is accepted.
    let out = tools.run("apply_design",
        serde_json::json!({ "yaml": yaml, "relayout": ["a"] }), &ctx).unwrap();
    assert!(out.get("error").is_none(), "relayout=[a] should be accepted: {out}");

    // An unrecognized relayout string is rejected with a structured error.
    let out = tools.run("apply_design",
        serde_json::json!({ "yaml": yaml, "relayout": "nonsense" }), &ctx).unwrap();
    let err = out["error"].as_str().expect("error string for bad relayout");
    assert!(err.contains("relayout"), "error should mention relayout: {err}");
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
        .join("../kicad-bridge/tests/fixtures/footprints");
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
    let path = tmp.path().to_path_buf();
    (tmp, path)
}

/// A ctx whose footprint index is the staged vendored fixtures. The returned
/// TempDir guard must outlive the ctx (it holds the staged `.pretty` dir).
fn fixture_ctx() -> (ToolCtx, tempfile::TempDir) {
    let (guard, dir) = staged_footprint_dir();
    let ctx = ToolCtx::with_footprint_dir_for_test(dir).expect("fixture ctx");
    (ctx, guard)
}

#[test]
fn search_footprints_finds_vendored_fixture() {
    let (ctx, _guard) = fixture_ctx();
    let tools = Tools::new();
    let out = tools
        .run("search_footprints", serde_json::json!({ "query": "R_0603" }), &ctx)
        .unwrap();
    let hits = out["hits"].as_array().expect("hits array");
    assert!(
        hits.iter().any(|h| h["lib_id"] == "Fixtures:R_0603_1608Metric"),
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
fn get_footprint_info_returns_pads_courtyard_bbox() {
    let (ctx, _guard) = fixture_ctx();
    let tools = Tools::new();
    let out = tools
        .run(
            "get_footprint_info",
            serde_json::json!({ "lib_id": "Fixtures:SOT-23" }),
            &ctx,
        )
        .unwrap();
    let pads = out["pads"].as_array().expect("pads array");
    assert_eq!(pads.len(), 3, "SOT-23 has 3 pads: {out}");
    let p0 = &pads[0];
    assert!(p0.get("number").is_some());
    assert!(p0.get("offset").is_some());
    assert!(p0.get("size").is_some());
    assert!(p0.get("technology").is_some());
    assert!(p0.get("layers").is_some());
    assert!(out.get("courtyard").is_some(), "courtyard present: {out}");
    assert!(out["courtyard"].get("width").is_some());
    assert!(out.get("bbox").is_some(), "bbox present: {out}");
}

#[test]
fn get_footprint_info_suggests_for_unknown_lib_id() {
    let (ctx, _guard) = fixture_ctx();
    let tools = Tools::new();
    let out = tools
        .run(
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

#[test]
fn create_board_resolves_vendored_footprints_and_persists_draft() {
    let (ctx, _guard) = fixture_ctx();
    let tools = Tools::new();

    // get_board before any board -> recoverable error.
    let out = tools.run("get_board", serde_json::json!({}), &ctx).unwrap();
    assert!(out["error"].as_str().is_some_and(|e| e.contains("no board")), "got: {out}");

    let board = serde_json::json!({
        "bounds": { "min_x": 0.0, "max_x": 30.0, "min_y": 0.0, "max_y": 20.0 },
        "parts": [
            { "reference": "R1", "footprint": "Fixtures:R_0603_1608Metric",
              "pad_nets": { "1": "VIN", "2": "MID" } },
            { "reference": "U1", "footprint": "Fixtures:SOT-23",
              "pad_nets": { "1": "MID", "2": "GND", "3": "VOUT" } },
            { "reference": "J1", "footprint": "Fixtures:PinHeader_1x02_P2.54mm_Vertical",
              "pad_nets": { "1": "VIN", "2": "GND" } }
        ]
    });
    let out = tools.run("create_board", board.clone(), &ctx).unwrap();
    assert_eq!(out["ok"], serde_json::json!(true), "got: {out}");
    assert_eq!(out["part_count"], serde_json::json!(3), "got: {out}");
    // VIN(2), MID(2), GND(2), VOUT(1) -> 4 nets; VOUT is a single-pin warning.
    assert_eq!(out["net_count"], serde_json::json!(4), "got: {out}");
    let warnings = out["warnings"].as_array().expect("warnings");
    assert!(
        warnings.iter().any(|w| w.as_str().unwrap().contains("VOUT")),
        "VOUT single-pin net should warn: {out}"
    );

    // The draft persisted; get_board returns it with a derived summary.
    let out = tools.run("get_board", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(out["summary"]["part_count"], serde_json::json!(3), "got: {out}");
    assert_eq!(out["summary"]["net_count"], serde_json::json!(4), "got: {out}");
    assert_eq!(out["summary"]["placed"], serde_json::json!(false));
    assert_eq!(out["summary"]["routed"], serde_json::json!(false));
    // Rules defaulted to the engine values.
    assert_eq!(out["draft"]["rules"]["clearance"], serde_json::json!(0.2), "got: {out}");
    assert_eq!(out["draft"]["rules"]["viaDiameter"], serde_json::json!(0.6), "got: {out}");

    // A second create_board without overwrite is rejected.
    let out = tools.run("create_board", board, &ctx).unwrap();
    assert!(out["error"].as_str().is_some_and(|e| e.contains("already exists")), "got: {out}");
}

#[test]
fn create_board_unknown_footprint_errors_with_suggestions() {
    let (ctx, _guard) = fixture_ctx();
    let tools = Tools::new();
    let out = tools
        .run(
            "create_board",
            serde_json::json!({
                "bounds": { "min_x": 0.0, "max_x": 10.0, "min_y": 0.0, "max_y": 10.0 },
                "parts": [
                    { "reference": "R1", "footprint": "Fixtures:R_0603_WRONG",
                      "pad_nets": { "1": "A", "2": "B" } }
                ]
            }),
            &ctx,
        )
        .unwrap();
    assert!(
        out["error"].as_str().is_some_and(|e| e.contains("R1") && e.contains("unknown footprint")),
        "got: {out}"
    );
    assert!(out.get("suggestions").is_some(), "expected suggestions: {out}");
}

#[test]
fn board_draft_round_trips_through_the_workspace() {
    use agent::tools_pcb::{BoardDraft, DraftPart, DraftRules};
    use pcb_engine::placement::PlacementHints;
    use pcb_engine::problem::Bounds;

    let (ctx, _guard) = fixture_ctx();
    let mut pad_nets = std::collections::BTreeMap::new();
    pad_nets.insert("1".to_string(), "VIN".to_string());
    pad_nets.insert("2".to_string(), "GND".to_string());

    let draft = BoardDraft {
        bounds: Bounds { min_x: 0.0, max_x: 30.0, min_y: 0.0, max_y: 20.0 },
        rules: DraftRules::default(),
        parts: vec![DraftPart {
            reference: "R1".into(),
            footprint: "Fixtures:R_0603_1608Metric".into(),
            pad_nets,
            locked: None,
        }],
        keepouts: vec![],
        hints: PlacementHints::default(),
        last_placement: None,
    };
    draft.save(&ctx).unwrap();
    let loaded = BoardDraft::load(&ctx).expect("draft loads back");
    assert_eq!(draft, loaded, "board draft must round-trip byte-equivalent");
}
