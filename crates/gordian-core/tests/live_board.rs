//! Offline `sync_board` over a real project: create, edit the schematic, sync again,
//! and check that only what changed changed.
//!
//! Real `kicad-cli`, a real `.kicad_sch` and a real `.kicad_pcb` on disk — no
//! pcbnew, network, or mocks. Skips when no KiCAD installation is available.

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

fn board(ctx: &AgentRuntime) -> Vec<BoardFootprint> {
    let text = std::fs::read_to_string(ctx.pcb_path()).expect("board on disk");
    BoardDoc::parse(text)
        .expect("a board document")
        .footprints()
}

fn part(parts: &[BoardFootprint], reference: &str) -> BoardFootprint {
    parts
        .iter()
        .find(|fp| fp.reference == reference)
        .unwrap_or_else(|| panic!("{reference} is on the board"))
        .clone()
}

fn tracks(ctx: &AgentRuntime) -> usize {
    std::fs::read_to_string(ctx.pcb_path())
        .expect("board on disk")
        .matches("(segment")
        .count()
}

fn refs(value: &Value, key: &str) -> Vec<String> {
    value["delta"][key]
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .map(|entry| {
            entry
                .get("reference")
                .unwrap_or(entry)
                .as_str()
                .unwrap_or_default()
                .to_owned()
        })
        .collect()
}

const R0805: &str = "Resistor_SMD:R_0805_2012Metric";
const R0603: &str = "Resistor_SMD:R_0603_1608Metric";

#[test]
fn sync_board_creates_then_edits_a_board_without_disturbing_it() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };

    tool(
        &ctx,
        "place_parts",
        json!({"block": "divider", "parts": [
            {"ref": "R1", "part": "Device:R", "value": "10k", "footprint": R0805,
             "pins": {"1": "VIN", "2": "SENSE"}},
            {"ref": "R2", "part": "Device:R", "value": "10k", "footprint": R0805,
             "pins": {"1": "SENSE", "2": "GND"}}
        ]}),
    );

    // ── an absent board is created, with every part "added" ─────────────────
    let created = tool(&ctx, "sync_board", json!({}));
    assert_eq!(created["created"], json!(true));
    assert_eq!(refs(&created, "added"), ["R1", "R2"]);
    assert!(created["outline"]["max_x"].as_f64().unwrap() > 0.0);

    // ── a board that already agrees is not rewritten at all ────────────────
    let seeded = std::fs::read_to_string(ctx.pcb_path()).unwrap();
    let again = tool(&ctx, "sync_board", json!({}));
    assert_eq!(again["changed"], json!(false), "{again:#}");
    assert_eq!(std::fs::read_to_string(ctx.pcb_path()).unwrap(), seeded);

    tool(&ctx, "place_board", json!({}));
    tool(&ctx, "route_board", json!({}));
    let placed = board(&ctx);
    let routed_tracks = tracks(&ctx);
    assert!(
        routed_tracks > 0,
        "the divider must have copper to preserve"
    );

    // ── a value-only change moves no geometry and retracts no copper ────────
    tool(
        &ctx,
        "set_fields",
        json!({"ref": "R1", "fields": {"Value": "4.7k"}}),
    );
    let synced = tool(&ctx, "sync_board", json!({}));
    assert_eq!(refs(&synced, "value_changed"), ["R1"]);
    assert!(refs(&synced, "added").is_empty());
    assert!(refs(&synced, "removed").is_empty());
    assert!(refs(&synced, "footprint_changed").is_empty());
    assert_eq!(synced["retracted_tracks"], json!(0));
    assert!(synced["revision"].as_u64().is_some());

    let after_value = board(&ctx);
    assert_eq!(part(&after_value, "R1").value, "4.7k");
    for reference in ["R1", "R2"] {
        let (was, now) = (part(&placed, reference), part(&after_value, reference));
        assert_eq!(
            (now.at, now.rotation),
            (was.at, was.rotation),
            "{reference} moved"
        );
        assert_eq!(now.pad_nets, was.pad_nets, "{reference} changed nets");
        assert_eq!(now.lib_id, was.lib_id, "{reference} changed package");
    }
    assert_eq!(tracks(&ctx), routed_tracks, "copper survives a value edit");

    // ── a footprint swap touches that part alone ────────────────────────────
    tool(
        &ctx,
        "assign_footprints",
        json!({"assignments": [{"reference": "R1", "footprint": R0603}]}),
    );
    let swapped = tool(&ctx, "sync_board", json!({}));
    assert_eq!(refs(&swapped, "footprint_changed"), ["R1"]);
    assert!(refs(&swapped, "pads_retargeted").is_empty(), "{swapped:#}");
    assert!(
        swapped["retracted_tracks"].as_u64().unwrap() > 0,
        "the swapped part's copper must come out: {swapped:#}"
    );

    let after_swap = board(&ctx);
    let r1 = part(&after_swap, "R1");
    assert_eq!(r1.lib_id, R0603);
    assert_eq!(r1.at, part(&placed, "R1").at, "a swap keeps its position");
    assert_eq!(r1.pad_nets, part(&placed, "R1").pad_nets);
    let r2 = part(&after_swap, "R2");
    assert_eq!(r2, part(&placed, "R2"), "R2 was not part of the edit");

    // ── the board is still shippable ────────────────────────────────────────
    tool(&ctx, "route_board", json!({}));
    let checked = tool(&ctx, "check_board", json!({}));
    assert_eq!(checked["drc_clean"], json!(true), "{checked:#}");
    assert_eq!(checked["unconnected_items"], json!(0), "{checked:#}");
}
