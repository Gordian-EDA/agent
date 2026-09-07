//! The three ERC classes a finished sheet used to be left holding.
//!
//! Each one is reproduced here as the engine produces it, repaired with the very
//! call its own finding carries, and re-checked against real ERC. Skips when no
//! KiCAD is installed: every assertion is against `kicad-cli`.

use gordian_runtime::AgentRuntime;
use serde_json::{Value, json};

const EMPTY_SHEET: &str = "(kicad_sch\n\
\t(version 20250114)\n\
\t(generator \"eeschema\")\n\
\t(generator_version \"9.0\")\n\
\t(uuid \"4a1c0f2e-0000-4000-8000-0000000000dd\")\n\
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

fn findings(ctx: &AgentRuntime) -> Vec<Value> {
    call(ctx, "check_schematic", json!({"detail": true}))["findings"]
        .as_array()
        .expect("check_schematic reports findings")
        .clone()
}

/// Every electrical-rule violation KiCAD itself calls an error, whatever the
/// planner then decided to do about it.
fn erc_errors(ctx: &AgentRuntime) -> Vec<String> {
    findings(ctx)
        .iter()
        .filter(|finding| finding["source"] == "kicad_erc")
        .filter(|finding| finding["severity"] == "error" || finding["advisory"] == true)
        .map(|finding| format!("{} {}", finding["code"], finding["refs"]))
        .collect()
}

/// Declare a rail no output pin drives, the way KiCAD asks for: one PWR_FLAG.
/// A ground made of rail glyphs alone is `power_pin_not_driven` otherwise, which
/// is a different finding and would drown out the one under test.
fn declare_rail(ctx: &AgentRuntime, pin: &str, net: &str) {
    let flag = call(ctx, "connect", json!({"pin": pin, "net": net}));
    assert_eq!(
        flag["power_symbol_used"], "power:PWR_FLAG",
        "fixture must flag {net}: {flag}"
    );
}

fn of_code<'a>(findings: &'a [Value], code: &str) -> Option<&'a Value> {
    findings.iter().find(|finding| finding["code"] == code)
}

/// Run a finding's machine-applicable fix exactly as the model would.
fn apply(ctx: &AgentRuntime, finding: &Value) -> Value {
    let tool = finding["fix"]["tool"]
        .as_str()
        .unwrap_or_else(|| panic!("the finding carries no fix: {finding:#?}"));
    let result = call(ctx, tool, finding["fix"]["args"].clone());
    assert!(
        result.get("error").is_none(),
        "the fix `{tool}` was refused: {result}"
    );
    result
}

/// A 3.3V regulator feeding a load, with a rail glyph on its output — and then a
/// PWR_FLAG on that same rail, which is what the engine used to add whenever a
/// power net looked undeclared. The flag's pin and the regulator's output are
/// both typed Power output, which IS `pin_to_pin`.
fn flagged_regulator(ctx: &AgentRuntime) {
    let added = call(
        ctx,
        "place_parts",
        json!({"parts": [
            {"lib_id": "Regulator_Linear:AMS1117-3.3", "ref": "U1"},
            {"lib_id": "Device:R", "ref": "R1"},
        ]}),
    );
    assert!(added.get("error").is_none(), "fixture failed: {added}");
    call(ctx, "connect", json!({"from": "U1.2", "to": "R1.1"}));
    call(ctx, "connect", json!({"pin": "U1.1", "net": "GND"}));
    call(ctx, "connect", json!({"pin": "R1.2", "net": "GND"}));
    call(ctx, "connect", json!({"pin": "U1.3", "net": "+5V"}));
    call(ctx, "connect", json!({"pin": "U1.2", "net": "+3V3"}));
    declare_rail(ctx, "U1.1", "GND");
    declare_rail(ctx, "U1.3", "+5V");
    declare_rail(ctx, "U1.2", "+3V3");
}

/// A flag on a rail a regulator already drives is the error, and taking it away
/// is the whole repair — one call, and the rail keeps every pin it had.
#[test]
fn a_redundant_power_flag_is_removed_by_the_finding_it_raises() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    flagged_regulator(&ctx);
    let before = findings(&ctx);
    let conflict = of_code(&before, "pin_to_pin")
        .unwrap_or_else(|| panic!("fixture must raise pin_to_pin: {before:#?}"));
    assert_eq!(conflict["fix"]["tool"], "remove_symbols", "{conflict:#?}");

    let applied = apply(&ctx, conflict);
    let moved = applied["net_delta"]["now_unconnected"].as_array().unwrap();
    assert!(
        moved
            .iter()
            .all(|pin| pin.as_str().unwrap().starts_with("#FLG")),
        "removing the flag moved a real pin: {applied}"
    );
    assert_eq!(erc_errors(&ctx), Vec::<String>::new());
}

/// Two part outputs on one net is a real conflict in the drawing, not a flag to
/// drop, so the planner refuses — but it says which two pins and the exact two
/// calls that clear it, and those two calls do clear it.
#[test]
fn two_real_drivers_keep_a_refusal_and_a_two_call_sequence_that_works() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    let added = call(
        &ctx,
        "place_parts",
        json!({"parts": [
            {"lib_id": "Regulator_Linear:AMS1117-3.3", "ref": "U1"},
            {"lib_id": "Regulator_Linear:AMS1117-3.3", "ref": "U2"},
            {"lib_id": "Device:R", "ref": "R1"},
        ]}),
    );
    assert!(added.get("error").is_none(), "fixture failed: {added}");
    call(&ctx, "connect", json!({"from": "U1.2", "to": "R1.1"}));
    for (pin, net) in [
        ("U1.1", "GND"),
        ("U2.1", "GND"),
        ("R1.2", "GND"),
        ("U1.3", "+5V"),
        ("U2.3", "+5V"),
        ("U1.2", "+3V3"),
        ("U2.2", "+3V3"),
    ] {
        call(&ctx, "connect", json!({"pin": pin, "net": net}));
    }
    declare_rail(&ctx, "U1.1", "GND");
    declare_rail(&ctx, "U1.3", "+5V");
    let conflict = findings(&ctx);
    let conflict = of_code(&conflict, "pin_to_pin")
        .unwrap_or_else(|| panic!("fixture must tie two regulator outputs together"));
    assert!(conflict["fix"].is_null(), "{conflict:#?}");
    let why = conflict["why"].as_str().unwrap_or_default().to_string();
    for expected in ["U1.2", "U2.2", "no_connect", "PWR_FLAG"] {
        assert!(why.contains(expected), "the reason omits {expected}: {why}");
    }

    // The sequence the reason prescribes for U2.2, run as written.
    let cut = why
        .split("or `")
        .nth(1)
        .and_then(|rest| rest.split('`').next())
        .expect("the reason names the call that takes U2.2 off the rail")
        .to_string();
    let (tool, args) = cut.split_once(' ').expect("the call carries its arguments");
    let cut = call(&ctx, tool, serde_json::from_str(args).unwrap());
    assert!(cut.get("error").is_none(), "{cut}");
    let marked = call(&ctx, "no_connect", json!({"pin": "U2.2"}));
    assert!(marked.get("error").is_none(), "{marked}");
    assert!(
        of_code(&findings(&ctx), "pin_to_pin").is_none(),
        "the two calls the reason prescribes did not part the two drivers"
    );
    // Taking a regulator off the rail leaves that regulator's own stubs loose,
    // and nothing else: the half of the sheet the calls did not name is clean.
    let left = erc_errors(&ctx);
    assert!(
        left.iter()
            .all(|finding| finding.contains("U2") || finding.contains("#PWR")),
        "parting the drivers disturbed a part it did not name: {left:?}"
    );
}

/// Append a label anchored in empty space — what a cut wire or a re-drawn block
/// used to leave behind.
fn strand_a_label(ctx: &AgentRuntime, text: &str, uuid: &str) {
    let sheet = std::fs::read_to_string(ctx.sch_path()).unwrap();
    let stranded = format!(
        "\t(label \"{text}\"\n\t\t(at 33.02 33.02 0)\n\t\t(effects\n\t\t\t(font\n\t\t\t\t(size 1.27 1.27)\n\t\t\t)\n\t\t\t(justify left bottom)\n\t\t)\n\t\t(uuid \"{uuid}\")\n\t)\n)\n"
    );
    std::fs::write(
        ctx.sch_path(),
        format!("{}{stranded}", sheet.trim_end().trim_end_matches(')')),
    )
    .unwrap();
}

/// A pair of resistors wired together, so the sheet is otherwise clean.
fn wired_pair(ctx: &AgentRuntime) {
    call(
        ctx,
        "place_parts",
        json!({"parts": [
            {"lib_id": "Device:R", "ref": "R1"},
            {"lib_id": "Device:R", "ref": "R2"},
        ]}),
    );
    call(ctx, "connect", json!({"from": "R1.2", "to": "R2.1"}));
    call(ctx, "connect", json!({"pin": "R1.1", "net": "GND"}));
    call(ctx, "connect", json!({"pin": "R2.2", "net": "GND"}));
    declare_rail(ctx, "R1.1", "GND");
}

/// The finding carries the call that clears it, and the call clears it.
#[test]
fn a_stranded_label_is_cleared_by_the_fix_its_finding_carries() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    wired_pair(&ctx);
    strand_a_label(&ctx, "ORPHAN", "11111111-2222-4333-8444-555555555555");
    let before = findings(&ctx);
    let dangling = of_code(&before, "label_dangling")
        .unwrap_or_else(|| panic!("fixture must strand a label: {before:#?}"));
    assert_eq!(dangling["fix"]["tool"], "delete_wires", "{dangling:#?}");

    let applied = apply(&ctx, dangling);
    assert_eq!(
        applied["net_delta"], "connectivity unchanged",
        "dropping a label that touches nothing moved a net: {applied}"
    );
    assert_eq!(erc_errors(&ctx), Vec::<String>::new());
}

/// No edit may leave one behind, whichever tool made the cut: the commit path
/// drops a label that touches nothing, and proves it changed no net by the same
/// extraction diff every other edit is held to.
#[test]
fn no_edit_leaves_a_label_touching_nothing_on_the_sheet() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    wired_pair(&ctx);
    strand_a_label(&ctx, "ORPHAN", "11111111-2222-4333-8444-555555555556");

    let moved = call(&ctx, "arrange", json!({"refs": ["R2"]}));
    assert!(moved.get("error").is_none(), "{moved}");

    let doc = sch_doc::SchDoc::read(ctx.sch_path()).unwrap();
    assert_eq!(
        sch_doc::stray_labels(&doc),
        Vec::<String>::new(),
        "an unrelated edit left the stranded label on the sheet"
    );
    assert!(
        of_code(&findings(&ctx), "label_dangling").is_none(),
        "ERC still reads a dangling label"
    );
}

/// A connector shell nothing on the sheet shares a net or a function with. There
/// is no connection to restore, so the honest repair is to say it is deliberate.
#[test]
fn a_loose_shield_pin_with_no_partner_is_marked_no_connect() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    wired_pair(&ctx);
    let added = call(
        &ctx,
        "place_parts",
        json!({"parts": [
            {"lib_id": "Connector:USB_C_Receptacle_PowerOnly_6P", "ref": "J1"},
        ]}),
    );
    assert!(added.get("error").is_none(), "fixture failed: {added}");
    for pin in ["A1", "B1", "A4", "B4"] {
        call(
            &ctx,
            "connect",
            json!({"pin": format!("J1.{pin}"), "net": "GND"}),
        );
    }
    call(&ctx, "connect", json!({"from": "J1.A4", "to": "R1.2"}));

    let before = findings(&ctx);
    let loose = before
        .iter()
        .find(|finding| {
            finding["code"] == "pin_not_connected" && finding["fix"]["tool"] == "no_connect"
        })
        .unwrap_or_else(|| panic!("fixture must leave a pin loose: {before:#?}"));
    assert_eq!(loose["fix"]["tool"], "no_connect", "{loose:#?}");

    let pin = loose["fix"]["args"]["pin"].as_str().unwrap().to_string();
    assert!(
        pin.ends_with(".SHIELD") || pin.ends_with(".SH"),
        "{loose:#?}"
    );
    apply(&ctx, loose);
    assert!(
        !erc_errors(&ctx)
            .iter()
            .any(|left| left.contains("pin_not_connected") && left.contains(&pin)),
        "the finding survived its own fix: {:?}",
        erc_errors(&ctx)
    );
}

/// A signal input named by a label that reaches nothing else. No output can ever
/// appear on that net and nothing on the sheet can be wired to it without
/// inventing the connection, so the pin is a dead end and saying so is the fix.
#[test]
fn an_input_on_a_dead_end_net_is_marked_no_connect() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    wired_pair(&ctx);
    let added = call(
        &ctx,
        "place_parts",
        json!({"parts": [{"lib_id": "Amplifier_Operational:LM358", "ref": "U1"}]}),
    );
    assert!(added.get("error").is_none(), "fixture failed: {added}");
    call(&ctx, "connect", json!({"pin": "U1.8", "net": "+5V"}));
    declare_rail(&ctx, "U1.8", "+5V");
    call(&ctx, "connect", json!({"pin": "U1.4", "net": "GND"}));
    call(&ctx, "connect", json!({"from": "U1.1", "to": "U1.2"}));
    call(&ctx, "connect", json!({"pin": "U1.3", "net": "SENSE_IN"}));

    let before = findings(&ctx);
    let undriven = of_code(&before, "pin_not_driven")
        .unwrap_or_else(|| panic!("fixture must leave an undriven input: {before:#?}"));
    assert_eq!(
        undriven["fix"],
        json!({"tool": "no_connect", "args": {"pin": "U1.3"}}),
        "{undriven:#?}"
    );

    apply(&ctx, undriven);
    assert!(
        !erc_errors(&ctx).contains(&"\"pin_not_driven\" [\"U1.3\"]".to_string()),
        "the finding survived its own fix: {:?}",
        erc_errors(&ctx)
    );
}
