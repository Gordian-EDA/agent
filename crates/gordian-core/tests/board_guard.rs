//! The board guard and subset placement, over a real project.
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

fn pose(parts: &[BoardFootprint], reference: &str) -> (geom::Point2, f64) {
    let part = parts
        .iter()
        .find(|fp| fp.reference == reference)
        .unwrap_or_else(|| panic!("{reference} is on the board"));
    (part.at, part.rotation)
}

const R0805: &str = "Resistor_SMD:R_0805_2012Metric";

fn divider(ctx: &AgentRuntime) {
    tool(
        ctx,
        "place_parts",
        json!({"block": "divider", "parts": [
            {"ref": "R1", "part": "Device:R", "value": "10k", "footprint": R0805,
             "pins": {"1": "VIN", "2": "SENSE"}},
            {"ref": "R2", "part": "Device:R", "value": "10k", "footprint": R0805,
             "pins": {"1": "SENSE", "2": "GND"}},
            {"ref": "R3", "part": "Device:R", "value": "1k", "footprint": R0805,
             "pins": {"1": "SENSE", "2": "GND"}}
        ]}),
    );
    // Room to move: a board packed to its own minimum has no free seat for a
    // subset placement to find.
    tool(
        ctx,
        "sync_board",
        json!({ "bounds": { "min_x": 0.0, "min_y": 0.0, "max_x": 40.0, "max_y": 40.0 } }),
    );
}

/// One project, one live KiCAD session: subset placement, the guard's refusal,
/// and what `check_board` says about a board nothing has laid out yet.
#[test]
fn the_board_guard_and_its_subset_placement() {
    let _kicad = common::KicadLock::acquire();
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    divider(&ctx);

    // ── check_board names what nothing has placed ───────────────────────────
    let checked = run_tool("check_board", json!({}), &ctx).unwrap();
    assert_eq!(
        checked["unplaced"],
        json!(["R1", "R2", "R3"]),
        "a board straight out of sync has laid out nothing: {checked:#}"
    );

    // Routing a board nothing has laid out is refused before any write, rather
    // than reported as a row of failed nets.
    let early = run_tool("route_board", json!({}), &ctx).unwrap();
    assert_eq!(early["code"], json!("board_not_placed"), "{early:#}");
    assert_eq!(early["unplaced"], json!(["R1", "R2", "R3"]), "{early:#}");

    tool(&ctx, "place_board", json!({}));
    let checked = run_tool("check_board", json!({}), &ctx).unwrap();
    assert_eq!(checked["unplaced"], json!([]), "{checked:#}");

    // ── a refused mutator writes nothing ────────────────────────────────────
    let routed = tool(&ctx, "route_board", json!({}));
    let before_text = std::fs::read_to_string(ctx.pcb_path()).unwrap();
    assert!(
        before_text.contains("(segment"),
        "the divider must have copper for the outline to cut through: {routed:#}"
    );
    let before = footprints(&ctx);

    // Shrinking the outline onto routed copper puts that copper outside the
    // board — a violation this edit, and only this edit, is responsible for.
    let refused = run_tool(
        "update_board_outline",
        json!({ "bounds": { "min_x": 0.0, "min_y": 0.0, "max_x": 6.0, "max_y": 6.0 } }),
        &ctx,
    )
    .unwrap();
    assert_eq!(
        refused["code"],
        json!("board_guard_refused"),
        "shrinking the outline over live copper must be refused: {refused:#}"
    );
    assert!(
        !refused["violations"].as_array().unwrap().is_empty(),
        "the refusal must name what it found: {refused:#}"
    );
    assert_eq!(refused["restored"], json!(true), "{refused:#}");
    assert_eq!(
        std::fs::read_to_string(ctx.pcb_path()).unwrap(),
        before_text,
        "a refused mutator must leave the board byte-identical"
    );

    // ── a subset placement moves only what it was asked to ──────────────────
    // With nothing unplaced, bare place_board refuses rather than re-place.
    let whole = run_tool("place_board", json!({}), &ctx).unwrap();
    assert_eq!(whole["code"], json!("board_already_placed"), "{whole:#}");
    assert_eq!(std::fs::read_to_string(ctx.pcb_path()).unwrap(), before_text);

    let placed = tool(&ctx, "place_board", json!({ "refs": ["R3"] }));
    assert_eq!(placed["placed_refs"], json!(["R3"]), "{placed:#}");
    let after = footprints(&ctx);
    for reference in ["R1", "R2"] {
        assert_eq!(
            pose(&after, reference),
            pose(&before, reference),
            "{reference} moved during a subset placement"
        );
    }

    // An unknown reference is named, not silently ignored.
    let unknown = run_tool("place_board", json!({ "refs": ["R9"] }), &ctx).unwrap();
    assert!(
        unknown["error"].as_str().unwrap_or_default().contains("R9"),
        "{unknown:#}"
    );
    ctx.close_kicad_session();
}


/// `move_parts` judges a move by the courtyards KiCAD's DRC checks. A connector
/// whose origin is pin 1 rather than its body centre must not be measured by a
/// box centred on that origin — the phantom half refuses moves that clear.
#[test]
fn move_parts_measures_the_real_courtyards() {
    let _kicad = common::KicadLock::acquire();
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    tool(
        &ctx,
        "place_parts",
        json!({"block": "driver", "parts": [
            {"ref": "R1", "part": "Device:R", "value": "10k", "footprint": R0805,
             "pins": {"1": "VIN", "2": "SENSE"}},
            {"ref": "R2", "part": "Device:R", "value": "10k", "footprint": R0805,
             "pins": {"1": "SENSE", "2": "GND"}},
            {"ref": "C1", "part": "Device:C", "value": "100n",
             "footprint": "Capacitor_SMD:C_0603_1608Metric",
             "pins": {"1": "VIN", "2": "GND"}},
            {"ref": "J1", "part": "Connector_Generic:Conn_01x02", "value": "PWR",
             "footprint": "Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical",
             "pins": {"1": "VIN", "2": "GND"}}
        ]}),
    );
    tool(
        &ctx,
        "sync_board",
        json!({ "bounds": { "min_x": 0.0, "min_y": 0.0, "max_x": 60.0, "max_y": 60.0 } }),
    );
    tool(&ctx, "place_board", json!({}));

    // Park everything in one corner, then move the 0603 to open board well
    // clear of all of it. Nothing is within 5 mm, so nothing may refuse.
    tool(
        &ctx,
        "move_parts",
        json!({"moves": [
            { "reference": "R1", "to": [6.0, 6.0] },
            { "reference": "R2", "to": [6.0, 12.0] },
            { "reference": "J1", "to": [6.0, 20.0] },
        ]}),
    );
    let clear = tool(&ctx, "move_parts", json!({"moves": [
        { "reference": "C1", "to": [40.0, 40.0] }
    ]}));
    assert_eq!(clear["moved"], json!(1), "{clear:#}");

    // A quarter-turned header beside an axial resistor: 5 mm of clear board is
    // 5 mm however the part is turned.
    let turned = tool(&ctx, "move_parts", json!({"moves": [
        { "reference": "J1", "to": [40.0, 25.0], "rotation": 90.0 }
    ]}));
    assert_eq!(turned["moved"], json!(1), "{turned:#}");
    let beside = tool(&ctx, "move_parts", json!({"moves": [
        { "reference": "R1", "to": [40.0, 33.0] }
    ]}));
    assert_eq!(beside["moved"], json!(1), "{beside:#}");

    // And a move that really does collide still refuses — showing its work.
    let refused = run_tool(
        "move_parts",
        json!({"moves": [{ "reference": "R2", "to": [40.0, 40.0] }]}),
        &ctx,
    )
    .unwrap();
    assert_eq!(refused["code"], json!("courtyards_overlap"), "{refused:#}");
    assert_eq!(refused["moved"]["reference"], json!("R2"), "{refused:#}");
    assert!(
        refused["moved"]["courtyard_mm"].as_array().is_some_and(|r| r.len() == 4),
        "the refusal must show both courtyards: {refused:#}"
    );
    ctx.close_kicad_session();
}
