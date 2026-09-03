//! The incremental PCB loop over a real 27-part project.
//!
//! The board is built the way a human engineer builds one: sync, look at what
//! is staged, place the parts whose position is decided and lock them, route a
//! few nets on a board that is only half laid out, read the progress, then
//! place and route the rest. Every step here is legal partial state; nothing
//! refuses because the board is incomplete.
//!
//! Real `kicad-cli`, a real `.kicad_sch` and a real `.kicad_pcb` on disk.
//! Skips when no KiCAD installation is available.

use gordian_core::AgentRuntime;
use gordian_core::tools::run_tool;
use serde_json::{Value, json};

fn tool(ctx: &AgentRuntime, name: &str, input: Value) -> Value {
    let out = run_tool(name, input, ctx).unwrap_or_else(|e| panic!("{name}: {e}"));
    assert!(
        out.get("error").is_none() && out.get("ok").and_then(Value::as_bool) != Some(false),
        "{name} failed: {out:#}"
    );
    out
}

const R0805: &str = "Resistor_SMD:R_0805_2012Metric";
const C0603: &str = "Capacitor_SMD:C_0603_1608Metric";
const HEADER: &str = "Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical";

/// The references a result reports as staged, in order.
fn staged_refs(result: &Value) -> Vec<String> {
    result["staged"]
        .as_array()
        .unwrap_or_else(|| panic!("a staged list: {result:#}"))
        .iter()
        .map(|part| {
            part.get("ref")
                .or(Some(part))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        })
        .collect()
}

/// One `place_parts` block: three resistors in a chain off the rail. A chain is
/// the shape the schematic typesetter lays out reliably, and this test is about
/// the BOARD loop, not about stressing the schematic engine.
fn chain_block(block: &str, index: usize) -> Value {
    json!({ "block": block, "parts": [
        {"ref": format!("R{index}"), "part": "Device:R", "value": "10k", "footprint": R0805,
         "pins": {"1": "VRAIL", "2": format!("N{index}A")}},
        {"ref": format!("R{}", index + 1), "part": "Device:R", "value": "10k",
         "footprint": R0805,
         "pins": {"1": format!("N{index}A"), "2": format!("N{index}B")}},
        {"ref": format!("C{index}"), "part": "Device:C", "value": "100n", "footprint": C0603,
         "pins": {"1": format!("N{index}B"), "2": "GND"}}
    ]})
}

/// 27 parts: a power entry, and eight three-part chains hanging off the rail.
fn design(ctx: &AgentRuntime) {
    tool(
        ctx,
        "place_parts",
        json!({"block": "power", "parts": [
            {"ref": "J1", "part": "Connector_Generic:Conn_01x02", "value": "PWR",
             "footprint": HEADER, "pins": {"1": "VIN", "2": "GND"}},
            {"ref": "R99", "part": "Device:R", "value": "0R", "footprint": R0805,
             "pins": {"1": "VIN", "2": "VRAIL"}},
            {"ref": "C99", "part": "Device:C", "value": "10u", "footprint": C0603,
             "pins": {"1": "VRAIL", "2": "GND"}}
        ]}),
    );
    for chain in 0..8 {
        let index = 1 + chain * 3;
        tool(
            ctx,
            "place_parts",
            chain_block(&format!("chain{chain}"), index),
        );
    }
}

#[test]
fn the_board_is_built_incrementally_through_legal_partial_states() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    design(&ctx);

    // ── sync: the board exists, and everything on it is staged ──────────────
    tool(
        &ctx,
        "sync_board",
        json!({ "bounds": { "min_x": 0.0, "min_y": 0.0, "max_x": 90.0, "max_y": 90.0 } }),
    );
    let board = tool(&ctx, "get_board", json!({}));
    let summary = &board["summary"];
    assert_eq!(summary["part_count"], json!(27), "{summary:#}");
    assert_eq!(staged_refs(summary).len(), 27, "{summary:#}");
    assert_eq!(summary["placed"], json!([]), "{summary:#}");
    assert_eq!(summary["fully_placed"], json!(false), "{summary:#}");
    for part in summary["staged"].as_array().unwrap() {
        assert_eq!(part["staged_reason"], json!("unplaced"), "{part:#}");
    }

    // ── check_board: staged is progress, never a violation ──────────────────
    let checked = tool(&ctx, "check_board", json!({}));
    let nets = checked["total_connection_count"].as_u64().unwrap();
    assert!(nets >= 14, "{checked:#}");
    assert_eq!(checked["staged_count"], json!(27), "{checked:#}");
    assert_eq!(checked["routed"], json!(format!("0/{nets}")), "{checked:#}");
    assert_eq!(
        checked["blocking_findings"],
        json!(0),
        "27 parts sitting in the staging row are not 27 DRC failures: {checked:#}"
    );

    // ── place the parts whose position is decided, by intent ────────────────
    let placed = tool(
        &ctx,
        "place_board",
        json!({
            "refs": ["J1", "R99", "C99"],
            "intent": { "edge": { "J1": "left" } }
        }),
    );
    let mut placed_refs: Vec<&str> = placed["placed_refs"]
        .as_array()
        .unwrap_or_else(|| panic!("{placed:#}"))
        .iter()
        .map(|reference| reference.as_str().unwrap())
        .collect();
    placed_refs.sort_unstable();
    assert_eq!(placed_refs, ["C99", "J1", "R99"], "{placed:#}");
    assert_eq!(
        placed["mechanically_locked"],
        json!(["J1"]),
        "an edge-intent connector is a physical fixing and locks itself: {placed:#}"
    );
    assert_eq!(placed["still_staged"].as_array().unwrap().len(), 24);

    // ── locks: what is locked never moves, and says who locked it ───────────
    tool(
        &ctx,
        "lock_parts",
        json!({ "refs": ["R99"], "reason": "agent" }),
    );
    let board = tool(&ctx, "get_board", json!({}));
    let locked = board["summary"]["locked"].as_array().unwrap();
    assert_eq!(
        locked,
        &vec![
            json!({ "ref": "J1", "locked_reason": "mechanical" }),
            json!({ "ref": "R99", "locked_reason": "agent" }),
        ],
        "{board:#}"
    );
    let refused = run_tool(
        "move_parts",
        json!({"moves": [{"reference": "J1", "to": [45.0, 45.0]}]}),
        &ctx,
    )
    .unwrap();
    assert_eq!(refused["code"], json!("parts_locked"), "{refused:#}");
    assert!(
        refused["error"].as_str().unwrap().contains("mechanical"),
        "the refusal says who locked it: {refused:#}"
    );
    let refused = run_tool("place_board", json!({ "refs": ["J1"] }), &ctx).unwrap();
    assert_eq!(refused["placement_applied"], json!(false), "{refused:#}");
    assert_eq!(
        refused["locked"],
        json!([{ "ref": "J1", "locked_reason": "mechanical" }]),
        "{refused:#}"
    );

    // ── route a few nets on a board that is only half placed ────────────────
    // Only VIN joins two placed parts; every other net reaches the staging row.
    let routed = tool(&ctx, "route_board", json!({ "nets": ["VIN"] }));
    let mut routed_so_far = routed["routed_connection_count"].as_u64().unwrap();
    let entries = routed["ratsnest"].as_array().unwrap();
    assert_eq!(
        entries.len() as u64,
        nets,
        "the ratsnest covers every net, not just the routed one: {routed:#}"
    );
    let vin = entries
        .iter()
        .find(|entry| entry["net"] == json!("VIN"))
        .unwrap_or_else(|| panic!("{routed:#}"));
    assert_eq!(vin["status"], json!("routed"), "{vin:#}");
    for endpoint in ["from", "to"] {
        for field in ["ref", "pad", "x", "y", "layer"] {
            assert!(
                !vin[endpoint][field].is_null(),
                "every ratsnest endpoint names {field}: {vin:#}"
            );
        }
    }
    // A net that reaches a part nobody has placed is open, and its way out is
    // to place that part — not to move copper that does not exist yet.
    let staged_now: Vec<String> =
        staged_refs(&run_tool("get_board", json!({}), &ctx).unwrap()["summary"]);
    let reaches_staging = |entry: &Value| {
        ["from", "to"].iter().any(|end| {
            entry[end]["ref"]
                .as_str()
                .is_some_and(|reference| staged_now.iter().any(|staged| staged == reference))
        })
    };
    let open: Vec<&Value> = entries
        .iter()
        .filter(|entry| entry["status"] == json!("open") && reaches_staging(entry))
        .collect();
    assert!(!open.is_empty(), "{routed:#}");
    for entry in &open {
        assert!(
            entry["escapes"][0]
                .as_str()
                .unwrap()
                .starts_with("place_board"),
            "an open net that reaches the staging row names the call that frees it: {entry:#}"
        );
    }

    // ── progress on the partial board ───────────────────────────────────────
    // A half-routed board still has unrouted pairs; that is honest work left,
    // and it is reported as progress. What must NOT appear is a finding blamed
    // on a part still in the staging row.
    let checked = run_tool("check_board", json!({}), &ctx).unwrap();
    assert_eq!(checked["staged_count"], json!(24), "{checked:#}");
    assert_eq!(checked["routed"], json!(format!("1/{nets}")), "{checked:#}");
    for finding in checked["findings"].as_array().unwrap() {
        if finding["staged"] == json!(false) {
            for reference in finding["refs"].as_array().unwrap() {
                assert!(
                    !staged_now
                        .iter()
                        .any(|staged| staged == reference.as_str().unwrap()),
                    "a staged part is never blamed for a DRC finding: {finding:#}"
                );
            }
        }
    }
    assert!(
        checked["next"].as_str().unwrap().contains("staged"),
        "{checked:#}"
    );

    // ── place the rest; the locked parts do not move ────────────────────────
    let before = std::fs::read_to_string(ctx.pcb_path()).unwrap();
    let j1 = kicad_board::read_snapshot(&ctx.pcb_path())
        .unwrap()
        .imported
        .parts
        .into_iter()
        .find(|part| part.reference == "J1")
        .unwrap();
    let placed = tool(&ctx, "place_board", json!({}));
    assert_eq!(placed["still_staged"], Value::Null, "{placed:#}");
    assert_ne!(before, std::fs::read_to_string(ctx.pcb_path()).unwrap());
    let after = kicad_board::read_snapshot(&ctx.pcb_path())
        .unwrap()
        .imported
        .parts
        .into_iter()
        .find(|part| part.reference == "J1")
        .unwrap();
    assert_eq!((after.at, after.rotation), (j1.at, j1.rotation));
    assert!(after.locked, "a lock survives a whole-board placement");

    // Nothing is staged any more, so a bare place_board is a no-op that says so.
    let again = tool(&ctx, "place_board", json!({}));
    assert_eq!(again["placement_applied"], json!(false), "{again:#}");
    assert_eq!(again["staged"], json!([]), "{again:#}");
    assert_eq!(again["placed"].as_array().unwrap().len(), 27, "{again:#}");

    // ── route the rest, then read the progress ──────────────────────────────
    let routable = tool(&ctx, "get_board", json!({}))["summary"]["nets"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|net| net["pins"].as_u64().unwrap_or_default() >= 2)
        .filter_map(|net| net["name"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    let mut routed = Value::Null;
    for batch in routable.chunks(6) {
        routed = tool(&ctx, "route_board", json!({ "nets": batch }));
        let progress = routed["routed_connection_count"].as_u64().unwrap();
        assert!(
            progress >= routed_so_far,
            "routed progress must be monotone: {routed_so_far} -> {progress}: {routed:#}"
        );
        routed_so_far = progress;
    }
    assert!(
        routed_so_far > 1,
        "routing in batches makes progress: {routed:#}"
    );
    for entry in routed["ratsnest"].as_array().unwrap() {
        let status = entry["status"].as_str().unwrap();
        assert!(["open", "routed", "blocked"].contains(&status), "{entry:#}");
        if status == "blocked" {
            for field in ["kind", "owner_ref", "at", "gap_mm", "need_mm"] {
                assert!(
                    entry["blocker"].get(field).is_some(),
                    "a blocked entry carries its blocker's {field}: {entry:#}"
                );
            }
        }
    }

    let checked = run_tool("check_board", json!({}), &ctx).unwrap();
    assert_eq!(checked["staged"], json!([]), "{checked:#}");
    assert_eq!(checked["staged_count"], json!(0), "{checked:#}");
    assert_eq!(
        checked["total_connection_count"],
        json!(nets),
        "{checked:#}"
    );
    assert_eq!(
        checked["routed"],
        json!(format!("{}/{nets}", checked["routed_connection_count"])),
        "{checked:#}"
    );
    assert!(
        checked["routed_connection_count"].as_u64().unwrap()
            + checked["blocked"].as_array().unwrap().len() as u64
            <= nets,
        "routed and blocked are disjoint parts of the same {nets} nets: {checked:#}"
    );
    assert!(
        checked["routed_connection_count"].as_u64().unwrap() > 1,
        "the second route made progress on top of the first: {checked:#}"
    );
    let open = checked["total_connection_count"].as_u64().unwrap()
        - checked["routed_connection_count"].as_u64().unwrap();
    let blocked = checked["blocked"].as_array().unwrap();
    assert!(
        checked["drc_clean"] == json!(true)
            || (blocked.len() as u64 == open
                && blocked
                    .iter()
                    .all(|entry| entry.get("blocker").is_some_and(Value::is_object))),
        "the incremental route must end at DRC 0 or with every open net carrying a blocker: {checked:#}"
    );

    let copper = tool(&ctx, "get_board", json!({ "include_copper": true }));
    let routed_net = copper["board"]["copper"]["tracks"]
        .as_array()
        .and_then(|tracks| tracks.first())
        .and_then(|track| track["net"].as_str())
        .expect("a routed track to delete")
        .to_owned();
    let deleted = tool(&ctx, "delete_copper", json!({ "net": routed_net.clone() }));
    assert!(deleted["deleted"].as_u64().is_some_and(|count| count > 0));
    assert!(
        deleted["now_open"]
            .as_array()
            .is_some_and(|nets| nets.iter().any(|net| net == &routed_net)),
        "deleting copper reports the net it intentionally opened: {deleted:#}"
    );
    tool(&ctx, "route_board", json!({ "nets": [routed_net] }));
}

/// Sixty LED-array passives plus their connector are placed once, then routed
/// in local batches whose saved-board progress can only increase.
#[test]
fn the_sixty_part_led_array_routes_in_monotone_batches() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let catalog = ctx.footprint_catalog().expect("footprint catalog");
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../pcb-workflow/examples/pcb_circuits/led-array-60.json");
    let board = pcb_workflow::corpus::load_corpus_board(&source, catalog).expect("LED corpus");
    assert_eq!(
        board.problem.parts.len(),
        61,
        "60 passives plus one connector"
    );
    let placed = pcb_engine::place_tuned(&board.problem, &board.hints);
    assert!(placed.legal, "the array placement must be legal");
    let text = pcb_workflow::corpus::routed_board_text(
        &board,
        &placed.placements,
        &pcb_model::RouteSolution::default(),
        catalog,
    )
    .expect("saved board");
    std::fs::write(ctx.pcb_path(), text).expect("write saved board");

    let nets = tool(&ctx, "get_board", json!({}))["summary"]["nets"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|net| net["pins"].as_u64().unwrap_or_default() >= 2)
        .filter_map(|net| net["name"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    assert_eq!(nets.len(), 32);
    let mut routed = 0_u64;
    let mut last = Value::Null;
    for batch in nets.chunks(8) {
        last = tool(&ctx, "route_board", json!({ "nets": batch }));
        let progress = last["routed_connection_count"].as_u64().unwrap();
        assert!(
            progress >= routed,
            "routed progress decreased from {routed} to {progress}: {last:#}"
        );
        routed = progress;
    }
    assert!(routed > 0, "the array route made no progress: {last:#}");

    let checked = tool(&ctx, "check_board", json!({}));
    let drc_clean = checked["blocking_findings"] == json!(0);
    let blocked_are_actionable = checked["blocked"].as_array().is_some_and(|blocked| {
        !blocked.is_empty()
            && blocked
                .iter()
                .all(|entry| entry.get("blocker").is_some_and(Value::is_object))
    });
    assert!(
        drc_clean || blocked_are_actionable,
        "the final partial state needs DRC 0 or geometric blockers: {checked:#}"
    );
}

/// A live schematic edit makes the board net table stale until sync imports the
/// new nets; ERC findings and placement intent remain reportable parallel work.
#[test]
fn stale_nets_request_sync_and_existing_board_intent_runs_both_halves() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    tool(
        &ctx,
        "place_parts",
        json!({ "parts": [
            {"ref":"R1", "part":"Device:R", "footprint":R0805,
             "pins":{"1":"VIN", "2":"MID"}},
            {"ref":"R2", "part":"Device:R", "footprint":R0805,
             "pins":{"1":"MID", "2":"GND"}}
        ]}),
    );
    tool(&ctx, "sync_board", json!({}));
    run_tool(
        "add_symbols",
        json!({ "parts": [{ "ref": "R99", "lib_id": "Device:R" }] }),
        &ctx,
    )
    .expect("add an intentionally incomplete symbol");
    tool(&ctx, "label", json!({ "pin": "R99.1", "net": "UNSYNCED" }));

    let stale_get = run_tool("get_board", json!({ "net": "UNSYNCED" }), &ctx).unwrap();
    let imported_net = "UNSYNCED".to_owned();
    for stale in [
        stale_get,
        run_tool("route_board", json!({ "nets": ["UNSYNCED"] }), &ctx).unwrap(),
    ] {
        assert_eq!(stale["code"], "board_net_table_stale", "{stale:#}");
        assert!(
            stale["error"]
                .as_str()
                .is_some_and(|error| error.contains("run sync_board first")),
            "{stale:#}"
        );
        assert!(
            !stale["schematic_changed"]["nets"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    let synced = run_tool(
        "sync_board",
        json!({ "intent": {
            "keep_near": [["R1", "R2"]],
            "edge": { "J404": "left" }
        } }),
        &ctx,
    )
    .unwrap();
    assert!(synced.get("error").is_none(), "{synced:#}");
    assert_eq!(synced["normalized_edges"]["J404"], json!("left"));
    assert!(synced["sync"].is_object(), "{synced:#}");
    assert!(synced["placement"].is_object(), "{synced:#}");
    assert!(
        synced["placement"]["reference_status"]
            .as_array()
            .is_some_and(|statuses| statuses.iter().any(|status| {
                status["reference"] == "J404" && status["status"] == "absent_from_schematic"
            })),
        "an absent intent ref is classified while sync proceeds: {synced:#}"
    );
    assert!(
        synced["schematic_erc"]["errors"]
            .as_u64()
            .is_some_and(|errors| errors > 0),
        "ERC errors are reported without blocking sync: {synced:#}"
    );
    let fresh = tool(&ctx, "get_board", json!({}));
    assert_ne!(fresh["sync_required"], json!(true), "{fresh:#}");
    assert!(
        std::fs::read_to_string(ctx.pcb_path())
            .unwrap()
            .contains(&imported_net),
        "sync must import the new net into the KiCad board table"
    );
}

/// `reserve_refs` records the claim in the project, so a second process that
/// knows nothing about the first still hands out different references.
#[test]
fn reserved_references_are_recorded_in_the_project_not_in_this_process() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let first = tool(&ctx, "reserve_refs", json!({ "prefix": "R", "count": 3 }));
    assert_eq!(first["refs"], json!(["R1", "R2", "R3"]), "{first:#}");

    // A fresh runtime over the same project directory is what a restart looks
    // like: it reads the claim off disk rather than remembering it.
    let restarted = AgentRuntime::for_project(ctx.env().clone(), ctx.project_dir().to_path_buf())
        .expect("a second runtime over the same project");
    let second = run_tool(
        "reserve_refs",
        json!({ "prefix": "R", "count": 2 }),
        &restarted,
    )
    .unwrap();
    assert_eq!(second["refs"], json!(["R4", "R5"]), "{second:#}");
}

/// A reservation is a promise, so the allocators that mint designators for a
/// caller who did not name one have to step over it.
#[test]
fn a_minted_designator_never_takes_a_reserved_reference() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let reserved = tool(&ctx, "reserve_refs", json!({ "prefix": "R", "count": 2 }));
    assert_eq!(reserved["refs"], json!(["R1", "R2"]), "{reserved:#}");

    // place_parts' own allocator: `ref` omitted, so it mints.
    let placed = tool(
        &ctx,
        "place_parts",
        json!({ "parts": [
            { "part": "Device:R", "pins": { "1": "VIN", "2": "MID" } },
            { "part": "Device:R", "pins": { "1": "MID", "2": "GND" } }
        ] }),
    );
    assert!(
        placed["text"]
            .as_str()
            .is_some_and(|text| text.starts_with("PLACED  R3 R4")),
        "{placed:#}"
    );

    // add_symbols' allocator mints from the same store.
    let added = tool(
        &ctx,
        "add_symbols",
        json!({ "parts": [{ "lib_id": "Device:R" }] }),
    );
    let refs: Vec<&str> = added["changed"]["placed"]
        .as_array()
        .unwrap_or_else(|| panic!("placed parts: {added:#}"))
        .iter()
        .filter_map(|part| part["ref"].as_str())
        .collect();
    assert_eq!(refs, ["R5"], "{added:#}");
}

/// A symbol on the bench is on its nets but has no layout, so a board built from
/// it would silently omit real circuitry. That is the one refusal an unfinished
/// design earns, and it names the way out.
#[test]
fn the_board_tools_refuse_while_the_schematic_bench_is_not_empty() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    tool(
        &ctx,
        "add_parts",
        json!({"parts": [
            {"ref": "R1", "part": "Device:R", "pins": {"1": "VIN", "2": "MID"}},
            {"ref": "R2", "part": "Device:R", "pins": {"1": "MID", "2": "GND"}}
        ]}),
    );

    for name in ["sync_board", "export_fab"] {
        let refused = run_tool(name, json!({}), &ctx).unwrap();
        assert_eq!(
            refused["code"],
            json!("bench_not_empty"),
            "{name}: {refused:#}"
        );
        assert_eq!(refused["bench"], json!(2), "{name}: {refused:#}");
        assert_eq!(
            refused["bench_refs"],
            json!(["R1", "R2"]),
            "{name}: {refused:#}"
        );
    }

    tool(&ctx, "arrange", json!({"refs": ["R1", "R2"]}));
    let synced = run_tool("sync_board", json!({}), &ctx).unwrap();
    assert_ne!(synced["code"], json!("bench_not_empty"), "{synced:#}");
}
