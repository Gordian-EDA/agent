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
    let ctx = AgentRuntime::detect_for_test()?;
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
    let result = call(ctx, "add_symbols", json!({"parts": parts}));
    assert!(result.get("error").is_none(), "fixture failed: {result}");
}

#[test]
fn connect_joins_derived_endpoints_but_not_two_authored_nets() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: KiCad 10 not configured");
        return;
    };
    add_resistors(&ctx, &["R1", "R2", "R3", "R4", "R5"]);

    let seeded = call(&ctx, "connect", json!({"from": "R1.2", "to": "R2.1"}));
    assert!(seeded.get("error").is_none(), "fixture failed: {seeded}");
    let named = call(&ctx, "label", json!({"pin": "R3.1", "net": "3V3"}));
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
    let net = call(&ctx, "get_net", json!({"name": "3V3"})).to_string();
    for pin in ["R1.2", "R2.1", "R3.1"] {
        assert!(net.contains(pin), "{pin} absent from joined net: {net}");
    }

    for (pin, net) in [("R4.1", "SDA"), ("R5.1", "SCL")] {
        let result = call(&ctx, "label", json!({"pin": pin, "net": net}));
        assert!(result.get("error").is_none(), "fixture failed: {result}");
    }
    let refused = call(&ctx, "connect", json!({"from": "R4.1", "to": "R5.1"}));
    let error = refused["error"].as_str().unwrap_or_default();
    assert!(error.contains("SDA") && error.contains("SCL"), "{refused}");
    assert_eq!(
        refused["fix"],
        json!({"tool": "delete_wires", "args": {"pins": ["R5.1"]}})
    );
    let sda = call(&ctx, "get_net", json!({"name": "SDA"})).to_string();
    let scl = call(&ctx, "get_net", json!({"name": "SCL"})).to_string();
    assert!(sda.contains("R4.1") && !sda.contains("R5.1"), "{sda}");
    assert!(scl.contains("R5.1") && !scl.contains("R4.1"), "{scl}");
}

#[test]
fn connect_accepts_a_net_name_as_either_endpoint() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: KiCad 10 not configured");
        return;
    };
    add_resistors(&ctx, &["R1", "R2", "R3"]);
    let named = call(&ctx, "label", json!({"pin": "R1.1", "net": "VREF"}));
    assert!(named.get("error").is_none(), "fixture failed: {named}");

    let existing = call(&ctx, "connect", json!({"from": "R2.1", "to": "VREF"}));
    assert!(existing.get("error").is_none(), "{existing}");
    assert_eq!(existing["net_endpoint"]["existed"], true, "{existing}");
    let created = call(&ctx, "connect", json!({"from": "NEW_SENSE", "to": "R3.1"}));
    assert!(created.get("error").is_none(), "{created}");
    assert_eq!(created["net_endpoint"]["created"], true, "{created}");

    let vref = call(&ctx, "get_net", json!({"name": "VREF"})).to_string();
    assert!(vref.contains("R1.1") && vref.contains("R2.1"), "{vref}");
    let sense = call(&ctx, "get_net", json!({"name": "NEW_SENSE"})).to_string();
    assert!(sense.contains("R3.1"), "{sense}");
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
    let net = call(&ctx, "get_net", json!({"name": "N_R1_2"})).to_string();
    for pin in ["R1.2", "R2.1", "R3.1", "R4.1", "C1.1"] {
        assert!(net.contains(pin), "{pin} absent from resolved net: {net}");
    }

    let missing = call(
        &ctx,
        "label",
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
fn remove_symbols_declares_every_net_at_the_selected_symbols() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: KiCad 10 not configured");
        return;
    };
    let added = call(
        &ctx,
        "add_symbols",
        json!({"parts": [
            {"lib_id": "Device:R", "ref": "R1"},
            {"lib_id": "power:GND", "ref": "#PWR01"},
            {"lib_id": "power:VCC", "ref": "#PWR02", "value": "3V3"}
        ]}),
    );
    assert!(added.get("error").is_none(), "fixture failed: {added}");
    let doc = sch_doc::SchDoc::read(ctx.sch_path()).unwrap();
    let pin = sch_doc::placed_pins(&doc)
        .into_iter()
        .find(|pin| pin.refdes == "R1" && pin.number == "1")
        .unwrap()
        .at;
    let mut source = std::fs::read_to_string(ctx.sch_path()).unwrap();
    for reference in ["#PWR01", "#PWR02"] {
        let symbol = doc.symbol_by_ref(reference).unwrap();
        let power_pin = sch_doc::placed_pins(&doc)
            .into_iter()
            .find(|candidate| candidate.refdes == reference)
            .unwrap();
        source = sch_floorplan::test_util::replace_symbol_at(
            &source,
            reference,
            [
                pin.x - (power_pin.at.x - symbol.at.x),
                pin.y - (power_pin.at.y - symbol.at.y),
            ],
        );
    }
    std::fs::write(ctx.sch_path(), source).unwrap();

    let mut positioned = sch_doc::SchDoc::read(ctx.sch_path()).unwrap();
    for (reference, value) in [("#PWR01", "GND"), ("#PWR02", "3V3")] {
        let uuid = positioned.symbol_by_ref(reference).unwrap().uuid.clone();
        positioned.set_field(&uuid, "Value", value).unwrap();
    }
    let markers = positioned
        .items()
        .iter()
        .filter_map(|item| match item {
            sch_doc::Item::NoConnect(marker) if marker.at.near_eq(pin, geom::EPS) => {
                Some(marker.uuid.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    positioned.remove_drawing(&markers);
    positioned.write(ctx.sch_path()).unwrap();
    let before = call(&ctx, "get_net", json!({"name": "3V3"})).to_string();
    assert!(before.contains("R1.1"), "fixture did not overlap: {before}");
    let removed = call(&ctx, "remove_symbols", json!({"refs": ["#PWR02"]}));
    assert!(removed.get("error").is_none(), "{removed}");
    assert_eq!(removed["changed"]["removed"]["power"], 1, "{removed}");
    let netlist = ctx.env().netlist(ctx.sch_path()).expect("KiCad 10 netlist");
    let rail = netlist
        .nets
        .iter()
        .find(|net| net.nodes.contains(&("R1".to_string(), "1".to_string())))
        .map(|net| net.name.as_str());
    assert_eq!(rail, Some("GND"), "{netlist:?}");
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
        .find(|net| {
            net.nodes
                .contains(&("U1".to_string(), "4".to_string()))
        })
        .map(|net| net.name.as_str());
    assert!(
        nc_net.is_some_and(|name| name.starts_with("unconnected-(")),
        "{netlist:?}"
    );
}
