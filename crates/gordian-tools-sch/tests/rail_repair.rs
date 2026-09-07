//! A rail glyph left touching nothing.
//!
//! KiCAD reports `pin_not_connected` on a `power:` symbol whose one pin has no
//! wire under it. Every edit now sweeps such a glyph as it commits, as long as
//! its net still has a member elsewhere, so the model never sees the finding for
//! a rail the engine drew; the sole mention of a rail stays, and its finding
//! carries the one repair that clears it. `connect` between two glyphs that
//! already read the same name is refused, so a no-op cannot pass as a repair.
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

/// Two resistors on a GND rail, with the second rail glyph cut loose by the
/// last edit.
fn orphan_rail_sheet(ctx: &AgentRuntime) {
    let added = call(
        ctx,
        "place_parts",
        json!({"parts": [
            {"lib_id": "Device:R", "ref": "R1"},
            {"lib_id": "Device:R", "ref": "R2"},
        ]}),
    );
    assert!(added.get("error").is_none(), "fixture failed: {added}");
    call(ctx, "connect", json!({"from": "R1.2", "to": "R2.1"}));
    call(ctx, "connect", json!({"pin": "R1.1", "net": "GND"}));
    call(ctx, "connect", json!({"pin": "R2.2", "net": "GND"}));
    call(ctx, "delete_wires", json!({"pins": ["R2.2"]}));
}

/// Whether a symbol with this reference is on the sheet.
fn on_sheet(ctx: &AgentRuntime, reference: &str) -> bool {
    sch_doc::SchDoc::read(ctx.sch_path())
        .unwrap()
        .symbols()
        .any(|symbol| symbol.refdes() == reference)
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

/// The edit that cuts a rail loose takes the glyph with it: nothing is left for
/// ERC to report, and the model never sees a repair it did not ask for.
#[test]
fn a_rail_cut_loose_is_swept_by_the_edit_that_cut_it() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    orphan_rail_sheet(&ctx);

    assert!(!on_sheet(&ctx, "#PWR2"), "the loose rail glyph survived the cut");
    assert!(on_sheet(&ctx, "#PWR1"), "the rail that feeds R1.1 was taken");
    let bare: Vec<Value> = erc_errors(&ctx)
        .into_iter()
        .filter(|finding| finding["code"] == "pin_not_connected")
        .filter(|finding| finding["refs"].to_string().contains("#PWR"))
        .collect();
    assert!(bare.is_empty(), "a rail glyph still reads as bare: {bare:#?}");
}

/// The only glyph naming a rail is the author's, however bare: it stays, its
/// finding names the one repair that clears it, and that repair clears it.
#[test]
fn the_sole_mention_of_a_rail_stays_and_its_finding_clears_in_one_call() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    call(&ctx, "place_parts", json!({"parts": [{"lib_id": "Device:R", "ref": "R1"}]}));
    call(&ctx, "connect", json!({"pin": "R1.1", "net": "GND"}));
    call(&ctx, "delete_wires", json!({"pins": ["R1.1"]}));
    assert!(on_sheet(&ctx, "#PWR1"), "the sole GND glyph was swept");

    let before = erc_errors(&ctx);
    let finding = find(&before, "pin_not_connected", "#PWR1.1")
        .unwrap_or_else(|| panic!("the bare rail is reported: {before:#?}"));
    assert_eq!(
        finding["fix"],
        json!({"tool": "remove_symbols", "args": {"refs": ["#PWR1"]}}),
        "{finding}"
    );
    apply(&ctx, finding);
    assert!(
        find(&erc_errors(&ctx), "pin_not_connected", "#PWR1.1").is_none(),
        "the finding survived its own fix"
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

/// The blue-pill shape: several rail glyphs cut loose one after another. Each
/// cut glyph goes as soon as the rail has another member, so at most the last
/// one — the sole mention — is ever reported, and its one repair clears it.
#[test]
fn rails_cut_loose_one_after_another_never_pile_up() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    let refs = ["R1", "R2", "R3", "R4", "R5"];
    call(
        &ctx,
        "place_parts",
        json!({"parts": refs.iter().map(|reference| json!({"lib_id": "Device:R", "ref": reference}))
            .collect::<Vec<_>>()}),
    );
    for reference in refs {
        call(
            &ctx,
            "connect",
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
    assert!(bare.len() <= 1, "cut rails piled up: {bare:#?}");
    if let Some(finding) = bare.first() {
        apply(&ctx, finding);
    }
    assert!(
        erc_errors(&ctx)
            .iter()
            .all(|finding| finding["code"] != "pin_not_connected" || !finding["refs"].to_string().contains("#PWR")),
        "a bare rail survived"
    );
}
