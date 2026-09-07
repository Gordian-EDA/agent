//! Offline `sync_board` over a real project: create, edit the schematic, sync again,
//! and check that only what changed changed.
//!
//! Real `kicad-cli`, a real `.kicad_sch` and a real `.kicad_pcb` on disk — no
//! editor process, network, or mocks. Skips when no KiCAD installation is available.

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
        "set_fields",
        json!({"footprints": {"R1": R0603}}),
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

#[test]
fn sync_board_retracts_copper_from_two_retargeted_pads() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };

    let parts = (1..=10)
        .map(|index| {
            json!({
                "ref": format!("R{index}"),
                "part": "Device:R",
                "value": "10k",
                "footprint": R0805,
                "pins": {
                    "1": format!("N{}", index - 1),
                    "2": format!("N{index}"),
                }
            })
        })
        .collect::<Vec<_>>();
    tool(
        &ctx,
        "place_parts",
        json!({ "block": "ten-part-route", "parts": parts }),
    );
    tool(&ctx, "connect", json!({ "pin": "R4.2", "net": "N4" }));
    tool(&ctx, "connect", json!({ "pin": "R5.2", "net": "N5" }));
    tool(
        &ctx,
        "sync_board",
        json!({ "bounds": { "min_x": 0, "min_y": 0, "max_x": 60, "max_y": 60 } }),
    );
    tool(&ctx, "place_board", json!({}));
    let routed = tool(&ctx, "route_board", json!({}));
    assert_eq!(routed["routed"], json!("9/9"), "{routed:#}");
    let before = kicad_board::read_snapshot(&ctx.pcb_path()).unwrap();

    // A half turn in place: R5's two pins exchange positions, so they exchange
    // nets, and no wire moves — the edit a person makes with the R key in KiCAD.
    let source = std::fs::read_to_string(ctx.sch_path()).unwrap();
    let turned = source
        .split("\n\t(symbol\n")
        .map(|block| {
            if !block.contains("\"Reference\" \"R5\"") {
                return block.to_string();
            }
            let at = block.find("(at ").expect("a symbol has a pose");
            let end = block[at..].find(')').expect("a pose closes") + at;
            let fields: Vec<&str> = block[at + 4..end].split_whitespace().collect();
            let rot: f64 = fields[2].parse().unwrap();
            format!(
                "{}(at {} {} {}){}",
                &block[..at],
                fields[0],
                fields[1],
                (rot + 180.0).rem_euclid(360.0),
                &block[end + 1..]
            )
        })
        .collect::<Vec<_>>()
        .join("\n\t(symbol\n");
    assert_ne!(turned, source, "R5 must be on the sheet");
    std::fs::write(ctx.sch_path(), turned).unwrap();
    let synced = tool(&ctx, "sync_board", json!({}));

    assert_eq!(
        synced["delta"]["pads_retargeted"],
        json!([
            { "pad": "R5.1", "from": "/N4", "to": "/N5" },
            { "pad": "R5.2", "from": "/N5", "to": "/N4" },
        ]),
        "{synced:#}"
    );
    assert_eq!(synced["copper_retracted"].as_array().unwrap().len(), 1);
    let retracted = &synced["copper_retracted"][0];
    assert_eq!(
        (retracted["net_a"].as_str(), retracted["net_b"].as_str()),
        (Some("/N4"), Some("/N5"))
    );
    assert!(
        retracted["segments"]
            .as_u64()
            .is_some_and(|count| count >= 2)
    );
    assert!(
        retracted["refs"]
            .as_array()
            .is_some_and(|refs| refs.contains(&json!("R5.1")) && refs.contains(&json!("R5.2"))),
        "{synced:#}"
    );
    let open = synced["now_open"].as_array().unwrap();
    assert!(
        ["/N4", "/N5"].iter().all(|net| open
            .iter()
            .any(|entry| { entry["net"] == json!(net) && entry["status"] != json!("routed") })),
        "{synced:#}"
    );

    let after = kicad_board::read_snapshot(&ctx.pcb_path()).unwrap();
    assert!(
        after
            .copper
            .traces
            .iter()
            .all(|trace| !matches!(trace.connection.as_str(), "/N4" | "/N5")),
        "the old components touching the swapped pads must be gone"
    );
    assert!(
        after.copper.traces.len() < before.copper.traces.len()
            && after
                .copper
                .traces
                .iter()
                .any(|trace| !matches!(trace.connection.as_str(), "/N4" | "/N5")),
        "unrelated routed copper must remain"
    );
    let checked = run_tool("check_board", json!({}), &ctx).unwrap();
    assert_eq!(checked["drc"]["copper_violations"], json!(0), "{checked:#}");
    assert_eq!(checked["unconnected_items"], json!(2), "{checked:#}");
}

#[test]
fn incomplete_sync_and_auto_edge_placement_preserve_a_reported_partial_board() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };

    let placed = tool(
        &ctx,
        "place_parts",
        json!({"block": "partial", "parts": [
            {"ref": "J1", "part": "Connector_Generic:Conn_01x02", "value": "INPUT",
             "footprint": "Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical",
             "pins": {"1": "SIG", "2": "GND"}},
            {"ref": "R1", "part": "Device:R", "value": "10k",
             "footprint": "Missing:PackageA", "pins": {"1": "SIG", "2": "MID"}},
            {"ref": "R2", "part": "Device:R", "value": "10k",
             "footprint": "Missing:PackageB", "pins": {"1": "MID", "2": "GND"}}
        ]}),
    );
    assert_eq!(placed["footprints_unresolved"].as_array().unwrap().len(), 2);

    let synced = tool(
        &ctx,
        "sync_board",
        json!({"bounds": "auto", "rules": {"layer_count": 2, "pours": "GND"}}),
    );
    assert_eq!(synced["missing_footprints"], json!(["R1", "R2"]));
    assert_eq!(synced["staged_missing_footprint"], json!(["R1", "R2"]));
    assert_eq!(
        synced["design_rules"]["pours"],
        json!([{"net": "GND", "layer": "bottom", "connect": "thermal"}])
    );

    let placed = tool(
        &ctx,
        "place_board",
        json!({"refs": ["J1", "R1", "R2"], "intent": {"edge": {"J1": "left"}}}),
    );
    assert_eq!(placed["placed_refs"], json!(["J1"]), "{placed:#}");
    assert_eq!(
        placed["unplaced"].as_array().unwrap().len(),
        2,
        "{placed:#}"
    );
    assert!(placed["outline_refit"].is_object(), "{placed:#}");
    assert!(
        placed["parts_courtyard_area_mm2"].as_f64().unwrap()
            < synced["parts_courtyard_area_mm2"].as_f64().unwrap(),
        "staged placeholder extents must not size the placed outline: {placed:#}"
    );

    let checked = run_tool("check_board", json!({}), &ctx).unwrap();
    assert_eq!(checked["staged_count"], json!(2), "{checked:#}");
    assert!(checked["outline"]["min"].is_array(), "{checked:#}");
    assert!(checked["outline"]["max"].is_array(), "{checked:#}");
    assert!(checked["outline"]["size_mm"].is_array(), "{checked:#}");
    for staged in checked["staged"].as_array().unwrap() {
        assert_eq!(staged["staged_reason"], json!("missing_footprint"));
        assert!(staged["extent"]["min"].is_array(), "{staged:#}");
        assert!(staged["extent"]["max"].is_array(), "{staged:#}");
    }
}
