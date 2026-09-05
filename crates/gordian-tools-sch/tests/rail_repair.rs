//! A rail glyph left touching nothing, and the loop it used to open.
//!
//! KiCAD reports `pin_not_connected` on a `power:` symbol whose one pin has no
//! wire under it. The repair the planner used to offer was `connect` to another
//! glyph on the same rail — which joins nothing, because both ends already read
//! that name — so the call reported success, the sheet came back byte-identical,
//! and the same finding came back with it. These tests hold the loop shut from
//! both ends: the fix a finding carries has to clear the finding, and `connect`
//! has to stop calling a no-op a connection.
//!
//! Skips when no KiCAD is installed: every assertion is against real ERC.

use gordian_runtime::AgentRuntime;
use serde_json::{Value, json};

const EMPTY_SHEET: &str = "(kicad_sch\n\
\t(version 20250114)\n\
\t(generator \"eeschema\")\n\
\t(generator_version \"9.0\")\n\
\t(uuid \"4a1c0f2e-0000-4000-8000-0000000000cc\")\n\
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

/// Two resistors on a GND rail, with the second rail glyph cut loose: its pin
/// touches nothing, and it sits far enough away that no wire reads as local.
fn orphan_rail_sheet(ctx: &AgentRuntime) {
    let added = call(
        ctx,
        "add_symbols",
        json!({"parts": [
            {"lib_id": "Device:R", "ref": "R1"},
            {"lib_id": "Device:R", "ref": "R2"},
        ]}),
    );
    assert!(added.get("error").is_none(), "fixture failed: {added}");
    call(ctx, "connect", json!({"from": "R1.2", "to": "R2.1"}));
    call(ctx, "add_power", json!({"pin": "R1.1", "net": "GND"}));
    call(ctx, "add_power", json!({"pin": "R2.2", "net": "GND"}));
    call(ctx, "delete_wires", json!({"pins": ["R2.2"]}));
    call(
        ctx,
        "move_symbols",
        json!({"moves": [{"ref": "#PWR2", "to": [60.0, 60.0]}]}),
    );
}

/// Every blocking finding KiCAD's ERC raises against the sheet as it stands.
fn erc_errors(ctx: &AgentRuntime) -> Vec<Value> {
    call(ctx, "check_schematic", json!({"detail": true}))["findings"]
        .as_array()
        .expect("check_schematic reports findings")
        .iter()
        .filter(|finding| finding["severity"] == "error")
        .cloned()
        .collect()
}

fn find<'a>(findings: &'a [Value], code: &str, reference: &str) -> Option<&'a Value> {
    findings.iter().find(|finding| {
        finding["code"] == code
            && finding["refs"]
                .as_array()
                .is_some_and(|refs| refs.iter().any(|found| found == reference))
    })
}

/// Run a finding's machine-applicable fix exactly as the model would.
fn apply(ctx: &AgentRuntime, finding: &Value) -> Value {
    let fix = &finding["fix"];
    let tool = fix["tool"].as_str().expect("the finding carries a fix");
    let result = call(ctx, tool, fix["args"].clone());
    assert!(
        result.get("error").is_none(),
        "the fix `{tool}` was refused: {result}"
    );
    result
}

/// The loop, closed: the finding names a repair, the repair runs, and the
/// finding is gone. Nothing else the sheet was holding up comes loose.
#[test]
fn an_orphan_rails_fix_clears_the_finding_it_came_with() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    orphan_rail_sheet(&ctx);
    let before = erc_errors(&ctx);
    let finding = find(&before, "pin_not_connected", "#PWR2.1")
        .unwrap_or_else(|| panic!("fixture must leave the rail bare: {before:#?}"));

    let applied = apply(&ctx, finding);

    assert_eq!(
        applied["changed"]["now_loose"],
        json!([]),
        "removing a pin that touches nothing loosened another pin: {applied}"
    );
    let after = erc_errors(&ctx);
    assert!(
        find(&after, "pin_not_connected", "#PWR2.1").is_none(),
        "the finding survived its own fix: {after:#?}"
    );
}

/// The repair on offer has to be one that works wherever the glyph happens to
/// sit. Wiring it to another glyph on the rail only clears the rule when the two
/// are close enough to draw between, so it is never the answer.
#[test]
fn the_offered_repair_is_not_a_wire_to_another_glyph_on_the_rail() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    orphan_rail_sheet(&ctx);
    let findings = erc_errors(&ctx);

    let finding = find(&findings, "pin_not_connected", "#PWR2.1").expect("the rail reads as bare");

    assert_eq!(
        finding["fix"],
        json!({"tool": "remove_symbols", "args": {"refs": ["#PWR2"]}}),
        "{finding:#?}"
    );
    assert!(
        finding["why"]
            .as_str()
            .is_some_and(|why| why.contains("add_power")),
        "the reason must name the constructive alternative: {finding:#?}"
    );
}

/// Naming both ends of a connection joins them only where the name is new to an
/// end. Two glyphs on one rail already read it, so the labels are dropped again
/// as duplicates and the file is byte-identical — a refusal, not a connection.
#[test]
fn connect_refuses_a_name_both_ends_already_read() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    orphan_rail_sheet(&ctx);
    let before = std::fs::read_to_string(ctx.sch_path()).unwrap();

    let result = call(&ctx, "connect", json!({"from": "#PWR2.1", "to": "#PWR1.1"}));

    let error = result["error"].as_str().unwrap_or_default();
    assert!(error.contains("already read `GND`"), "{result}");
    assert!(error.contains("remove_symbols"), "{result}");
    assert!(error.contains("add_power"), "{result}");
    assert_eq!(
        std::fs::read_to_string(ctx.sch_path()).unwrap(),
        before,
        "a refused connect wrote to the sheet"
    );
}

/// The refusal is about a name that adds nothing, not about naming: two ordinary
/// pins too far apart to wire are still joined by a label at each end, and KiCAD
/// stops calling them unconnected.
#[test]
fn joining_ordinary_pins_by_name_still_connects_them() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    call(
        &ctx,
        "add_symbols",
        json!({"parts": [
            {"lib_id": "Device:R", "ref": "R1"},
            {"lib_id": "Device:R", "ref": "R2"},
        ]}),
    );
    call(
        &ctx,
        "move_symbols",
        json!({"moves": [{"ref": "R2", "to": [60.0, 60.0]}]}),
    );

    let result = call(&ctx, "connect", json!({"from": "R1.2", "to": "R2.1"}));

    assert!(result.get("error").is_none(), "{result}");
    assert_eq!(
        result["net_delta"]["now_connected"],
        json!(["R1.2", "R2.1"]),
        "{result}"
    );
    let after = call(&ctx, "check_schematic", json!({"detail": true}));
    let loose = after["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|finding| finding["code"] == "pin_not_connected")
        .filter(|finding| {
            let refs = finding["refs"].to_string();
            refs.contains("R1.2") || refs.contains("R2.1")
        })
        .count();
    assert_eq!(
        loose, 0,
        "the named pins still read as unconnected: {after}"
    );
}

/// A rail glyph whose pin does touch something is carrying its net, and no
/// finding may offer to take it away.
#[test]
fn a_rail_that_reaches_a_pin_is_never_offered_for_removal() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    orphan_rail_sheet(&ctx);

    let offered_for_removal: Vec<Value> = erc_errors(&ctx)
        .iter()
        .filter(|finding| finding["fix"]["tool"] == "remove_symbols")
        .flat_map(|finding| {
            finding["fix"]["args"]["refs"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .collect();

    assert!(
        !offered_for_removal.contains(&json!("#PWR1")),
        "#PWR1 feeds R1.1 and must not be offered for removal: {offered_for_removal:?}"
    );
}

/// The blue-pill shape: several rail glyphs cut loose at once. The thrash guard
/// budgets rail removals by the CALL and stops at three, so a repair offered one
/// glyph at a time would be refused as a purge halfway through clearing itself.
/// One call names them all.
#[test]
fn every_bare_rail_is_cleared_by_one_call() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    let refs = ["R1", "R2", "R3", "R4", "R5"];
    call(
        &ctx,
        "add_symbols",
        json!({"parts": refs.iter().map(|reference| json!({"lib_id": "Device:R", "ref": reference}))
            .collect::<Vec<_>>()}),
    );
    for reference in refs {
        call(
            &ctx,
            "add_power",
            json!({"pin": format!("{reference}.2"), "net": "GND"}),
        );
        call(
            &ctx,
            "delete_wires",
            json!({"pins": [format!("{reference}.2")]}),
        );
    }
    let bare: Vec<Value> = erc_errors(&ctx)
        .into_iter()
        .filter(|finding| finding["code"] == "pin_not_connected")
        .filter(|finding| finding["fix"]["tool"] == "remove_symbols")
        .collect();
    assert!(
        bare.len() >= 4,
        "fixture must leave several rails bare: {bare:#?}"
    );

    let applied = apply(&ctx, &bare[0]);

    assert_eq!(
        applied["changed"]["removed"]["power"].as_u64().unwrap_or(0) as usize,
        bare.len(),
        "one call must take every bare rail: {applied}"
    );
    let after = erc_errors(&ctx);
    assert!(
        after
            .iter()
            .all(|finding| finding["code"] != "pin_not_connected"
                || finding["fix"]["tool"] != "remove_symbols"),
        "bare rails survived the one call that named them all: {after:#?}"
    );
}

/// The v4 blue-pill ladder, rung by rung. The run that exposed this defect went
/// check -> connect -> check -> connect -> check -> no_connect -> delete_wires ->
/// remove_symbols, every rung answering an unchanged finding, and the purge at
/// the end took the rails a working sheet was using. Replayed here, the ladder
/// has no second rung: the first repair the finding names clears it.
#[test]
fn the_escalation_ladder_has_nothing_left_to_climb() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    orphan_rail_sheet(&ctx);

    // Rung 1, as the run made it: read the finding and do what it says.
    let finding = erc_errors(&ctx)
        .into_iter()
        .find(|finding| finding["code"] == "pin_not_connected")
        .expect("the bare rail is reported");
    apply(&ctx, &finding);
    assert!(
        erc_errors(&ctx)
            .iter()
            .all(|left| left["code"] != "pin_not_connected"),
        "the finding survived rung 1, which is where the ladder used to start"
    );

    // Rung 2, the move the run repeated: it is now refused outright, so even a
    // caller that ignores the finding cannot mistake a no-op for progress.
    std::fs::write(ctx.sch_path(), EMPTY_SHEET).unwrap();
    orphan_rail_sheet(&ctx);
    let repeated = call(&ctx, "connect", json!({"from": "#PWR2.1", "to": "#PWR1.1"}));
    assert!(
        repeated.get("error").is_some(),
        "wiring one rail glyph to another still reports success: {repeated}"
    );
}
