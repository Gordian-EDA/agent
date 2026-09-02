//! `place_board{bbox}` and `route_board{bbox}` over a real project: a board
//! window must move only what is inside it and rip only the copper that reaches
//! it, leaving everything else byte-identical.
//!
//! Real `kicad-cli`, a real `.kicad_sch` and a real `.kicad_pcb` on disk — no
//! network, no mocks. Skips when no KiCAD is installed.

mod common;

use gordian_core::AgentRuntime;
use gordian_core::tools::run_tool;
use kicad_board::{BoardDoc, BoardFootprint};
use serde_json::{Value, json};

fn tool(ctx: &AgentRuntime, name: &str, input: Value) -> Value {
    let out = run_tool(name, input, ctx).unwrap_or_else(|e| panic!("{name}: {e}"));
    assert!(
        out.get("error").is_none() && out.get("ok").and_then(Value::as_bool) != Some(false),
        "{name} failed: {out:#}"
    );
    out
}

fn footprints(ctx: &AgentRuntime) -> Vec<BoardFootprint> {
    let text = std::fs::read_to_string(ctx.pcb_path()).expect("board on disk");
    BoardDoc::parse(text)
        .expect("a board document")
        .footprints()
}

fn pose(parts: &[BoardFootprint], reference: &str) -> (f64, f64, f64) {
    let fp = parts
        .iter()
        .find(|fp| fp.reference == reference)
        .unwrap_or_else(|| panic!("{reference} is on the board"));
    (fp.at.x, fp.at.y, fp.rotation)
}

const R0805: &str = "Resistor_SMD:R_0805_2012Metric";

/// Three resistors in a chain, so a window can take the middle one.
fn seed(ctx: &AgentRuntime) {
    tool(
        ctx,
        "place_parts",
        json!({"block": "chain", "parts": [
            {"ref": "R1", "part": "Device:R", "value": "10k", "footprint": R0805,
             "pins": {"1": "VIN", "2": "A"}},
            {"ref": "R2", "part": "Device:R", "value": "10k", "footprint": R0805,
             "pins": {"1": "A", "2": "B"}},
            {"ref": "R3", "part": "Device:R", "value": "10k", "footprint": R0805,
             "pins": {"1": "B", "2": "GND"}}
        ]}),
    );
    tool(ctx, "sync_board", json!({}));
    tool(ctx, "place_board", json!({}));
    tool(ctx, "route_board", json!({}));
}

#[test]
fn a_window_places_only_what_is_inside_it() {
    let _kicad = common::KicadLock::acquire();
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    seed(&ctx);

    let before = footprints(&ctx);
    let (x, y, _) = pose(&before, "R2");
    // A window around R2 alone. R2's courtyard centre is inside; the others' are not.
    let placed = tool(
        &ctx,
        "place_board",
        json!({ "bbox": { "min_x": x - 1.0, "min_y": y - 1.0, "max_x": x + 1.0, "max_y": y + 1.0 } }),
    );

    assert_eq!(placed["placed_refs"], json!(["R2"]), "{placed:#}");
    assert_eq!(placed["bbox"]["min_x"], json!(x - 1.0));
    assert_eq!(
        placed["still_unplaced"],
        Value::Null,
        "every part already has a pose"
    );

    let after = footprints(&ctx);
    for reference in ["R1", "R3"] {
        assert_eq!(
            pose(&before, reference),
            pose(&after, reference),
            "{reference} was outside the window and must not have moved"
        );
    }
}

#[test]
fn a_window_that_holds_no_footprint_is_refused_before_anything_is_written() {
    let _kicad = common::KicadLock::acquire();
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    seed(&ctx);

    let before = std::fs::read_to_string(ctx.pcb_path()).unwrap();
    let out = run_tool(
        "place_board",
        json!({ "bbox": { "min_x": -50.0, "min_y": -50.0, "max_x": -40.0, "max_y": -40.0 } }),
        &ctx,
    )
    .unwrap();

    let error = out["error"].as_str().unwrap_or_default();
    assert!(error.contains("courtyard centre"), "{out:#}");
    assert_eq!(
        std::fs::read_to_string(ctx.pcb_path()).unwrap(),
        before,
        "a refusal writes nothing"
    );
}

#[test]
fn a_window_routes_the_nets_that_reach_it_and_keeps_the_rest() {
    let _kicad = common::KicadLock::acquire();
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    seed(&ctx);

    let before = footprints(&ctx);
    let (x, y, _) = pose(&before, "R1");
    let routed = tool(
        &ctx,
        "route_board",
        json!({ "bbox": { "min_x": x - 3.0, "min_y": y - 3.0, "max_x": x + 3.0, "max_y": y + 3.0 } }),
    );

    let scope: Vec<&str> = routed["scope"]
        .as_array()
        .expect("a bbox call names its nets")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        scope.contains(&"VIN"),
        "R1 pad 1 is in the window: {routed:#}"
    );
    assert!(
        !scope.contains(&"GND"),
        "GND is R3's far pad and never reaches the window: {routed:#}"
    );
    assert!(
        routed["kept_existing_copper"]["traces"].as_u64().unwrap() > 0,
        "the copper outside the window is kept, not re-made: {routed:#}"
    );

    // A net left alone is reported out of scope rather than silently missing.
    let out_of_scope: Vec<&str> = routed["unrouted"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter(|item| item["in_scope"] == json!(false))
                .filter_map(|item| item["net"].as_str())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        out_of_scope.is_empty(),
        "every out-of-scope net kept its copper: {out_of_scope:?}"
    );

    let checked = tool(&ctx, "check_board", json!({}));
    assert_eq!(checked["unconnected_items"], json!(0), "{checked:#}");
}

#[test]
fn a_box_and_a_list_together_are_refused() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    for (name, both) in [
        (
            "place_board",
            json!({"refs": ["R1"], "bbox": {"min_x": 0.0, "min_y": 0.0, "max_x": 1.0, "max_y": 1.0}}),
        ),
        (
            "route_board",
            json!({"nets": ["VIN"], "bbox": {"min_x": 0.0, "min_y": 0.0, "max_x": 1.0, "max_y": 1.0}}),
        ),
    ] {
        let out = run_tool(name, both, &ctx).unwrap();
        assert!(
            out["error"]
                .as_str()
                .unwrap_or_default()
                .contains("not both"),
            "{name}: {out:#}"
        );
    }
}
