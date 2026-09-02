//! Real KiCad schematic rendering with deterministic visual facts.

use std::path::Path;

use gordian_core::{AgentRuntime, tools};
use kicad::KicadInstallation;
use serde_json::json;

#[test]
fn renders_fixture_with_visual_facts() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    let temp = tempfile::tempdir().expect("temp project");
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../quality/cases/sch-replace-ic/input/design.kicad_sch");
    let schematic = temp.path().join("design.kicad_sch");
    std::fs::copy(source, &schematic).expect("copy fixture schematic");
    let ctx = AgentRuntime::new(env, temp.path().to_path_buf(), schematic).expect("runtime");

    let result = tools::run_tool("render_schematic", json!({}), &ctx).expect("render tool");

    assert_eq!(result["ok"], true);
    let png = result["png_path"].as_str().expect("PNG path");
    assert!(Path::new(png).is_file(), "missing render at {png}");
    let visual = result["visual"].as_object().expect("visual facts object");
    assert!(visual["baseline_revision"].is_null());
    let extent = visual["sheet_extent"].as_array().expect("sheet extent");
    assert_eq!(extent.len(), 4);
    let coordinates: Vec<f64> = extent
        .iter()
        .map(|value| value.as_f64().expect("numeric extent"))
        .collect();
    assert!(coordinates[2] > coordinates[0], "{coordinates:?}");
    assert!(coordinates[3] > coordinates[1], "{coordinates:?}");
    assert!(coordinates.iter().all(|value| value.is_finite()));
    for fact in [
        "body_overlaps",
        "wires_through_bodies",
        "text_collisions",
        "off_grid_pins",
        "dangling_wire_ends",
    ] {
        assert!(visual[fact].is_array(), "visual.{fact} must be an array");
        let introduced = format!("{fact}_introduced");
        assert!(
            visual[&introduced].is_array(),
            "visual.{fact}_introduced must be an array"
        );
    }

    ctx.begin_turn().unwrap();
    let revision = ctx
        .revisions()
        .capture(
            "test_edit",
            "capture visual baseline",
            &[ctx.sch_path().to_path_buf()],
        )
        .unwrap();
    let unchanged = tools::run_tool("render_schematic", json!({}), &ctx).expect("second render");
    let visual = unchanged["visual"].as_object().unwrap();
    assert_eq!(visual["baseline_revision"], json!(revision));
    for fact in [
        "body_overlaps",
        "wires_through_bodies",
        "text_collisions",
        "off_grid_pins",
        "dangling_wire_ends",
    ] {
        assert_eq!(visual[&format!("{fact}_introduced")], json!([]));
    }
}
