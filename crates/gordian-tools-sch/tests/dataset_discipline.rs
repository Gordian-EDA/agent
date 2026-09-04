//! A request that fixes the part list: no gaps to tempt an addition, and a
//! report of how far the sheet is from the netlist it must reproduce.

use gordian_runtime::AgentRuntime;
use gordian_runtime::workspace::RequestScope;
use serde_json::{Value, json};

fn payload(strict: bool) -> Value {
    json!({
        "name": "Strict",
        "block": "regulator",
        "strict": strict,
        "parts": [
            {"ref": "J1", "part": "Connector_Generic:Conn_01x02", "pins": {"1": "VIN", "2": "GND"}},
            {"ref": "U1", "part": "Regulator_Linear:AMS1117-3.3",
             "pins": {"VI": "VIN", "GND": "GND", "VO": "+3V3"}},
            {"ref": "R1", "part": "Device:R", "value": "10k", "pins": {"1": "+3V3", "2": "GND"}}
        ],
        "layout": {
            "regulator": {"row": [{"part": "J1"}, {"part": "U1"}, {"part": "R1"}]}
        }
    })
}

fn place(ctx: &AgentRuntime, strict: bool) -> Value {
    gordian_tools_sch::run("place_parts", payload(strict), ctx)
        .expect("registered tool")
        .expect("place_parts result")
}

fn gaps(value: &Value) -> &Vec<Value> {
    value["gaps"].as_array().expect("gaps array")
}

#[test]
fn strict_mode_silences_every_completeness_gap() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    let permissive = place(&ctx, false);
    assert!(
        !gaps(&permissive).is_empty(),
        "fixture must produce gaps to suppress: {permissive}"
    );

    let Some(strict) = AgentRuntime::detect_for_test() else {
        return;
    };
    let placed = place(&strict, true);
    assert!(
        gaps(&placed).is_empty(),
        "strict placement must report no gaps: {placed}"
    );
    assert!(strict.request_scope().no_additions);

    let checked = gordian_tools_sch::run("check_schematic", json!({}), &strict)
        .expect("registered tool")
        .expect("check_schematic result");
    assert_eq!(
        checked.pointer("/completeness/gaps"),
        Some(&json!([])),
        "strict checks must not suggest additions: {checked}"
    );
    assert_eq!(checked.pointer("/completeness/strict"), Some(&json!(true)));
}

#[test]
fn a_strict_request_recorded_on_the_project_silences_a_later_check() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    place(&ctx, false);
    ctx.workspace()
        .set_request_scope(&RequestScope { no_additions: true })
        .unwrap();
    let checked = gordian_tools_sch::run("check_schematic", json!({}), &ctx)
        .expect("registered tool")
        .expect("check_schematic result");
    assert_eq!(checked.pointer("/completeness/gaps"), Some(&json!([])));
}

#[test]
fn netlist_fidelity_names_the_extra_part_and_the_mis_netted_pin() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    // The dataset extractor's shape: a titled document. R1 is never named.
    let reference = json!({"title": "Strict", "parts": [
        {"ref": "J1", "lib_id": "Connector_Generic:Conn_01x02",
         "pins": {"1": "VIN", "2": "GND"}},
        {"ref": "U1", "lib_id": "Regulator_Linear:AMS1117-3.3",
         "pins": {"1": "GND", "2": "+3V3", "3": "VIN"}},
        {"ref": "R9", "lib_id": "Device:R", "value": "10k",
         "pins": {"1": "+3V3", "2": "GND"}}
    ]});
    std::fs::write(
        ctx.project_dir().join("netlist.json"),
        serde_json::to_vec(&reference).unwrap(),
    )
    .unwrap();
    place(&ctx, true);

    let checked = gordian_tools_sch::run("check_schematic", json!({}), &ctx)
        .expect("registered tool")
        .expect("check_schematic result");
    let fidelity = &checked["netlist_fidelity"];
    assert_eq!(fidelity["matches"], json!(false), "{checked}");
    assert_eq!(fidelity["parts_extra"], json!(["R1"]), "{fidelity}");
    assert_eq!(fidelity["parts_missing"], json!(["R9"]), "{fidelity}");

    // With R2 wired where the reference puts its output pin, the same sheet is
    // faithful apart from the part names; move one pin and it is not.
    let mis_netted = json!({"title": "Strict", "parts": [
        {"ref": "J1", "lib_id": "Connector_Generic:Conn_01x02",
         "pins": {"1": "VIN", "2": "GND"}},
        {"ref": "U1", "lib_id": "Regulator_Linear:AMS1117-3.3",
         "pins": {"1": "GND", "2": "+3V3", "3": "VIN"}},
        {"ref": "R1", "lib_id": "Device:R", "value": "10k",
         "pins": {"1": "+3V3", "2": "VIN"}}
    ]});
    std::fs::write(
        ctx.project_dir().join("netlist.json"),
        serde_json::to_vec(&mis_netted).unwrap(),
    )
    .unwrap();
    let checked = gordian_tools_sch::run("check_schematic", json!({}), &ctx)
        .expect("registered tool")
        .expect("check_schematic result");
    let fidelity = &checked["netlist_fidelity"];
    assert_eq!(fidelity["matches"], json!(false), "{fidelity}");
    assert_eq!(fidelity["parts_extra"], json!([]), "{fidelity}");
    assert_eq!(fidelity["parts_missing"], json!([]), "{fidelity}");
    let pins = fidelity["pins_mis_netted"]
        .as_array()
        .expect("mis-netted pins");
    assert_eq!(pins.len(), 1, "{fidelity}");
    assert_eq!(pins[0]["pin"], json!("R1.2"), "{fidelity}");
    assert_eq!(pins[0]["expected_net"], json!("VIN"), "{fidelity}");
}

#[test]
fn an_unreadable_reference_netlist_is_reported_rather_than_ignored() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    std::fs::write(ctx.project_dir().join("netlist.json"), b"{\"parts\": 7}").unwrap();
    place(&ctx, false);
    let checked = gordian_tools_sch::run("check_schematic", json!({}), &ctx)
        .expect("registered tool")
        .expect("check_schematic result");
    assert!(
        checked["netlist_fidelity"]["error"].is_string(),
        "{checked}"
    );
}

/// The scope is the request's, not the project's: the next request that says
/// nothing about the part list gets its gaps back.
#[test]
fn a_later_permissive_request_clears_the_strict_scope() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    place(&ctx, true);
    assert!(ctx.request_scope().no_additions);
    ctx.workspace()
        .set_request_scope(&RequestScope::default())
        .unwrap();
    let checked = gordian_tools_sch::run("check_schematic", json!({}), &ctx)
        .expect("registered tool")
        .expect("check_schematic result");
    assert!(
        !checked["completeness"]["gaps"]
            .as_array()
            .expect("gaps")
            .is_empty(),
        "{checked}"
    );
}
