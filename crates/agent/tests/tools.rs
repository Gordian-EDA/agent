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
        "add_parts",
        "get_board",
        "place_board",
        "set_placement_hints",
        "set_constraints",
        "resize_board",
        "move_part",
        "unlock_part",
        "route_board",
        "render_board",
        "export_board",
        "review_design",
        "assign_footprints",
        "derive_board",
    ] {
        assert!(names.contains(&expected.to_string()), "missing {expected}");
    }
    assert_eq!(names.len(), 27, "expected exactly 27 tools, got {}: {:?}", names.len(), names);

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
blocks:
  a:
    components:
      R1: {part: Device:R, value: 1k, between: [N1, GND]}
      PWR1: {part: power:GND, pins: {1: GND}}
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
blocks:
  a:
    components:
      R1: {part: Device:R, value: 1k, between: [N1, GND]}
      PWR1: {part: power:GND, pins: {1: GND}}
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
blocks:
  a:
    components:
      R1: {part: Device:R, value: 1k, between: [N1, GND]}
      PWR1: {part: power:GND, pins: {1: GND}}
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
fn apply_design_surfaces_layout_warnings() {
    let Some(ctx) = agent::tools::ToolCtx::detect_for_test() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let tools = agent::tools::Tools::new();
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
    let out = tools.run("apply_design",
        serde_json::json!({ "yaml": yaml, "commit": true }), &ctx).unwrap();
    assert_eq!(out["written"], serde_json::json!(true));
    // The EmitOutput layout_warnings field is present in the result JSON.
    assert!(out["layout_warnings"].is_array(), "layout_warnings present: {out}");
    // A clean single-R layout has no collisions.
    assert_eq!(out["layout_warnings"].as_array().unwrap().len(), 0);

    // Dry-run path also carries them.
    let dry = tools.run("apply_design", serde_json::json!({ "yaml": yaml }), &ctx).unwrap();
    assert!(dry["layout_warnings"].is_array(), "dry-run layout_warnings: {dry}");
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
fn assign_footprints_validates_and_persists() {
    let (ctx, _guard) = fixture_ctx();
    let tools = Tools::new();

    // A valid fixture footprint is stored under its refdes.
    let out = tools
        .run(
            "assign_footprints",
            serde_json::json!({ "assignments": { "R1": "Fixtures:R_0603_1608Metric" } }),
            &ctx,
        )
        .unwrap();
    assert_eq!(out["ok"], serde_json::json!(true), "{out}");
    assert_eq!(out["footprints"]["R1"], serde_json::json!("Fixtures:R_0603_1608Metric"));

    // An unknown lib_id is reported in `unknown` and skipped; R1 still persists.
    let out2 = tools
        .run(
            "assign_footprints",
            serde_json::json!({ "assignments": { "R2": "Nope:DoesNotExist" } }),
            &ctx,
        )
        .unwrap();
    assert_eq!(out2["ok"], serde_json::json!(false), "{out2}");
    assert!(!out2["unknown"].as_array().unwrap().is_empty(), "unknown listed: {out2}");
    assert_eq!(out2["footprints"]["R1"], serde_json::json!("Fixtures:R_0603_1608Metric"));
    assert!(out2["footprints"].get("R2").is_none(), "bad lib_id not stored: {out2}");
}

#[test]
fn derive_board_builds_from_schematic_and_footprint_map() {
    // Needs a real KiCAD env (lift runs kicad-cli + resolves real footprints).
    let Some(ctx) = ToolCtx::detect_for_test() else {
        eprintln!("SKIP derive_board: no KiCAD env");
        return;
    };
    let tools = Tools::new();
    // Stage the RC-pair fixture as the project's schematic.
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../kicad-bridge/tests/fixtures/rc_pair.kicad_sch");
    std::fs::copy(&fixture, ctx.sch_path()).unwrap();

    let bounds = serde_json::json!({ "min_x": 0, "max_x": 20, "min_y": 0, "max_y": 12 });

    // No footprints assigned yet → derive_board reports the gaps (R1, C1).
    let gaps = tools
        .run("derive_board", serde_json::json!({ "bounds": bounds }), &ctx)
        .unwrap();
    if gaps.get("error").is_some() {
        eprintln!("SKIP derive_board: lift failed (kicad-cli unavailable?): {gaps}");
        return;
    }
    assert_eq!(gaps["ok"], serde_json::json!(false), "gaps reported: {gaps}");
    assert_eq!(gaps["needs_footprints"].as_array().unwrap().len(), 2, "R1 + C1: {gaps}");

    // Assign real footprints, then derive succeeds and builds the board.
    tools
        .run(
            "assign_footprints",
            serde_json::json!({ "assignments": {
                "R1": "Resistor_SMD:R_0603_1608Metric",
                "C1": "Capacitor_SMD:C_0603_1608Metric"
            }}),
            &ctx,
        )
        .unwrap();
    let out = tools
        .run("derive_board", serde_json::json!({ "bounds": bounds, "overwrite": true }), &ctx)
        .unwrap();
    assert!(out.get("error").is_none(), "derive succeeded: {out}");

    // The board draft now carries R1 + C1, derived from the schematic (not retyped).
    let board = tools.run("get_board", serde_json::json!({}), &ctx).unwrap();
    let s = board.to_string();
    assert!(s.contains("R1") && s.contains("C1"), "board has R1 + C1: {board}");
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
    // Lean shape: pad NUMBER list + a compact geometry summary (no per-pad coordinate dump).
    let nums = out["pad_numbers"].as_array().expect("pad_numbers array");
    assert_eq!(nums.len(), 3, "SOT-23 has 3 pads: {out}");
    assert!(nums.iter().all(|n| n.is_string()), "pad numbers are strings: {out}");
    assert_eq!(out["pad_count"], 3);
    assert!(out["min_pitch_mm"].as_f64().is_some_and(|p| p > 0.0), "min_pitch present: {out}");
    assert!(out.get("pad_min_dim_mm").is_some(), "pad dims present: {out}");
    assert!(out.get("technologies").is_some(), "technologies present: {out}");
    assert!(out.get("courtyard").is_some(), "courtyard present: {out}");
    assert!(out["courtyard"].get("width").is_some());
    assert!(out.get("bbox").is_some(), "bbox present: {out}");
    // Per-pad coordinate table is intentionally summarized away (model places nothing by coord).
    assert!(out.get("pads").is_none(), "per-pad table should be gone: {out}");
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
fn build_board_draft_resolves_vendored_footprints_and_persists() {
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
    let out = agent::tools_pcb::build_board_draft(board.clone(), &ctx).unwrap();
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
    let out = agent::tools_pcb::build_board_draft(board, &ctx).unwrap();
    assert!(out["error"].as_str().is_some_and(|e| e.contains("already exists")), "got: {out}");
}

#[test]
fn add_parts_appends_incrementally_and_rejects_duplicate() {
    let (ctx, _guard) = fixture_ctx();
    let tools = Tools::new();

    // add_parts before any board -> recoverable error.
    let pre = tools
        .run(
            "add_parts",
            serde_json::json!({ "parts": [] }),
            &ctx,
        )
        .unwrap();
    assert!(pre["error"].as_str().is_some_and(|e| e.contains("no board draft")), "got: {pre}");

    // Base board with two parts.
    let board = serde_json::json!({
        "bounds": { "min_x": 0.0, "max_x": 30.0, "min_y": 0.0, "max_y": 20.0 },
        "parts": [
            { "reference": "R1", "footprint": "Fixtures:R_0603_1608Metric",
              "pad_nets": { "1": "VIN", "2": "MID" } },
            { "reference": "U1", "footprint": "Fixtures:SOT-23",
              "pad_nets": { "1": "MID", "2": "GND", "3": "VOUT" } }
        ]
    });
    let out = agent::tools_pcb::build_board_draft(board, &ctx).unwrap();
    assert!(out.get("error").is_none(), "create_board ok: {out}");

    // Append a third part WITHOUT re-sending the first two.
    let add = serde_json::json!({
        "parts": [
            { "reference": "J1", "footprint": "Fixtures:PinHeader_1x02_P2.54mm_Vertical",
              "pad_nets": { "1": "VIN", "2": "GND" } }
        ]
    });
    let out = tools.run("add_parts", add, &ctx).unwrap();
    assert_eq!(out["part_count"], serde_json::json!(3), "appended to 3 parts: {out}");
    assert!(out["added"].as_array().unwrap().iter().any(|r| r == "J1"), "J1 reported added: {out}");

    // get_board reflects all three (the first two were NOT re-sent).
    let gb = tools.run("get_board", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(gb["summary"]["part_count"], serde_json::json!(3), "draft has 3 parts: {gb}");

    // A reference already on the board is rejected.
    let dup = serde_json::json!({
        "parts": [
            { "reference": "R1", "footprint": "Fixtures:R_0603_1608Metric",
              "pad_nets": { "1": "A", "2": "B" } }
        ]
    });
    let out = tools.run("add_parts", dup, &ctx).unwrap();
    assert!(
        out["error"].as_str().is_some_and(|e| e.contains("already on the board")),
        "duplicate reference rejected: {out}"
    );
}

#[test]
fn build_board_draft_unknown_footprint_errors_with_suggestions() {
    let (ctx, _guard) = fixture_ctx();
    let out = agent::tools_pcb::build_board_draft(
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
        last_place_illegal: false,
        outline: None,
    };
    draft.save(&ctx).unwrap();
    let loaded = BoardDraft::load(&ctx).expect("draft loads back");
    assert_eq!(draft, loaded, "board draft must round-trip byte-equivalent");
}

// ── PCB tools (slice 5, Task 2): place / route / constraints / triage ─────────
//
// These use the same vendored-fixture footprint index as the Task 1 tests (no
// KiCAD install needed). The standard board is a small 3-part divider-ish board
// whose nets each have ≥2 pins, so it places legal and routes with zero failures.

/// Create the standard small board (R1 + U1 + J1, three 2-pin nets) on a fresh
/// fixture ctx. Returns the ctx, its tempdir guard, and the Tools registry.
fn placed_board_ctx() -> (ToolCtx, tempfile::TempDir, Tools) {
    let (ctx, guard) = fixture_ctx();
    let tools = Tools::new();
    let board = serde_json::json!({
        "bounds": { "min_x": 0.0, "max_x": 30.0, "min_y": 0.0, "max_y": 20.0 },
        "parts": [
            { "reference": "R1", "footprint": "Fixtures:R_0603_1608Metric",
              "pad_nets": { "1": "VIN", "2": "MID" } },
            { "reference": "R2", "footprint": "Fixtures:R_0603_1608Metric",
              "pad_nets": { "1": "MID", "2": "GND" } },
            { "reference": "J1", "footprint": "Fixtures:PinHeader_1x02_P2.54mm_Vertical",
              "pad_nets": { "1": "VIN", "2": "GND" } }
        ]
    });
    let out = agent::tools_pcb::build_board_draft(board, &ctx).unwrap();
    assert_eq!(out["ok"], serde_json::json!(true), "create_board: {out}");
    (ctx, guard, tools)
}

#[test]
fn place_board_failure_suggests_a_larger_bounds() {
    // Three parts crammed into a 3x3 mm board cannot fit; the failure must hand the
    // agent a CONCRETE, larger min-bounds suggestion so it can retry deterministically.
    let (ctx, _g) = fixture_ctx();
    let tools = Tools::new();
    let board = serde_json::json!({
        "bounds": { "min_x": 0.0, "max_x": 3.0, "min_y": 0.0, "max_y": 3.0 },
        "parts": [
            { "reference": "J1", "footprint": "Fixtures:PinHeader_1x02_P2.54mm_Vertical",
              "pad_nets": { "1": "A", "2": "B" } },
            { "reference": "J2", "footprint": "Fixtures:PinHeader_1x02_P2.54mm_Vertical",
              "pad_nets": { "1": "A", "2": "B" } },
            { "reference": "R1", "footprint": "Fixtures:R_0603_1608Metric",
              "pad_nets": { "1": "A", "2": "B" } }
        ]
    });
    agent::tools_pcb::build_board_draft(board, &ctx).unwrap();
    let out = tools.run("place_board", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(out["legal"], serde_json::json!(false), "should not fit in 3x3: {out}");
    let s = &out["suggested_min_bounds_mm"];
    let (w, h) = (s["w"].as_f64().unwrap(), s["h"].as_f64().unwrap());
    assert!(w > 3.0 && h > 3.0, "suggestion must exceed the failing bounds: {out}");
    assert!(
        out["parts_courtyard_area_mm2"].as_f64().unwrap() > 0.0,
        "must report the parts' courtyard area: {out}"
    );
}

#[test]
fn locked_part_rejects_non_axis_aligned_rotation() {
    // A 45° lock must be rejected at the surface (the placer/synth are axis-aligned
    // only) with a clear message — not silently routed to wrong pads then failed at
    // export. 0/90/180/270 are accepted.
    let (ctx, _g) = fixture_ctx();
    let bad = agent::tools_pcb::build_board_draft(
        serde_json::json!({
            "bounds": { "min_x": 0.0, "max_x": 30.0, "min_y": 0.0, "max_y": 20.0 },
            "parts": [
                { "reference": "U1", "footprint": "Fixtures:R_0603_1608Metric",
                  "pad_nets": { "1": "A", "2": "B" },
                  "locked": { "x": 15.0, "y": 10.0, "rotation": 45 } }
            ]
        }),
        &ctx,
    )
    .unwrap();
    assert!(
        bad["error"].as_str().is_some_and(|e| e.contains("not supported")),
        "45° lock must be rejected: {bad}"
    );
    let ok = agent::tools_pcb::build_board_draft(
        serde_json::json!({
            "overwrite": true,
            "bounds": { "min_x": 0.0, "max_x": 30.0, "min_y": 0.0, "max_y": 20.0 },
            "parts": [
                { "reference": "U1", "footprint": "Fixtures:R_0603_1608Metric",
                  "pad_nets": { "1": "A", "2": "B" },
                  "locked": { "x": 15.0, "y": 10.0, "rotation": 90 } }
            ]
        }),
        &ctx,
    )
    .unwrap();
    assert_eq!(ok["ok"], serde_json::json!(true), "90° lock must be accepted: {ok}");
}

#[test]
fn full_flow_create_place_route_is_clean() {
    let (ctx, _g, tools) = placed_board_ctx();

    // route_board before any placement -> recoverable error.
    let out = tools.run("route_board", serde_json::json!({}), &ctx).unwrap();
    assert!(
        out["error"].as_str().is_some_and(|e| e.contains("place_board")),
        "route before place must tell the model to place first: {out}"
    );

    // place_board -> legal, positions for every part.
    let out = tools.run("place_board", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(out["legal"], serde_json::json!(true), "place_board: {out}");
    let positions = out["positions"].as_array().expect("positions");
    assert_eq!(positions.len(), 3, "one position per part: {out}");
    for p in positions {
        assert!(p.get("reference").is_some() && p.get("x").is_some()
            && p.get("y").is_some() && p.get("rotation").is_some(), "position shape: {p}");
    }
    assert!(out["hpwl"].as_f64().unwrap() >= 0.0);

    // get_board now reports placed=true.
    let out = tools.run("get_board", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(out["summary"]["placed"], serde_json::json!(true), "{out}");

    // route_board -> zero failed, lint clean (no engine_bug), real metrics.
    let out = tools.run("route_board", serde_json::json!({}), &ctx).unwrap();
    assert!(out["failed"].as_array().unwrap().is_empty(),
        "the small board must route with zero failed nets: {out}");
    assert!(out.get("engine_bug").is_none(),
        "a clean route must NOT flag an engine bug: {out}");
    // lint_summary is an object with no counts (all zero).
    assert!(out["lint_summary"].as_object().unwrap().is_empty(),
        "lint_summary must be empty (zero violations): {out}");
    assert!(out["metrics"]["wirelength"].as_f64().unwrap() > 0.0, "{out}");
    assert!(out["metrics"]["traces"].as_u64().unwrap() > 0, "{out}");
    // Either engine may win (route_auto picks the fewer-failed result); just
    // assert the provenance tag is one of the two honest values.
    assert!(
        matches!(out["router"].as_str(), Some("naive") | Some("detailed")),
        "router must be naive or detailed: {out}"
    );

    // The solution persisted; get_board reports routed=true.
    let out = tools.run("get_board", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(out["summary"]["routed"], serde_json::json!(true), "{out}");
}

#[test]
fn export_board_requires_place_and_route_then_writes_parseable_board() {
    use kicad_bridge::pcb::{extract_copper, read_problem};

    let (ctx, _g, tools) = placed_board_ctx();

    // export before placement -> recoverable error pointing at place_board.
    let out = tools.run("export_board", serde_json::json!({}), &ctx).unwrap();
    assert!(
        out["error"].as_str().is_some_and(|e| e.contains("place")),
        "export before place must tell the model to place first: {out}"
    );

    // place, but not yet routed -> recoverable error pointing at route_board.
    tools.run("place_board", serde_json::json!({}), &ctx).unwrap();
    let out = tools.run("export_board", serde_json::json!({}), &ctx).unwrap();
    assert!(
        out["error"].as_str().is_some_and(|e| e.contains("rout")),
        "export before route must tell the model to route first: {out}"
    );

    // route, then export to an explicit path.
    let routed = tools.run("route_board", serde_json::json!({}), &ctx).unwrap();
    assert!(routed["failed"].as_array().unwrap().is_empty(), "route: {routed}");

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("exported.kicad_pcb");
    let out = tools
        .run(
            "export_board",
            serde_json::json!({ "path": path.display().to_string() }),
            &ctx,
        )
        .unwrap();
    assert_eq!(out["ok"], serde_json::json!(true), "export_board: {out}");
    assert_eq!(out["part_count"], serde_json::json!(3), "{out}");
    assert_eq!(out["failed_nets"], serde_json::json!(0), "{out}");
    assert!(path.exists(), "export must write the board file");

    // The written board parses with read_problem: three parts' pads, the nets,
    // and the routed copper all round-trip.
    let board = read_problem(&path).expect("read_problem on exported board");
    for n in ["VIN", "MID", "GND"] {
        assert!(board.net_codes.contains_key(n), "net {n} missing: {:?}", board.net_codes);
    }
    let copper = extract_copper(&path).expect("extract_copper");
    assert!(!copper.traces.is_empty(), "exported board carries routed copper");

    // Silkscreen is kept so the board renders like a real PCB: reference
    // designators on F.SilkS and component outline graphics. (The Value property
    // is hidden, not rendered, so the long footprint name never clutters.)
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.contains("(layer \"F.SilkS\")"),
        "exported board must keep silkscreen (refs + outlines) for a real-board render"
    );
    assert!(
        text.contains("(property \"Value\"") && text.contains("(hide yes)"),
        "the Value property must be hidden, not rendered at full size"
    );

    // DRC is reported as run-or-skipped depending on the environment.
    assert!(out["drc"].get("ran").is_some(), "drc status present: {out}");
}

/// Slice-5 export gate (KiCAD-gated): the agent-tools flow
/// create_board → place_board → route_board → export_board on a small vendored
/// circuit must yield a board KiCAD's DRC finds zero copper-violation /
/// unconnected (only the inert `lib_footprint_mismatch` warning is tolerated).
/// Skips visibly when no KiCAD >= 8 is installed.
#[test]
fn export_board_e2e_kicad_drc_clean() {
    // Build the fixture-footprint ctx; it carries a real KiCAD env when one is
    // installed (with_footprint_dir_for_test falls back to KicadEnv::detect).
    let (ctx, _g, tools) = placed_board_ctx();
    let major: u32 = ctx
        .env()
        .cli_version
        .split('.')
        .next()
        .and_then(|m| m.parse().ok())
        .unwrap_or(0);
    if major < 8 {
        eprintln!("SKIP: no KiCAD >= 8 for export_board DRC e2e");
        return;
    }

    tools.run("place_board", serde_json::json!({}), &ctx).unwrap();
    let routed = tools.run("route_board", serde_json::json!({}), &ctx).unwrap();
    assert!(routed["failed"].as_array().unwrap().is_empty(), "route: {routed}");

    let out = tools.run("export_board", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(out["ok"], serde_json::json!(true), "export_board: {out}");

    let drc = &out["drc"];
    assert_eq!(drc["ran"], serde_json::json!(true), "DRC should have run: {out}");
    // Strict: zero copper-layer violations of ANY severity (the detailed router's
    // spurious via_dangling vias are dropped at the stitch source, so this stays
    // clean — and now guards against that regression).
    assert_eq!(
        drc["copper_violations"], serde_json::json!(0),
        "exported board must be copper-DRC-clean: {out}"
    );
    assert_eq!(
        drc["unconnected_items"], serde_json::json!(0),
        "exported board must have zero unconnected items: {out}"
    );
    assert_eq!(
        drc["errors"], serde_json::json!(0),
        "exported board must have zero error-severity DRC findings: {out}"
    );

    eprintln!(
        "export_board e2e OK (KiCAD {}): synthesized board DRC copper-clean, \
         0 unconnected, {} tolerated footprint warning(s)",
        ctx.env().cli_version,
        drc["tolerated_footprint_warnings"]
    );
}

#[test]
fn keepout_wall_changes_routing_outcome() {
    use agent::tools_pcb::BoardDraft;

    // A board whose two connected parts are LOCKED on opposite sides, forced to
    // route across the middle, then a keepout WALL on both layers spanning the
    // whole height down the centre. With the wall, the only corridor is gone, so
    // routing must differ — fail honestly or take a longer path — vs without it.
    let (ctx, _g) = fixture_ctx();
    let tools = Tools::new();
    let board = serde_json::json!({
        "bounds": { "min_x": 0.0, "max_x": 40.0, "min_y": 0.0, "max_y": 20.0 },
        "parts": [
            { "reference": "J1", "footprint": "Fixtures:PinHeader_1x02_P2.54mm_Vertical",
              "pad_nets": { "1": "A", "2": "B" } },
            { "reference": "J2", "footprint": "Fixtures:PinHeader_1x02_P2.54mm_Vertical",
              "pad_nets": { "1": "A", "2": "B" } }
        ]
    });
    assert_eq!(agent::tools_pcb::build_board_draft(board, &ctx).unwrap()["ok"], serde_json::json!(true));

    // Lock J1 on the far west, J2 on the far east — connected by nets A and B.
    tools.run("move_part",
        serde_json::json!({ "reference": "J1", "x": 3.0, "y": 10.0 }), &ctx).unwrap();
    tools.run("move_part",
        serde_json::json!({ "reference": "J2", "x": 37.0, "y": 10.0 }), &ctx).unwrap();

    // Baseline: place + route with NO keepout.
    assert_eq!(tools.run("place_board", serde_json::json!({}), &ctx).unwrap()["legal"],
        serde_json::json!(true));
    let base = tools.run("route_board", serde_json::json!({}), &ctx).unwrap();
    let base_failed = base["failed"].as_array().unwrap().len();
    let base_wirelength = base["metrics"]["wirelength"].as_f64().unwrap();

    // Now add a full-height wall keepout on BOTH layers down the middle.
    let out = tools.run("set_constraints", serde_json::json!({
        "keepouts": [
            { "rect": { "min_x": 19.0, "max_x": 21.0, "min_y": 0.0, "max_y": 20.0 },
              "layers": ["top", "bottom"] }
        ]
    }), &ctx).unwrap();
    assert_eq!(out["ok"], serde_json::json!(true), "set_constraints: {out}");
    assert_eq!(out["keepout_count"], serde_json::json!(1), "{out}");
    // Placement stands (rules/keepouts don't move parts).
    assert_eq!(out["placement_kept"], serde_json::json!(true), "{out}");
    // The route was invalidated and cleared.
    assert_eq!(out["route_cleared"], serde_json::json!(true), "{out}");
    let draft = BoardDraft::load(&ctx).expect("draft");
    assert!(draft.last_placement.is_some(), "set_constraints keeps the placement");
    assert!(ctx.workspace().read_route().is_none(), "route.json cleared");

    // Re-route WITH the wall: the outcome must be observably different.
    let walled = tools.run("route_board", serde_json::json!({}), &ctx).unwrap();
    let walled_failed = walled["failed"].as_array().unwrap().len();
    let walled_wirelength = walled["metrics"]["wirelength"].as_f64().unwrap();

    let differs = walled_failed != base_failed
        || (walled_wirelength - base_wirelength).abs() > 1e-6;
    assert!(
        differs,
        "a full-height wall keepout on both layers must change the route \
         (failed: {base_failed} -> {walled_failed}, wirelength: {base_wirelength} -> {walled_wirelength})"
    );
}

#[test]
fn move_part_lock_is_honored_by_place_board() {
    let (ctx, _g, tools) = placed_board_ctx();

    // Pin U1... actually our standard board has R1/R2/J1; pin R2 at a corner.
    let out = tools.run("move_part",
        serde_json::json!({ "reference": "R2", "x": 25.0, "y": 5.0, "rotation": 90 }), &ctx).unwrap();
    assert_eq!(out["ok"], serde_json::json!(true), "move_part: {out}");

    let out = tools.run("place_board", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(out["legal"], serde_json::json!(true), "{out}");
    let r2 = out["positions"].as_array().unwrap().iter()
        .find(|p| p["reference"] == "R2").expect("R2 placed");
    assert_eq!(r2["x"], serde_json::json!(25.0), "locked R2 keeps its x: {out}");
    assert_eq!(r2["y"], serde_json::json!(5.0), "locked R2 keeps its y: {out}");
    assert_eq!(r2["rotation"], serde_json::json!(90), "locked R2 keeps its rotation: {out}");

    // Out-of-bounds move is a recoverable error.
    let out = tools.run("move_part",
        serde_json::json!({ "reference": "R2", "x": 999.0, "y": 5.0 }), &ctx).unwrap();
    assert!(out["error"].as_str().is_some_and(|e| e.contains("outside the board")), "{out}");

    // Unknown reference is a recoverable error.
    let out = tools.run("move_part",
        serde_json::json!({ "reference": "ZZ", "x": 5.0, "y": 5.0 }), &ctx).unwrap();
    assert!(out["error"].as_str().is_some_and(|e| e.contains("unknown reference")), "{out}");

    // unlock_part releases the lock.
    let out = tools.run("unlock_part", serde_json::json!({ "reference": "R2" }), &ctx).unwrap();
    assert_eq!(out["ok"], serde_json::json!(true), "{out}");
    assert_eq!(out["was_locked"], serde_json::json!(true), "{out}");
}

#[test]
fn set_constraints_rejects_net_classes() {
    let (ctx, _g, tools) = placed_board_ctx();
    let out = tools.run("set_constraints", serde_json::json!({
        "net_classes": [ { "name": "power", "clearance": 0.5 } ]
    }), &ctx).unwrap();
    assert!(
        out["error"].as_str().is_some_and(|e| e.contains("net classes")
            && e.contains("not yet supported")),
        "net_classes must be honestly rejected: {out}"
    );
}

#[test]
fn set_constraints_partial_rules_merge_and_clear_route() {
    let (ctx, _g, tools) = placed_board_ctx();
    // Place + route so there's a route.json to invalidate.
    tools.run("place_board", serde_json::json!({}), &ctx).unwrap();
    tools.run("route_board", serde_json::json!({}), &ctx).unwrap();
    assert!(ctx.workspace().read_route().is_some(), "routed");

    // Partial rules update: only clearance changes.
    let out = tools.run("set_constraints", serde_json::json!({
        "rules": { "clearance": 0.3 }
    }), &ctx).unwrap();
    assert_eq!(out["ok"], serde_json::json!(true), "{out}");
    assert_eq!(out["rules"]["clearance"], serde_json::json!(0.3), "{out}");
    // The other rules kept their defaults (partial update).
    assert_eq!(out["rules"]["min_trace_width"], serde_json::json!(0.2), "{out}");
    assert_eq!(out["route_cleared"], serde_json::json!(true), "{out}");
    assert!(ctx.workspace().read_route().is_none(), "route cleared by rule change");
}

#[test]
fn resize_board_enlarges_keeps_parts_and_clears_state() {
    let (ctx, _g, tools) = placed_board_ctx();
    // Place + route so there is placement + route state for resize to clear.
    tools.run("place_board", serde_json::json!({}), &ctx).unwrap();
    tools.run("route_board", serde_json::json!({}), &ctx).unwrap();
    assert!(ctx.workspace().read_route().is_some(), "routed before resize");

    let out = tools
        .run(
            "resize_board",
            serde_json::json!({ "bounds": { "min_x": 0.0, "max_x": 200.0, "min_y": 0.0, "max_y": 200.0 } }),
            &ctx,
        )
        .unwrap();
    assert_eq!(out["ok"], serde_json::json!(true), "{out}");
    assert_eq!(out["bounds"]["max_x"], serde_json::json!(200.0), "{out}");
    assert!(out["parts_kept"].as_u64().is_some_and(|n| n > 0), "parts kept: {out}");
    assert_eq!(out["route_cleared"], serde_json::json!(true), "{out}");
    assert!(ctx.workspace().read_route().is_none(), "route cleared by resize");

    // Parts were kept, so a fresh place succeeds into the new (larger) bounds.
    let p = tools.run("place_board", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(p["legal"], serde_json::json!(true), "re-place legal after enlarge: {p}");
}

#[test]
fn resize_board_closes_the_too_tight_placement_trap() {
    // The deterministic proof of the convergence fix: a board too small for its parts is illegal,
    // and resize_board (not move_part, not re-create_board) is what makes the re-place legal.
    let (ctx, _g) = fixture_ctx();
    let tools = Tools::new();
    let board = serde_json::json!({
        "bounds": { "min_x": 0.0, "max_x": 4.0, "min_y": 0.0, "max_y": 4.0 },
        "parts": [
            { "reference": "J1", "footprint": "Fixtures:PinHeader_1x02_P2.54mm_Vertical",
              "pad_nets": { "1": "A", "2": "B" } },
            { "reference": "R1", "footprint": "Fixtures:R_0603_1608Metric",
              "pad_nets": { "1": "A", "2": "B" } }
        ]
    });
    assert_eq!(agent::tools_pcb::build_board_draft(board, &ctx).unwrap()["ok"], serde_json::json!(true));

    // Too tight → place_board reports illegal + a suggested larger bounds.
    let p = tools.run("place_board", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(p["legal"], serde_json::json!(false), "a 4x4 board can't hold a 1x2 header: {p}");
    let sw = p["suggested_min_bounds_mm"]["w"].as_f64().expect("suggested w");
    let sh = p["suggested_min_bounds_mm"]["h"].as_f64().expect("suggested h");

    // The cheap lever: resize to the suggestion (no re-create_board, parts kept).
    let r = tools
        .run(
            "resize_board",
            serde_json::json!({ "bounds": { "min_x": 0.0, "max_x": sw + 2.0, "min_y": 0.0, "max_y": sh + 2.0 } }),
            &ctx,
        )
        .unwrap();
    assert_eq!(r["ok"], serde_json::json!(true), "resize: {r}");
    assert_eq!(r["parts_kept"], serde_json::json!(2), "both parts kept: {r}");

    // Re-place is now LEGAL — the trap is closed without re-sending parts.
    let p2 = tools.run("place_board", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(p2["legal"], serde_json::json!(true), "legal after resize: {p2}");
}

#[test]
fn set_placement_hints_validates_members() {
    let (ctx, _g, tools) = placed_board_ctx();

    // Valid: a group over known references.
    let out = tools.run("set_placement_hints", serde_json::json!({
        "groups": [ { "name": "resistors", "members": ["R1", "R2"], "edge": "w" } ]
    }), &ctx).unwrap();
    assert_eq!(out["ok"], serde_json::json!(true), "{out}");
    assert_eq!(out["group_count"], serde_json::json!(1), "{out}");
    assert_eq!(out["groups"][0]["edge"], serde_json::json!(true), "edge recorded: {out}");

    // Invalid member -> recoverable error listing known refs.
    let out = tools.run("set_placement_hints", serde_json::json!({
        "groups": [ { "name": "bad", "members": ["R1", "NOPE"] } ]
    }), &ctx).unwrap();
    assert!(
        out["error"].as_str().is_some_and(|e| e.contains("NOPE") && e.contains("R1")),
        "unknown member must error listing known refs: {out}"
    );
}

// ── PCB tools (slice 5, Task 3): render_board ─────────────────────────────────

/// PNG magic bytes — the 8-byte header all valid PNG files start with.
const PNG_MAGIC: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

#[test]
fn render_board_before_create_is_recoverable_error() {
    let (ctx, _guard) = fixture_ctx();
    let tools = Tools::new();
    let out = tools.run("render_board", serde_json::json!({}), &ctx).unwrap();
    assert!(
        out["error"].as_str().is_some_and(|e| e.contains("no board")),
        "render_board before create_board must be a recoverable error: {out}"
    );
}

#[test]
fn render_board_before_place_is_recoverable_error() {
    let (ctx, _guard) = fixture_ctx();
    let tools = Tools::new();
    // Create board but do NOT place.
    let board = serde_json::json!({
        "bounds": { "min_x": 0.0, "max_x": 30.0, "min_y": 0.0, "max_y": 20.0 },
        "parts": [
            { "reference": "R1", "footprint": "Fixtures:R_0603_1608Metric",
              "pad_nets": { "1": "VIN", "2": "GND" } }
        ]
    });
    assert_eq!(agent::tools_pcb::build_board_draft(board, &ctx).unwrap()["ok"], serde_json::json!(true));

    // No placement yet: both explicit "placed" and auto (no route) must error.
    let out = tools.run("render_board", serde_json::json!({"view": "placed"}), &ctx).unwrap();
    assert!(
        out["error"].as_str().is_some_and(|e| e.contains("place_board")),
        "render placed before place must error: {out}"
    );
    let out = tools.run("render_board", serde_json::json!({}), &ctx).unwrap();
    assert!(
        out["error"].as_str().is_some_and(|e| e.contains("place_board")),
        "render auto (no route) before place must error: {out}"
    );

    // Explicit "routed" view before route.json is a distinct error.
    let out = tools.run("render_board", serde_json::json!({"view": "routed"}), &ctx).unwrap();
    assert!(
        out["error"].as_str().is_some_and(|e| e.contains("route_board")),
        "render routed before route must error: {out}"
    );
}

#[test]
fn render_board_placed_returns_ok_and_png_magic() {
    let (ctx, _g, tools) = placed_board_ctx();

    // Place the board.
    let out = tools.run("place_board", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(out["legal"], serde_json::json!(true), "place: {out}");

    // Render the placed view.
    let out = tools
        .run("render_board", serde_json::json!({"view": "placed"}), &ctx)
        .unwrap();
    assert_eq!(out["ok"], serde_json::json!(true), "render_board placed: {out}");
    assert_eq!(out["view"], serde_json::json!("placed"), "view field: {out}");

    let png_path = out["png_path"].as_str().expect("png_path present");
    assert!(png_path.contains(".autopcb/renders/"), "path under renders/: {out}");
    let png_bytes = std::fs::read(png_path).expect("PNG file written");
    assert_eq!(&png_bytes[..8], PNG_MAGIC, "must be a valid PNG");
    assert!(png_bytes.len() > 100, "PNG suspiciously small: {} bytes", png_bytes.len());

    // IMAGE_PATH_KEY must be set to the same path (so the agent loop attaches it).
    assert_eq!(
        out[agent::tools::IMAGE_PATH_KEY].as_str(),
        Some(png_path),
        "IMAGE_PATH_KEY must equal png_path"
    );
}

#[test]
fn render_board_routed_returns_ok_and_png_magic() {
    let (ctx, _g, tools) = placed_board_ctx();

    // Full flow: place then route.
    tools.run("place_board", serde_json::json!({}), &ctx).unwrap();
    let route_out = tools.run("route_board", serde_json::json!({}), &ctx).unwrap();
    assert!(
        route_out["failed"].as_array().unwrap().is_empty(),
        "small board must route cleanly: {route_out}"
    );

    // Render the routed view explicitly.
    let out = tools
        .run("render_board", serde_json::json!({"view": "routed"}), &ctx)
        .unwrap();
    assert_eq!(out["ok"], serde_json::json!(true), "render_board routed: {out}");
    assert_eq!(out["view"], serde_json::json!("routed"), "view field: {out}");

    let png_path = out["png_path"].as_str().expect("png_path present");
    let png_bytes = std::fs::read(png_path).expect("PNG file written");
    assert_eq!(&png_bytes[..8], PNG_MAGIC, "must be a valid PNG");

    // IMAGE_PATH_KEY set.
    assert_eq!(
        out[agent::tools::IMAGE_PATH_KEY].as_str(),
        Some(png_path),
        "IMAGE_PATH_KEY must equal png_path"
    );
}

#[test]
fn render_board_default_view_logic() {
    let (ctx, _g, tools) = placed_board_ctx();

    // After place but before route: auto should pick "placed".
    tools.run("place_board", serde_json::json!({}), &ctx).unwrap();
    let out = tools.run("render_board", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(out["ok"], serde_json::json!(true), "auto before route: {out}");
    assert_eq!(out["view"], serde_json::json!("placed"), "default before route must be placed: {out}");

    // After route: auto should pick "routed".
    tools.run("route_board", serde_json::json!({}), &ctx).unwrap();
    let out = tools.run("render_board", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(out["ok"], serde_json::json!(true), "auto after route: {out}");
    assert_eq!(out["view"], serde_json::json!("routed"), "default after route must be routed: {out}");
}

