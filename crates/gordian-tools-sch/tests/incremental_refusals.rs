//! Regression coverage for edits that state their connectivity intent.

use gordian_runtime::AgentRuntime;
use serde_json::{Value, json};

const EMPTY_SHEET: &str = "(kicad_sch\n\
\t(version 20250114)\n\
\t(generator \"eeschema\")\n\
\t(generator_version \"10.0\")\n\
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
    let ctx =
        AgentRuntime::detect_for_test().filter(|ctx| ctx.env().major_version() == Some(10))?;
    std::fs::write(ctx.sch_path(), EMPTY_SHEET).unwrap();
    Some(ctx)
}

fn call(ctx: &AgentRuntime, name: &str, input: Value) -> Value {
    gordian_tools_sch::run(name, input, ctx)
        .unwrap_or_else(|| panic!("`{name}` is not a schematic tool"))
        .unwrap_or_else(|error| panic!("`{name}` failed: {error}"))
}

fn add_resistors(ctx: &AgentRuntime, refs: &[&str]) {
    let parts = refs
        .iter()
        .map(|reference| json!({"lib_id": "Device:R", "ref": reference}))
        .collect::<Vec<_>>();
    let result = call(ctx, "place_parts", json!({"parts": parts}));
    assert!(result.get("error").is_none(), "fixture failed: {result}");
}

#[test]
fn connect_joins_every_net_carried_by_its_named_endpoints() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: KiCad 10 not configured");
        return;
    };
    add_resistors(&ctx, &["R1", "R2", "R3", "R4", "R5"]);

    let seeded = call(&ctx, "connect", json!({"from": "R1.2", "to": "R2.1"}));
    assert!(seeded.get("error").is_none(), "fixture failed: {seeded}");
    let named = call(&ctx, "connect", json!({"pin": "R3.1", "net": "3V3"}));
    assert!(named.get("error").is_none(), "fixture failed: {named}");

    let joined = call(&ctx, "connect", json!({"from": "R3.1", "to": "R1.2"}));
    assert!(joined.get("error").is_none(), "{joined}");
    assert_eq!(joined["joined"]["from_net"], "3V3", "{joined}");
    assert!(
        joined["joined"]["to_net"]
            .as_str()
            .is_some_and(|net| net.starts_with("Net-(")),
        "{joined}"
    );
    assert_eq!(joined["joined"]["survivor"], "3V3", "{joined}");
    let net = call(&ctx, "read_schematic", json!({"net": "3V3"})).to_string();
    for pin in ["R1.2", "R2.1", "R3.1"] {
        assert!(net.contains(pin), "{pin} absent from joined net: {net}");
    }

    for (pin, net) in [("R4.1", "CHG"), ("R5.1", "DSG")] {
        let result = call(&ctx, "connect", json!({"pin": pin, "net": net}));
        assert!(result.get("error").is_none(), "fixture failed: {result}");
    }
    let joined = call(&ctx, "connect", json!({"from": "R4.1", "to": "R5.1"}));
    assert!(joined.get("error").is_none(), "{joined}");
    assert_eq!(joined["joined"]["from_net"], "CHG", "{joined}");
    assert_eq!(joined["joined"]["to_net"], "DSG", "{joined}");
    let survivor = joined["joined"]["survivor"].as_str().unwrap();
    let merged = call(&ctx, "read_schematic", json!({"net": survivor})).to_string();
    assert!(
        merged.contains("R4.1") && merged.contains("R5.1"),
        "{merged}"
    );
    let oracle = ctx.env().netlist(ctx.sch_path()).unwrap();
    assert!(oracle.nets.iter().any(|net| {
        net.nodes.contains(&("R4".into(), "1".into()))
            && net.nodes.contains(&("R5".into(), "1".into()))
    }));
}

#[test]
fn connect_accepts_a_net_name_as_either_endpoint() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: KiCad 10 not configured");
        return;
    };
    add_resistors(&ctx, &["R1", "R2", "R3", "R4"]);
    let power = call(&ctx, "connect", json!({"pin": "R4.2", "net": "GND"}));
    assert!(power.get("error").is_none(), "fixture failed: {power}");
    let named = call(&ctx, "connect", json!({"pin": "R1.1", "net": "VREF"}));
    assert!(named.get("error").is_none(), "fixture failed: {named}");

    let existing = call(&ctx, "connect", json!({"from": "R2.1", "to": "VREF"}));
    assert!(existing.get("error").is_none(), "{existing}");
    assert_eq!(existing["net_endpoint"]["existed"], true, "{existing}");
    let created = call(&ctx, "connect", json!({"from": "NEW_SENSE", "to": "R3.1"}));
    assert!(created.get("error").is_none(), "{created}");
    assert_eq!(created["net_endpoint"]["created"], true, "{created}");

    let vref = call(&ctx, "read_schematic", json!({"net": "VREF"})).to_string();
    assert!(vref.contains("R1.1") && vref.contains("R2.1"), "{vref}");
    let sense = call(&ctx, "read_schematic", json!({"net": "NEW_SENSE"})).to_string();
    assert!(sense.contains("R3.1"), "{sense}");
    let power_only = call(&ctx, "connect", json!({"from": "R4.1", "to": "GND"}));
    assert!(power_only.get("error").is_none(), "{power_only}");
    assert_eq!(power_only["net_endpoint"]["existed"], true, "{power_only}");
}

#[test]
fn add_power_authorizes_the_power_net_and_the_named_pins_net() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: KiCad 10 not configured");
        return;
    };
    add_resistors(&ctx, &["R1", "R2"]);
    let seeded = call(&ctx, "connect", json!({"from": "R1.1", "to": "R2.1"}));
    assert!(seeded.get("error").is_none(), "fixture failed: {seeded}");
    let named = call(&ctx, "connect", json!({"pin": "R1.1", "net": "3V3"}));
    assert!(named.get("error").is_none(), "fixture failed: {named}");

    let powered = call(&ctx, "connect", json!({"pin": "R1.1", "net": "GND"}));

    assert!(powered.get("error").is_none(), "{powered}");
    let ground = call(&ctx, "read_schematic", json!({"net": "GND"})).to_string();
    assert!(
        ground.contains("R1.1") && ground.contains("R2.1"),
        "{ground}"
    );
}

#[test]
fn a_failed_connect_batch_summarizes_every_endpoint_reason() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: KiCad 10 not configured");
        return;
    };
    add_resistors(&ctx, &["R1"]);

    let failed = call(
        &ctx,
        "connect",
        json!({"pairs": [
            {"from": "NOPE.1", "to": "R1.1"},
            {"from": "R1.2", "to": "MISSING.1"}
        ]}),
    );

    let error = failed["error"].as_str().unwrap_or_default();
    assert!(error.contains("NOPE.1 -> R1.1"), "{failed}");
    assert!(error.contains("R1.2 -> MISSING.1"), "{failed}");
    assert!(error.matches("no symbol").count() >= 2, "{failed}");
    assert!(!error.contains("every connection failed"), "{failed}");
}

#[test]
fn copied_derived_net_names_resolve_in_payloads_and_connect() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: KiCad 10 not configured");
        return;
    };
    add_resistors(&ctx, &["R1", "R2", "R3", "R4"]);
    let seeded = call(&ctx, "connect", json!({"from": "R1.2", "to": "R2.1"}));
    assert!(seeded.get("error").is_none(), "fixture failed: {seeded}");
    let listing = call(&ctx, "read_schematic", json!({})).to_string();
    let start = listing.find("Net-(").expect("fixture has a derived net");
    let generated = &listing[start..start + listing[start..].find(')').unwrap() + 1];

    let placed = call(
        &ctx,
        "place_parts",
        json!({"parts": [{
            "ref": "C1",
            "part": "Device:C",
            "pins": {"1": generated, "2": "GND"}
        }]}),
    );
    assert!(placed.get("error").is_none(), "{placed}");
    assert_eq!(placed["resolved_nets"][generated], "@R1.2", "{placed}");

    let suffixed = format!("{generated}_1");
    let connected = call(
        &ctx,
        "connect",
        json!({"from": "R3.1", "to": "R4.1", "net": suffixed}),
    );
    assert!(connected.get("error").is_none(), "{connected}");
    assert_eq!(
        connected["resolved_nets"][&suffixed], "@R1.2",
        "{connected}"
    );
    let net = call(&ctx, "read_schematic", json!({"net": "N_R1_2"})).to_string();
    for pin in ["R1.2", "R2.1", "R3.1", "R4.1", "C1.1"] {
        assert!(net.contains(pin), "{pin} absent from resolved net: {net}");
    }

    let missing = call(
        &ctx,
        "connect",
        json!({"pin": "R3.2", "net": "Net-(U99-NOPE)"}),
    );
    assert!(
        missing["error"].as_str().is_some_and(
            |error| error.contains("pin `U99.NOPE`") && error.contains("does not exist")
        ),
        "{missing}"
    );
}

#[test]
fn place_parts_turns_a_library_nc_connection_into_a_reported_gap() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: KiCad 10 not configured");
        return;
    };
    let placed = call(
        &ctx,
        "place_parts",
        json!({"parts": [
            {
                "ref": "U1",
                "part": "Regulator_Linear:AP2112K-3.3",
                "pins": {"1": "VIN", "2": "GND", "3": "3V3", "4": "EN_REG", "5": "VIN"}
            },
            {"ref": "C1", "part": "Device:C", "pins": {"1": "VIN", "2": "GND"}},
            {"ref": "C2", "part": "Device:C", "pins": {"1": "3V3", "2": "GND"}}
        ]}),
    );
    assert!(placed.get("error").is_none(), "{placed}");
    assert_ne!(placed.get("ok"), Some(&json!(false)), "{placed}");
    assert_eq!(
        placed["nc_overridden"],
        json!([{"ref": "U1", "pin": "4", "requested_net": "EN_REG"}]),
        "{placed}"
    );
    assert!(
        placed["gaps"]
            .as_array()
            .is_some_and(|gaps| gaps.iter().any(|gap| {
                gap["kind"] == "library_no_connect_overridden"
                    && gap["ref"] == "U1"
                    && gap["pin"] == "4"
                    && gap["requested_net"] == "EN_REG"
            })),
        "{placed}"
    );
    let doc = sch_doc::SchDoc::read(ctx.sch_path()).unwrap();
    let nc_pin = sch_doc::placed_pins(&doc)
        .into_iter()
        .find(|pin| pin.refdes == "U1" && pin.number == "4")
        .unwrap();
    assert!(
        doc.items().iter().any(|item| {
            matches!(item, sch_doc::Item::NoConnect(marker) if marker.at.near_eq(nc_pin.at, geom::EPS))
        }),
        "U1.4 was not marked no-connect: {placed}"
    );
    let netlist = ctx.env().netlist(ctx.sch_path()).expect("KiCad 10 netlist");
    assert!(
        netlist.nets.iter().all(|net| net.name != "EN_REG"),
        "the refused requested net survived: {netlist:?}"
    );
    let nc_net = netlist
        .nets
        .iter()
        .find(|net| net.nodes.contains(&("U1".to_string(), "4".to_string())))
        .map(|net| net.name.as_str());
    assert!(
        nc_net.is_some_and(|name| name.starts_with("unconnected-(")),
        "{netlist:?}"
    );
}
