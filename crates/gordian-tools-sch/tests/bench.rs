//! The bench: symbols that are on the sheet and on their nets, but not laid out.
//!
//! What is checked here is the loop the agent actually runs — `add_parts` to get
//! connectivity now, `arrange` to lay it out later — and the one refusal an
//! incomplete design earns.
//!
//! SKIPs cleanly without a KiCAD installation.

use gordian_runtime::AgentRuntime;
use serde_json::{Value, json};

const EMPTY_SHEET: &str = "(kicad_sch\n\
\t(version 20250114)\n\
\t(generator \"eeschema\")\n\
\t(generator_version \"10.0\")\n\
\t(uuid \"4a1c0f2e-0000-4000-8000-0000000000be\")\n\
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

fn divider() -> Value {
    json!({"parts": [
        {"ref": "R1", "part": "Device:R", "value": "10k", "pins": {"1": "VIN", "2": "MID"}},
        {"ref": "R2", "part": "Device:R", "value": "10k", "pins": {"1": "MID", "2": "GND"}},
        {"ref": "C1", "part": "Device:C", "value": "100n", "pins": {"1": "MID", "2": "GND"}}
    ]})
}

/// `add_parts` writes connectivity and nothing else; `arrange` turns it into a
/// drawing; the netlist is the same throughout.
#[test]
fn a_payload_reaches_the_bench_and_arrange_empties_it() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: KiCad 10 not configured");
        return;
    };
    let added = call(&ctx, "add_parts", divider());
    assert_eq!(added["ok"].as_bool(), None, "{added:#}");
    let benched: Vec<&str> = added["changed"]["benched"]
        .as_array()
        .unwrap_or_else(|| panic!("{added:#}"))
        .iter()
        .filter_map(|entry| entry["ref"].as_str())
        .collect();
    assert_eq!(benched, ["C1", "R1", "R2"], "{added:#}");

    // Connectivity is COMPLETE on the bench: the parts are on the nets they were
    // declared with, drawn as names rather than as wires.
    let checked = call(&ctx, "check_schematic", json!({}));
    assert_eq!(checked["bench"], json!(3), "{checked:#}");
    let net = call(&ctx, "get_net", json!({"name": "MID"}));
    let text = serde_json::to_string(&net).unwrap();
    for pin in ["R1", "R2", "C1"] {
        assert!(text.contains(pin), "MID is missing {pin}: {net:#}");
    }

    let arranged = call(&ctx, "arrange", json!({"refs": ["R1", "R2", "C1"]}));
    assert_eq!(arranged.get("error"), None, "{arranged:#}");
    let checked = call(&ctx, "check_schematic", json!({}));
    assert_eq!(checked["bench"], json!(0), "{checked:#}");
    let text = serde_json::to_string(&call(&ctx, "get_net", json!({"name": "MID"}))).unwrap();
    for pin in ["R1", "R2", "C1"] {
        assert!(text.contains(pin), "arranging lost {pin} from MID");
    }
}

/// A part nothing can resolve costs that part, not the payload it arrived in.
#[test]
fn an_unresolvable_part_is_reported_and_the_rest_is_placed() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: KiCad 10 not configured");
        return;
    };
    let placed = call(
        &ctx,
        "place_parts",
        json!({"parts": [
            {"ref": "R1", "part": "Device:R", "pins": {"1": "VIN", "2": "MID"}},
            {"ref": "R2", "part": "Device:R", "pins": {"1": "MID", "2": "GND"}},
            {"ref": "J1", "part": "Device:Conn_01x02", "pins": {"1": "VIN", "2": "GND"}},
            {"ref": "U9", "part": "Device:R", "pins": {"NOSUCHPIN": "MID"}}
        ]}),
    );

    assert_eq!(placed.get("error"), None, "{placed:#}");
    let unplaced = placed["changed"]["unplaced"]
        .as_array()
        .unwrap_or_else(|| panic!("{placed:#}"));
    let refs: Vec<&str> = unplaced.iter().filter_map(|p| p["ref"].as_str()).collect();
    assert_eq!(refs, ["J1", "U9"], "{placed:#}");
    let suggestion = unplaced[0]["did_you_mean"][0].as_str().unwrap_or_default();
    assert!(
        suggestion.contains("Conn_01x02"),
        "the right symbol under the wrong library: {unplaced:#?}"
    );
    assert!(
        placed["text"]
            .as_str()
            .is_some_and(|text| text.starts_with("PLACED  R1 R2")),
        "{placed:#}"
    );
}

/// A layout hint of the wrong SHAPE is still only a hint. Dropping it costs a rail
/// band; refusing the payload costs the whole block — which is what a campaign run
/// did when the model wrote `"rails": "+3V3"`.
#[test]
fn a_malformed_intent_is_dropped_not_refused() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: KiCad 10 not configured");
        return;
    };
    let mut payload = divider();
    payload["intent"] = json!({ "rails": "+3V3" });

    let placed = call(&ctx, "place_parts", payload);

    assert_eq!(placed.get("error"), None, "{placed:#}");
    let warnings = serde_json::to_string(&placed["warnings"]).unwrap();
    assert!(warnings.contains("intent.rails"), "{placed:#}");
    assert!(
        placed["text"]
            .as_str()
            .is_some_and(|text| text.starts_with("PLACED  C1 R1 R2")),
        "{placed:#}"
    );
}
