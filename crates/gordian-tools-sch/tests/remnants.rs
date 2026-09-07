//! Deletion tools remove complete authored regions and leave every surviving
//! connection explicit.

use std::path::Path;

use geom::Point2;
use gordian_runtime::AgentRuntime;
use sch_doc::{LabelKind, Pose, SchDoc};
use serde_json::{Value, json};

const EMPTY_SHEET: &str = "(kicad_sch\n\
\t(version 20250114)\n\
\t(generator \"eeschema\")\n\
\t(generator_version \"10.0\")\n\
\t(uuid \"4a1c0f2e-0000-4000-8000-0000000000bc\")\n\
\t(paper \"A4\")\n\
\t(lib_symbols)\n\
\t(sheet_instances\n\
\t\t(path \"/\"\n\
\t\t\t(page \"1\")\n\
\t\t)\n\
\t)\n\
)\n";

fn sheet() -> Option<AgentRuntime> {
    let ctx = AgentRuntime::detect_for_test()?;
    std::fs::write(ctx.sch_path(), EMPTY_SHEET).unwrap();
    Some(ctx)
}

fn call(ctx: &AgentRuntime, name: &str, input: Value) -> Value {
    gordian_tools_sch::run(name, input, ctx)
        .unwrap_or_else(|| panic!("`{name}` is not a schematic tool"))
        .unwrap_or_else(|error| panic!("`{name}` failed: {error}"))
}

fn add(ctx: &AgentRuntime, parts: Value) {
    let result = call(ctx, "place_parts", json!({"parts": parts}));
    assert!(result.get("error").is_none(), "fixture failed: {result}");
}

/// Power symbols — the rail glyph a placement draws and the flag `connect` adds on a
/// pin already on its rail — are removable by reference and by UUID.
#[test]
fn power_symbols_are_removable_by_reference_and_uuid() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: KiCad 10 is not installed");
        return;
    };
    add(&ctx, json!([{"lib_id": "Device:R", "ref": "R1", "pins": {"1": "GND", "2": "OUT"}}]));
    let flagged = call(&ctx, "connect", json!({"from": "R1.1", "net": "GND"}));
    assert_eq!(flagged["power_symbol_used"], "power:PWR_FLAG", "{flagged}");
    let doc = SchDoc::read(ctx.sch_path()).unwrap();
    let glyph = doc.symbols().find(|s| s.refdes().starts_with("#PWR")).expect("a GND glyph").refdes().to_string();
    let flags: Vec<String> = doc.symbols().filter(|s| s.refdes().starts_with("#FLG")).map(|s| s.uuid.clone()).collect();
    assert!(!flags.is_empty(), "a flag landed on R1.1");

    // Flags by UUID first: taking the glyph first takes every flag welded to its pin.
    let by_uuid = call(&ctx, "remove_symbols", json!({"refs": flags}));
    assert_eq!(by_uuid["changed"]["removed"]["power"], flags.len(), "{by_uuid}");
    let by_ref = call(&ctx, "remove_symbols", json!({"refs": [glyph]}));
    assert_eq!(by_ref["changed"]["removed"]["power"], 1, "{by_ref}");
    assert_eq!(SchDoc::read(ctx.sch_path()).unwrap().symbols().count(), 1);
}

#[test]
fn remove_symbols_commits_matches_and_reports_missing_references() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: KiCad 10 is not installed");
        return;
    };
    add(&ctx, json!([{"lib_id": "Device:R", "ref": "R1"}]));

    let removed = call(&ctx, "remove_symbols", json!({"refs": ["R1", "J1"]}));

    assert!(removed.get("error").is_none(), "{removed}");
    assert_eq!(removed["changed"]["removed"]["symbols"], 1, "{removed}");
    assert_eq!(removed["changed"]["missing"], json!(["J1"]), "{removed}");
    assert!(
        SchDoc::read(ctx.sch_path())
            .unwrap()
            .symbol_by_ref("R1")
            .is_none()
    );

    let none = call(&ctx, "remove_symbols", json!({"refs": ["J1"]}));
    assert!(none.get("error").is_some(), "{none}");
    assert_eq!(none["missing"], json!(["J1"]), "{none}");
}

#[test]
fn remove_region_lists_blocks_and_uses_a_bbox_fallback() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: KiCad 10 is not installed");
        return;
    };
    add(
        &ctx,
        json!([
            {"lib_id": "Device:R", "ref": "R1"},
            {"lib_id": "Device:R", "ref": "R2"},
        ]),
    );
    let mut doc = SchDoc::read(ctx.sch_path()).unwrap();
    let r1 = doc.symbol_by_ref("R1").unwrap().uuid.clone();
    let r2 = doc.symbol_by_ref("R2").unwrap().uuid.clone();
    doc.set_field(&r1, sch_model::result::AP_BLOCK, "supply")
        .unwrap();
    doc.set_field(&r2, sch_model::result::AP_BLOCK, "load")
        .unwrap();
    let r1_at = doc.symbol(&r1).unwrap().at.point();
    doc.write(ctx.sch_path()).unwrap();

    let missing = call(&ctx, "remove_symbols", json!({"block": "cell_monitor"}));
    assert!(missing.get("error").is_some(), "{missing}");
    assert_eq!(missing["blocks"], json!(["load", "supply"]), "{missing}");

    let fallback = call(
        &ctx,
        "remove_symbols",
        json!({
            "block": "cell_monitor",
            "bbox": [r1_at.x - 1.0, r1_at.y - 1.0, r1_at.x + 1.0, r1_at.y + 1.0]
        }),
    );
    assert!(fallback.get("error").is_none(), "{fallback}");
    assert_eq!(fallback["changed"]["block_not_found"], "cell_monitor");
    assert_eq!(fallback["changed"]["blocks"], json!(["load", "supply"]));
    let doc = SchDoc::read(ctx.sch_path()).unwrap();
    assert!(doc.symbol_by_ref("R1").is_none(), "{fallback}");
    assert!(doc.symbol_by_ref("R2").is_some(), "{fallback}");
}

#[test]
fn delete_labels_by_net_reports_disconnected_pins_and_query_uuids() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: KiCad 10 is not installed");
        return;
    };
    add(
        &ctx,
        json!([
            {"lib_id": "Device:R", "ref": "R1"},
            {"lib_id": "Device:R", "ref": "R2"},
        ]),
    );
    for pin in ["R1.2", "R2.1"] {
        let result = call(&ctx, "connect", json!({"pin": pin, "net": "SIGNAL"}));
        assert!(result.get("error").is_none(), "fixture failed: {result}");
    }
    let label_uuids = SchDoc::read(ctx.sch_path())
        .unwrap()
        .labels()
        .map(|label| label.uuid.clone())
        .collect::<Vec<_>>();
    let lookup = call(&ctx, "read_schematic", json!({"net": "SIGNAL"}));
    let lookup = lookup.as_str().unwrap();
    assert!(
        label_uuids.iter().all(|uuid| lookup.contains(uuid)),
        "{lookup}"
    );

    let removed = call(&ctx, "delete_wires", json!({"labels": true, "net": "SIGNAL"}));
    assert_eq!(removed["changed"]["removed"], 2, "{removed}");
    assert_eq!(
        removed["changed"]["now_unconnected"]
            .as_array()
            .unwrap()
            .len(),
        2,
        "{removed}"
    );
    assert_eq!(SchDoc::read(ctx.sch_path()).unwrap().labels().count(), 0);
}

#[test]
fn remove_region_cuts_a_crossing_wire_and_names_the_outside_end() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: KiCad 10 is not installed");
        return;
    };
    let mut doc = SchDoc::read(ctx.sch_path()).unwrap();
    doc.add_wire(Point2::new(0.0, 0.0), Point2::new(10.16, 0.0));
    doc.add_label(LabelKind::Local, "CROSS", Pose::new(0.0, 0.0, 0.0));
    doc.write(ctx.sch_path()).unwrap();

    let removed = call(
        &ctx,
        "remove_symbols",
        json!({"bbox": [5.08, -2.54, 15.24, 2.54]}),
    );

    assert_eq!(removed["changed"]["removed"]["wires"], 1, "{removed}");
    assert_eq!(
        removed["changed"]["now_loose"],
        json!([{"at": [5.08, 0.0], "net": "CROSS"}]),
        "{removed}"
    );
    let doc = SchDoc::read(ctx.sch_path()).unwrap();
    assert!(
        doc.wires()
            .any(|wire| { wire.points == [Point2::new(0.0, 0.0), Point2::new(5.08, 0.0)] })
    );
}

#[test]
fn removing_a_regulator_block_takes_flags_stubs_and_labels_cleanly() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: KiCad 10 is not installed");
        return;
    };
    add(
        &ctx,
        json!([
            {"lib_id": "Regulator_Linear:L7805", "ref": "U1"},
            {"lib_id": "Device:C", "ref": "C1", "value": "10uF"},
            {"lib_id": "Device:C", "ref": "C2", "value": "100nF"},
        ]),
    );
    let wired = call(
        &ctx,
        "connect",
        json!({"pairs": [
            {"from": "U1.1", "to": "C1.1", "net": "VIN"},
            {"from": "U1.2", "to": "C1.2", "net": "GND"},
            {"from": "U1.2", "to": "C2.2", "net": "GND"},
            {"from": "U1.3", "to": "C2.1", "net": "VOUT"}
        ]}),
    );
    assert!(wired.get("error").is_none(), "fixture failed: {wired}");
    for (pin, net) in [("U1.1", "VIN"), ("U1.2", "GND")] {
        let flag = call(&ctx, "connect", json!({"pin": pin, "net": net}));
        assert!(flag.get("error").is_none(), "fixture failed: {flag}");
    }

    let removed = call(&ctx, "remove_symbols", json!({"refs": ["U1", "C1", "C2"]}));
    assert!(removed.get("error").is_none(), "{removed}");
    assert!(
        removed["changed"]["removed"]["power"].as_u64().unwrap_or(0) >= 2,
        "{removed}"
    );
    let doc = SchDoc::read(ctx.sch_path()).unwrap();
    assert_eq!(doc.wires().count(), 0, "wires survived: {removed}");
    assert_eq!(doc.labels().count(), 0, "labels survived: {removed}");
    let netlist = ctx.env().netlist(ctx.sch_path()).expect("KiCad netlist");
    assert!(netlist.components.is_empty(), "{netlist:?}");
    let erc = ctx.env().erc(ctx.sch_path()).expect("KiCad ERC");
    for kind in ["unconnected_wire_endpoint", "label_dangling"] {
        assert_eq!(
            erc.violations
                .iter()
                .filter(|violation| violation.kind == kind)
                .count(),
            0,
            "{erc:?}"
        );
    }
}

#[test]
fn bluepill_power_region_can_be_replaced_without_new_dangling_remnants() {
    let source = Path::new(
        "/tmp/claude-1000/-home-mimi-agent/859b05e2-ce70-455d-87fa-c4a8f253c33c/scratchpad/bluepill/proj/design.kicad_sch",
    );
    let Some(ctx) = AgentRuntime::detect_for_test().filter(|_| source.is_file()) else {
        eprintln!("SKIP: KiCad 10 or the BluePill fixture is unavailable");
        return;
    };
    std::fs::copy(source, ctx.sch_path()).unwrap();
    if source.with_extension("kicad_pro").is_file() {
        std::fs::copy(
            source.with_extension("kicad_pro"),
            ctx.sch_path().with_extension("kicad_pro"),
        )
        .unwrap();
    }
    let baseline = ctx.env().erc(ctx.sch_path()).expect("baseline KiCad ERC");
    let count = |report: &kicad::ErcReport, kind: &str| {
        report
            .violations
            .iter()
            .filter(|violation| violation.kind == kind)
            .count()
    };
    let removed = call(
        &ctx,
        "remove_symbols",
        json!({"bbox": [410.0, 18.0, 432.0, 34.0]}),
    );
    assert!(removed.get("error").is_none(), "{removed}");
    assert!(
        removed["changed"]["removed"]["symbols"]
            .as_u64()
            .unwrap_or(0)
            >= 1,
        "{removed}"
    );
    let placed = call(
        &ctx,
        "place_parts",
        json!({
            "block": "POWER_REPLACEMENT",
            "parts": [
                {"part": "Regulator_Linear:AMS1117-3.3", "ref": "U2", "pins": {"1": "GND", "2": "+3V3", "3": "VBUS"}},
                {"part": "Device:C", "ref": "C10", "value": "10uF", "pins": {"1": "VBUS", "2": "GND"}},
                {"part": "Device:C", "ref": "C11", "value": "10uF", "pins": {"1": "+3V3", "2": "GND"}}
            ]
        }),
    );
    assert!(placed.get("error").is_none(), "{placed}");
    assert_ne!(placed.get("ok"), Some(&json!(false)), "{placed}");
    ctx.env().netlist(ctx.sch_path()).expect("KiCad netlist");
    let erc = ctx.env().erc(ctx.sch_path()).expect("KiCad ERC");
    assert_eq!(erc.error_count(), 0, "{erc:?}");
    for kind in ["unconnected_wire_endpoint", "label_dangling"] {
        assert!(
            count(&erc, kind) <= count(&baseline, kind),
            "replacement introduced {kind}: {erc:?}"
        );
    }
}
