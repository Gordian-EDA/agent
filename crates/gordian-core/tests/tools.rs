//! Deterministic tests for the six-tool registry (no LLM, no network).
//!
//! Every test is SKIP-graceful: if no KiCAD installation is detected,
//! [`AgentRuntime::detect_for_test`] returns `None` and the test prints `SKIP` and
//! returns rather than failing. The tests that touch real symbol libraries and
//! `kicad-cli` therefore only assert on machines with KiCAD installed (the
//! project's test environment has KiCAD 10.0.3).

use gordian_core::AgentRuntime;
use gordian_core::tools::{run_tool, tool_defs};

/// A tiny self-contained valid design: one resistor between two named nets.
const TINY_YAML: &str =
    "version: 1\nblocks: {main: {components: {R1: {part: Device:R, pins: {1: A, 2: GND}}}}}";

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

    let out = run_tool(
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
fn apply_design_rejects_public_commit_argument() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };

    let out = run_tool(
        "apply_design",
        serde_json::json!({ "yaml": TINY_YAML, "commit": true }),
        &ctx,
    )
    .unwrap();

    assert!(
        out["error"]
            .as_str()
            .is_some_and(|e| e.contains("commit") && e.contains("removed")),
        "old commit arg should be rejected clearly: {out}"
    );
}

#[test]
fn apply_design_commit_writes_file_and_runs_erc() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    assert!(!ctx.sch_path().exists(), "fixture starts with no schematic");

    let out = run_tool(
        "apply_design",
        serde_json::json!({ "yaml": TINY_YAML, "__commit": true }),
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
        "autoroute",
        "render_board",
        "check_board",
        "export_fab",
        "review_design",
        "regenerate_board",
        "assign_footprints",
        "open_board",
        "board_state",
        "move_part",
        "route_track",
        "set_net_width",
    ];
    for expected in expected_tools {
        assert!(names.contains(&expected.to_string()), "missing {expected}");
    }
    // The Board-DSL authoring surface (design_board/import_board) and the old
    // board mutators are GONE — the board is seeded from the schematic
    // (regenerate_board) and edited INTERACTIVELY over KiCAD IPC (open_board +
    // move_part/route_track/set_net_width).
    for gone in [
        "set_placement_hints",
        "set_constraints",
        "resize_board",
        "unlock_part",
        "design_board",
        "import_board",
    ] {
        assert!(
            !names.contains(&gone.to_string()),
            "legacy tool still present: {gone}"
        );
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
            assert!(props.contains_key("yaml"));
            assert!(
                !props.contains_key("commit"),
                "apply_design commit flag must not be model-facing: {schema}"
            );
        }
    }
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
    run_tool(
        "apply_design",
        serde_json::json!({ "yaml": TINY_YAML, "__commit": true }),
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
    run_tool(
        "apply_design",
        serde_json::json!({ "yaml": TINY_YAML, "__commit": true }),
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
    run_tool(
        "apply_design",
        serde_json::json!({ "yaml": TINY_YAML, "__commit": true }),
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
    let out = run_tool(
        "apply_design",
        serde_json::json!({ "yaml": TINY_YAML, "__commit": true }),
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

    // apply_design with NO yaml applies the draft when the gate's commit phase invokes it.
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
    // Write a schematic with explicit yaml (no draft involved).
    run_tool(
        "apply_design",
        serde_json::json!({"yaml": yaml, "__commit": true}),
        &ctx,
    )
    .unwrap();

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
    let applied = run_tool(
        "apply_design",
        serde_json::json!({ "yaml": yaml, "__commit": true }),
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
    let out = run_tool(
        "apply_design",
        serde_json::json!({ "yaml": yaml, "__commit": true }),
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
    let dry = run_tool("apply_design", serde_json::json!({ "yaml": yaml }), &ctx).unwrap();
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
    assert!(
        assigned.get("diagnostics").is_none(),
        "compact success should omit diagnostics: {assigned}"
    );
    assert!(
        assigned.get("draft_written").is_none(),
        "compact success should omit draft_written: {assigned}"
    );
    let draft = ctx
        .workspace()
        .read_draft()
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

    let draft = ctx.workspace().read_draft().expect("draft after batch");
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

    let written = run_tool(
        "apply_design",
        serde_json::json!({ "yaml": TINY_YAML, "__commit": true }),
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

#[test]
fn build_seed_board_resolves_vendored_footprints_and_persists() {
    let (ctx, _guard) = fixture_ctx();

    // get_board before any board -> recoverable error.
    let out = run_tool("get_board", serde_json::json!({}), &ctx).unwrap();
    assert!(
        out["error"]
            .as_str()
            .is_some_and(|e| e.contains("no board") || e.contains("board not found")),
        "got: {out}"
    );

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
    let out = gordian_core::tools_pcb::build_seed_board(board.clone(), &ctx).unwrap();
    assert_eq!(out["ok"], serde_json::json!(true), "got: {out}");
    assert_eq!(out["part_count"], serde_json::json!(3), "got: {out}");
    // VIN(2), MID(2), GND(2), VOUT(1) -> 4 nets; VOUT is a single-pin warning.
    assert_eq!(out["net_count"], serde_json::json!(4), "got: {out}");
    let warnings = out["warnings"].as_array().expect("warnings");
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap().contains("VOUT")),
        "VOUT single-pin net should warn: {out}"
    );

    assert!(
        ctx.pcb_path().exists(),
        "builder should seed the project board"
    );
}

#[test]
fn build_seed_board_unknown_footprint_errors_with_suggestions() {
    let (ctx, _guard) = fixture_ctx();
    let out = gordian_core::tools_pcb::build_seed_board(
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
        out["error"]
            .as_str()
            .is_some_and(|e| e.contains("R1") && e.contains("unknown footprint")),
        "got: {out}"
    );
    assert!(
        out.get("suggestions").is_some(),
        "expected suggestions: {out}"
    );
}

#[test]
fn board_seed_round_trips_as_adapter_json() {
    use gordian_core::tools_pcb::{BoardSeed, BoardSeedPart, BoardSeedRules};
    use pcb_model::Rect;
    use pcb_place::placement::PlacementHints;

    let mut pad_nets = std::collections::BTreeMap::new();
    pad_nets.insert("1".to_string(), "VIN".to_string());
    pad_nets.insert("2".to_string(), "GND".to_string());

    let seed = BoardSeed {
        bounds: Rect {
            min_x: 0.0,
            max_x: 30.0,
            min_y: 0.0,
            max_y: 20.0,
        },
        rules: BoardSeedRules::default(),
        parts: vec![BoardSeedPart {
            reference: "R1".into(),
            footprint: "Fixtures:R_0603_1608Metric".into(),
            pad_nets,
            locked: None,
        }],
        keepouts: vec![],
        hints: PlacementHints::default(),
        outline: None,
    };
    let raw = serde_json::to_string_pretty(&seed).unwrap();
    let loaded: BoardSeed = serde_json::from_str(&raw).unwrap();
    assert_eq!(
        seed, loaded,
        "board seed adapter must round-trip byte-equivalent"
    );
}

// ── PCB tools: place / route / constraints / triage ──────────────────────────
//
// These use the same vendored-fixture footprint index as the Task 1 tests (no
// KiCAD install needed). The standard board is a small 3-part divider-ish board
// whose nets each have ≥2 pins, so it places legal and routes with zero failures.

/// Create the standard small board (R1 + U1 + J1, three 2-pin nets) on a fresh
/// fixture ctx. Returns the ctx and its tempdir guard.
fn placed_board_ctx() -> (AgentRuntime, tempfile::TempDir) {
    let (ctx, guard) = fixture_ctx();
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
    let out = gordian_core::tools_pcb::build_seed_board(board, &ctx).unwrap();
    assert_eq!(
        out["ok"],
        serde_json::json!(true),
        "build_seed_board: {out}"
    );
    (ctx, guard)
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
    gordian_core::tools_pcb::build_seed_board(board, &ctx).unwrap();
    let out = run_tool("place_board", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(
        out["legal"],
        serde_json::json!(false),
        "should not fit in 3x3: {out}"
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
fn locked_part_rejects_non_axis_aligned_rotation() {
    // A 45° lock must be rejected at the surface (the placer/synth are axis-aligned
    // only) with a clear message — not silently routed to wrong pads then failed at
    // export. 0/90/180/270 are accepted.
    let (ctx, _g) = fixture_ctx();
    let bad = gordian_core::tools_pcb::build_seed_board(
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
        bad["error"]
            .as_str()
            .is_some_and(|e| e.contains("not supported")),
        "45° lock must be rejected: {bad}"
    );
    let ok = gordian_core::tools_pcb::build_seed_board(
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
    assert_eq!(
        ok["ok"],
        serde_json::json!(true),
        "90° lock must be accepted: {ok}"
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
    // Either engine may win (route_auto picks the fewer-failed result); just
    // assert the provenance tag is one of the two honest values.
    assert!(
        matches!(out["router"].as_str(), Some("naive") | Some("detailed")),
        "router must be naive or detailed: {out}"
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
fn render_board_before_place_is_recoverable_error() {
    let (ctx, _guard) = fixture_ctx();
    // Create board but do NOT place.
    let board = serde_json::json!({
        "bounds": { "min_x": 0.0, "max_x": 30.0, "min_y": 0.0, "max_y": 20.0 },
        "parts": [
            { "reference": "R1", "footprint": "Fixtures:R_0603_1608Metric",
              "pad_nets": { "1": "VIN", "2": "GND" } }
        ]
    });
    assert_eq!(
        gordian_core::tools_pcb::build_seed_board(board, &ctx).unwrap()["ok"],
        serde_json::json!(true)
    );

    // No placement yet: both explicit "placed" and auto (no route) must error.
    let out = run_tool("render_board", serde_json::json!({"view": "placed"}), &ctx).unwrap();
    assert!(
        out["error"]
            .as_str()
            .is_some_and(|e| e.contains("place_board")),
        "render placed before place must error: {out}"
    );
    let out = run_tool("render_board", serde_json::json!({}), &ctx).unwrap();
    assert!(
        out["error"]
            .as_str()
            .is_some_and(|e| e.contains("place_board")),
        "render auto (no route) before place must error: {out}"
    );

    // Explicit "routed" view before routed copper is a distinct error.
    let out = run_tool("render_board", serde_json::json!({"view": "routed"}), &ctx).unwrap();
    assert!(
        out["error"]
            .as_str()
            .is_some_and(|e| e.contains("route_board")),
        "render routed before route must error: {out}"
    );
}

#[test]
#[ignore = "live KiCAD IPC: render_board saves/imports the active board session"]
fn render_board_placed_returns_ok_and_png_magic() {
    let (ctx, _g) = placed_board_ctx();
    if skip_unstable_footprint_update(&ctx) {
        return;
    }

    // Place the board.
    let out = run_tool("place_board", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(out["legal"], serde_json::json!(true), "place: {out}");

    // Render the placed view.
    let out = run_tool("render_board", serde_json::json!({"view": "placed"}), &ctx).unwrap();
    assert_eq!(
        out["ok"],
        serde_json::json!(true),
        "render_board placed: {out}"
    );
    assert_eq!(
        out["view"],
        serde_json::json!("placed"),
        "view field: {out}"
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
        out[gordian_core::tools::IMAGE_PATH_KEY].as_str(),
        Some(png_path),
        "IMAGE_PATH_KEY must equal png_path"
    );
}

#[test]
#[ignore = "live KiCAD IPC: render_board saves/imports the active board session"]
fn render_board_routed_returns_ok_and_png_magic() {
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

    // Render the routed view explicitly.
    let out = run_tool("render_board", serde_json::json!({"view": "routed"}), &ctx).unwrap();
    assert_eq!(
        out["ok"],
        serde_json::json!(true),
        "render_board routed: {out}"
    );
    assert_eq!(
        out["view"],
        serde_json::json!("routed"),
        "view field: {out}"
    );

    let png_path = out["png_path"].as_str().expect("png_path present");
    let png_bytes = std::fs::read(png_path).expect("PNG file written");
    assert_eq!(&png_bytes[..8], PNG_MAGIC, "must be a valid PNG");

    // IMAGE_PATH_KEY set.
    assert_eq!(
        out[gordian_core::tools::IMAGE_PATH_KEY].as_str(),
        Some(png_path),
        "IMAGE_PATH_KEY must equal png_path"
    );
}

#[test]
#[ignore = "live KiCAD IPC: render_board saves/imports the active board session"]
fn render_board_default_view_logic() {
    let (ctx, _g) = placed_board_ctx();
    if skip_unstable_footprint_update(&ctx) {
        return;
    }

    // After place but before route: auto should pick "placed".
    run_tool("place_board", serde_json::json!({}), &ctx).unwrap();
    let out = run_tool("render_board", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(
        out["ok"],
        serde_json::json!(true),
        "auto before route: {out}"
    );
    assert_eq!(
        out["view"],
        serde_json::json!("placed"),
        "default before route must be placed: {out}"
    );

    // After route: auto should pick "routed".
    run_tool("route_board", serde_json::json!({}), &ctx).unwrap();
    let out = run_tool("render_board", serde_json::json!({}), &ctx).unwrap();
    assert_eq!(
        out["ok"],
        serde_json::json!(true),
        "auto after route: {out}"
    );
    assert_eq!(
        out["view"],
        serde_json::json!("routed"),
        "default after route must be routed: {out}"
    );
}
