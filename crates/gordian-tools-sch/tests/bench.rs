//! The bench: symbols that are on the sheet and on their nets, but not laid out.
//!
//! What is checked here is what `place_parts` does when a payload cannot be drawn,
//! and how `arrange` lays the benched parts out later.
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

#[test]
fn connectivity_furniture_returns_the_nothing_placed_shape() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: KiCad 10 not configured");
        return;
    };

    let result = call(
        &ctx,
        "place_parts",
        json!({
            "block": "erc_repair",
            "parts": [{"part": "power:PWR_FLAG", "pins": {"1": "+5V_USB"}, "ref": "#FLG5"}]
        }),
    );

    assert_eq!(result["code"], "nothing_placed", "{result:#}");
    assert_eq!(result["ok"], false);
    assert_eq!(result["unplaced"][0]["ref"], "#FLG5");
    assert_eq!(result["unplaced"][0]["part"], "power:PWR_FLAG");
}

#[test]
fn arrange_reports_power_furniture_and_nearby_real_parts() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: KiCad 10 not configured");
        return;
    };
    let placed = call(&ctx, "place_parts", divider());
    assert_eq!(placed.get("error"), None, "{placed:#}");
    let flags_of = |doc: &sch_doc::SchDoc| -> Vec<String> {
        doc.symbols()
            .filter(|s| s.refdes().starts_with("#FLG"))
            .map(|s| s.uuid.clone())
            .collect()
    };
    let before = flags_of(&sch_doc::SchDoc::read(ctx.sch_path()).unwrap());
    // A pin already on its rail gets a PWR_FLAG: two more flags, at R2.2 and C1.2.
    for pin in ["R2.2", "C1.2"] {
        let flagged = call(&ctx, "connect", json!({"from": pin, "net": "GND"}));
        assert_eq!(flagged["power_symbol_used"], "power:PWR_FLAG", "{flagged:#}");
    }
    let mut doc = sch_doc::SchDoc::read(ctx.sch_path()).unwrap();
    let flags: Vec<String> = flags_of(&doc).into_iter().filter(|u| !before.contains(u)).collect();
    assert_eq!(flags.len(), 2, "two flags landed");
    for (uuid, to) in flags.iter().zip(["#FLG_Q9", "#FLG_RAW2"]) {
        doc.set_field(uuid, "Reference", to).unwrap();
    }
    doc.write(ctx.sch_path()).unwrap();

    let furniture = call(
        &ctx,
        "arrange",
        json!({
            "refs": ["#FLG_Q9", "#FLG_RAW2"]
        }),
    );
    assert_eq!(furniture.get("error"), None, "{furniture:#}");
    assert_eq!(
        furniture["changed"], "no arrangeable parts selected",
        "{furniture:#}"
    );
    assert_eq!(
        furniture["not_arrangeable"],
        json!(["#FLG_Q9", "#FLG_RAW2"]),
        "{furniture:#}"
    );
    assert!(
        furniture["arrangeable_nearby"]["#FLG_Q9"]
            .as_array()
            .is_some_and(|refs| !refs.is_empty()),
        "{furniture:#}"
    );

    let mixed = call(
        &ctx,
        "arrange",
        json!({
            "refs": ["#FLG_Q9", "R2"]
        }),
    );
    assert_eq!(mixed.get("error"), None, "{mixed:#}");
    assert_eq!(mixed["changed"]["moved"], json!(["R2"]), "{mixed:#}");
    assert_eq!(mixed["not_arrangeable"], json!(["#FLG_Q9"]), "{mixed:#}");
}

#[test]
fn connecting_coincident_power_and_flag_pins_is_idempotent() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: KiCad 10 not configured");
        return;
    };
    // R1.1 on GND draws the rail glyph; naming the pin GND again adds a PWR_FLAG on
    // the same point, so the glyph pin and the flag pin coincide.
    let placed = call(
        &ctx,
        "place_parts",
        json!({"parts": [{"ref": "R1", "part": "Device:R", "pins": {"1": "GND", "2": "OUT"}}],
               "intent": {"ports": {"OUT": "right"}}}),
    );
    assert_eq!(placed.get("error"), None, "{placed:#}");
    let flagged = call(&ctx, "connect", json!({"from": "R1.1", "net": "GND"}));
    assert_eq!(flagged["power_symbol_used"], "power:PWR_FLAG", "{flagged:#}");
    let mut doc = sch_doc::SchDoc::read(ctx.sch_path()).unwrap();
    let glyph = doc.symbols().find(|s| s.refdes().starts_with("#PWR")).unwrap().uuid.clone();
    let flag = doc.symbols().find(|s| s.refdes().starts_with("#FLG")).unwrap().uuid.clone();
    doc.set_field(&glyph, "Reference", "#PWR_GND").unwrap();
    doc.set_field(&flag, "Reference", "#FLG_GND").unwrap();
    doc.write(ctx.sch_path()).unwrap();

    let connected = call(
        &ctx,
        "connect",
        json!({"from": "#PWR_GND.1", "to": "#FLG_GND.1"}),
    );

    assert_eq!(connected.get("error"), None, "{connected:#}");
    assert_eq!(connected["changed"], "already connected", "{connected:#}");
    assert_eq!(
        connected["net_delta"], "connectivity unchanged",
        "{connected:#}"
    );
}
