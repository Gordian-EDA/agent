//! The KiCAD-ERC defects a BluePill dev-board run left on the sheet: no-connect
//! markers the wiring tools never cleared, a `label` that merged two named nets
//! without saying so, and wire runs left dangling by a part removal.
//!
//! Skips when no KiCAD is installed: the mutators embed library definitions.

use geom::Point2;
use gordian_runtime::AgentRuntime;
use sch_doc::{Item, LabelKind, Pose, SchDoc, connect, placed_pins};
use serde_json::{Value, json};

const EMPTY_SHEET: &str = "(kicad_sch\n\
\t(version 20250114)\n\
\t(generator \"eeschema\")\n\
\t(generator_version \"9.0\")\n\
\t(uuid \"4a1c0f2e-0000-4000-8000-0000000000bb\")\n\
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
        .unwrap_or_else(|e| panic!("`{name}` failed: {e}"))
}

fn add(ctx: &AgentRuntime, parts: Value) {
    let added = call(ctx, "place_parts", json!({ "parts": parts }));
    assert!(added.get("error").is_none(), "fixture failed: {added}");
}

fn pin_at(doc: &SchDoc, refdes: &str, number: &str) -> Point2 {
    placed_pins(doc)
        .into_iter()
        .find(|pin| pin.refdes == refdes && pin.number == number)
        .unwrap_or_else(|| panic!("no pin {refdes}.{number}"))
        .at
}

fn markers(doc: &SchDoc) -> Vec<Point2> {
    doc.items()
        .iter()
        .filter_map(|item| match item {
            Item::NoConnect(marker) => Some(marker.at),
            _ => None,
        })
        .collect()
}

/// Wiring a pin that carries a no-connect marker must take the marker away —
/// KiCAD reports the pair as `no_connect_connected` — and say that it did.
#[test]
fn connect_clears_the_no_connect_marker_on_a_pin_it_wires() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    add(
        &ctx,
        json!([
            {"lib_id": "Device:R", "ref": "R1"},
            {"lib_id": "Device:R", "ref": "R2"},
        ]),
    );
    let marked = call(&ctx, "no_connect", json!({"pins": ["R1.2", "R2.1"]}));
    assert!(marked.get("error").is_none(), "fixture failed: {marked}");
    assert_eq!(markers(&SchDoc::read(ctx.sch_path()).unwrap()).len(), 2);

    let result = call(&ctx, "connect", json!({"from": "R1.2", "to": "R2.1"}));

    assert!(result.get("error").is_none(), "connect failed: {result}");
    let cleared = &result["changed"]["removed_no_connects"];
    assert_eq!(cleared, &json!(["R1.2", "R2.1"]), "{result}");
    let doc = SchDoc::read(ctx.sch_path()).unwrap();
    assert!(markers(&doc).is_empty(), "markers survived the wiring");
    let after = connect::extract(&doc);
    assert!(
        after.nets.iter().any(|net| net.pins.len() == 2),
        "the two pins did not end up on one net: {:?}",
        after.nets
    );
}

/// A marker severs its point, so it has to go BEFORE the router looks at the
/// sheet: left in place it makes a perfectly good path read as unreachable and
/// the connection degrades to a pair of labels over a dead wire.
#[test]
fn a_stale_marker_does_not_degrade_a_route_into_labels() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    add(
        &ctx,
        json!([
            {"lib_id": "Device:R", "ref": "R1"},
            {"lib_id": "Device:R", "ref": "R2"},
        ]),
    );
    call(&ctx, "no_connect", json!({"pins": ["R1.2"]}));

    let result = call(&ctx, "connect", json!({"from": "R1.2", "to": "R2.1"}));

    let changed = result["changed"]["changed"].as_str().unwrap_or_default();
    assert!(
        changed.starts_with("wired"),
        "the route fell back to labels: {result}"
    );
    assert!(
        SchDoc::read(ctx.sch_path()).unwrap().labels().count() == 0,
        "a routed connection must not leave fallback labels"
    );
}

/// A no-connect on a net with other members is a refusal, not a marker: the
/// marker would sever a connection the sheet actually draws.
#[test]
fn no_connect_refuses_a_pin_on_a_multi_pin_net() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    add(
        &ctx,
        json!([
            {"lib_id": "Device:R", "ref": "R1"},
            {"lib_id": "Device:R", "ref": "R2"},
        ]),
    );
    let wired = call(&ctx, "connect", json!({"from": "R1.2", "to": "R2.1"}));
    assert!(wired.get("error").is_none(), "fixture failed: {wired}");

    let result = call(&ctx, "no_connect", json!({"pins": ["R1.2"]}));

    let error = result["error"].as_str().unwrap_or_default();
    assert!(error.contains("R1.2"), "{result}");
    assert!(error.contains("R2.1"), "{result}");
    assert!(
        markers(&SchDoc::read(ctx.sch_path()).unwrap()).is_empty(),
        "a refused no_connect must write nothing"
    );
}

/// A pin joined to another only by NAME still has other members: naming is a
/// connection, and the extractor has to report it as one.
#[test]
fn no_connect_refuses_a_pin_on_a_label_joined_net() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCad detected");
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
        let named = call(&ctx, "connect", json!({"pin": pin, "net": "SWDIO"}));
        assert!(named.get("error").is_none(), "fixture failed: {named}");
    }

    let result = call(&ctx, "no_connect", json!({"pins": ["R1.2"]}));

    assert!(
        result["error"]
            .as_str()
            .unwrap_or_default()
            .contains("R2.1"),
        "{result}"
    );
}

/// One point carries one marker. A second marker at the same coordinate says
/// nothing more and KiCAD counts it separately.
#[test]
fn a_repeated_no_connect_does_not_stack_markers() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    add(&ctx, json!([{"lib_id": "Device:R", "ref": "R1"}]));
    call(&ctx, "no_connect", json!({"pins": ["R1.1"]}));

    call(&ctx, "no_connect", json!({"pins": ["R1.1"]}));

    assert_eq!(markers(&SchDoc::read(ctx.sch_path()).unwrap()).len(), 1);
}

/// Hanging a second authored name on a pin does not rename its net — it merges
/// two of them, and nothing in the drawing shows that it happened.
#[test]
fn naming_a_pin_that_already_has_an_authored_net_refuses() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    add(&ctx, json!([{"lib_id": "Device:R", "ref": "R1"}]));
    let named = call(&ctx, "connect", json!({"pin": "R1.2", "net": "USB_D-"}));
    assert!(named.get("error").is_none(), "fixture failed: {named}");

    let result = call(&ctx, "connect", json!({"from": "R1.2", "net": "USB_D+"}));

    let error = result["error"].as_str().unwrap_or_default();
    assert!(error.contains("USB_D-"), "{result}");
    assert!(error.contains("USB_D+"), "{result}");
    let doc = SchDoc::read(ctx.sch_path()).unwrap();
    assert_eq!(doc.labels().count(), 1, "the refused label was written");
}

/// Naming a partition KiCAD named for itself is not a merge: there is no
/// authored name to lose.
#[test]
fn naming_a_pin_on_a_generated_net_is_still_allowed() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    add(
        &ctx,
        json!([
            {"lib_id": "Device:R", "ref": "R1"},
            {"lib_id": "Device:R", "ref": "R2"},
        ]),
    );
    let wired = call(&ctx, "connect", json!({"from": "R1.2", "to": "R2.1"}));
    assert!(wired.get("error").is_none(), "fixture failed: {wired}");

    let result = call(&ctx, "connect", json!({"from": "R1.2", "net": "USB_D+"}));

    assert!(result.get("error").is_none(), "{result}");
    let after = connect::extract(&SchDoc::read(ctx.sch_path()).unwrap());
    assert!(
        after.nets.iter().any(|net| net.name == "USB_D+"),
        "{after:?}"
    );
}

/// Removing a part takes the wire run that only reached its pin with it — the
/// far end of that run is not held up by the label the run was drawn to carry.
#[test]
fn remove_symbols_retracts_the_run_up_to_its_label() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    add(&ctx, json!([{"lib_id": "Device:R", "ref": "R5"}]));
    let mut doc = SchDoc::read(ctx.sch_path()).unwrap();
    let pin = pin_at(&doc, "R5", "2");
    let bend = Point2::new(pin.x, pin.y + 3.81);
    let end = Point2::new(pin.x + 3.81, pin.y + 3.81);
    doc.add_wire(pin, bend);
    doc.add_wire(bend, end);
    doc.add_label(LabelKind::Local, "USB_D+", Pose::new(end.x, end.y, 0.0));
    doc.write(ctx.sch_path()).unwrap();

    let result = call(&ctx, "remove_symbols", json!({"refs": ["R5"]}));

    assert!(result.get("error").is_none(), "{result}");
    assert!(
        result["changed"]["removed"]["wires"].as_u64().unwrap_or(0) >= 2
            && result["changed"]["removed"]["labels"] == 1,
        "the run and its label stayed behind: {result}"
    );
    let after = SchDoc::read(ctx.sch_path()).unwrap();
    assert_eq!(after.wires().count(), 0, "a wire run was left dangling");
    assert_eq!(after.labels().count(), 0, "the run's label was left behind");
}

/// The run is peeled back to the first branch and no further: a junction where
/// another net's wire carries on is where somebody else's drawing begins.
#[test]
fn retraction_stops_at_a_junction() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    add(
        &ctx,
        json!([
            {"lib_id": "Device:R", "ref": "R5"},
            {"lib_id": "Device:R", "ref": "R6"},
        ]),
    );
    let mut doc = SchDoc::read(ctx.sch_path()).unwrap();
    let doomed = pin_at(&doc, "R5", "2");
    let kept = pin_at(&doc, "R6", "1");
    let tee = Point2::new(doomed.x, doomed.y + 3.81);
    let bend = Point2::new(kept.x, tee.y);
    doc.add_wire(doomed, tee);
    doc.add_wire(tee, bend);
    doc.add_wire(bend, kept);
    doc.add_junction(tee);
    doc.write(ctx.sch_path()).unwrap();

    let result = call(&ctx, "remove_symbols", json!({"refs": ["R5"]}));

    assert!(result.get("error").is_none(), "{result}");
    let after = SchDoc::read(ctx.sch_path()).unwrap();
    assert_eq!(
        after.wires().count(),
        2,
        "the wire past the junction was taken too"
    );
}

/// The marker on a removed pin has no pin left to speak for: KiCAD calls that
/// `no_connect_dangling`.
#[test]
fn remove_symbols_takes_the_no_connect_marker_with_the_pin() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    add(&ctx, json!([{"lib_id": "Device:R", "ref": "R5"}]));
    call(&ctx, "no_connect", json!({"pins": ["R5.1", "R5.2"]}));

    let result = call(&ctx, "remove_symbols", json!({"refs": ["R5"]}));

    assert!(result.get("error").is_none(), "{result}");
    assert!(
        markers(&SchDoc::read(ctx.sch_path()).unwrap()).is_empty(),
        "a marker was left with no pin under it"
    );
}

/// The silent short, in full: a marker left on a wired pin severs it, so the pin
/// reads as belonging to no net, so naming it raises no objection — and the name
/// lands on top of the net that was already there.
#[test]
fn a_marker_cannot_hide_the_net_a_pin_is_already_on() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    add(
        &ctx,
        json!([
            {"lib_id": "Device:R", "ref": "R5"},
            {"lib_id": "Device:R", "ref": "R6"},
        ]),
    );
    for pin in ["R5.1", "R6.2"] {
        let named = call(&ctx, "connect", json!({"pin": pin, "net": "USB_D-"}));
        assert!(named.get("error").is_none(), "fixture failed: {named}");
    }
    // The marker place_parts leaves behind on a pin it thought was spare.
    let mut doc = SchDoc::read(ctx.sch_path()).unwrap();
    doc.add_no_connect(pin_at(&doc, "R5", "1"));
    doc.write(ctx.sch_path()).unwrap();
    assert!(
        connect::extract(&SchDoc::read(ctx.sch_path()).unwrap())
            .nets
            .iter()
            .find(|net| net.name == "USB_D-")
            .is_none_or(|net| net.pins.len() == 1),
        "fixture must hide R5.1 from the netlist"
    );

    let result = call(&ctx, "connect", json!({"from": "R5.1", "net": "USB_D+"}));

    let error = result["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("USB_D-") && error.contains("USB_D+"),
        "{result}"
    );
    let after = connect::extract(&SchDoc::read(ctx.sch_path()).unwrap());
    assert!(
        after.nets.iter().all(|net| net.name != "USB_D+"),
        "the refused name was written anyway: {:?}",
        after.nets
    );
}

/// A net the sheet already names with a pennant keeps that scope when a later
/// call names another pin on it: a plain label of the same name is a different
/// net that merely reads the same, and KiCAD says so.
#[test]
fn a_later_label_adopts_the_nets_existing_scope() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    add(
        &ctx,
        json!([
            {"lib_id": "Device:R", "ref": "R1"},
            {"lib_id": "Device:R", "ref": "R2"},
        ]),
    );
    let global = call(
        &ctx,
        "connect",
        json!({"pin": "R1.1", "net": "SWDIO", "kind": "global"}),
    );
    assert!(global.get("error").is_none(), "fixture failed: {global}");

    let named = call(&ctx, "connect", json!({"from": "R2.1", "net": "SWDIO"}));

    assert!(named.get("error").is_none(), "{named}");
    let doc = SchDoc::read(ctx.sch_path()).unwrap();
    let kinds: Vec<LabelKind> = doc
        .labels()
        .filter(|label| sch_doc::unescape(&label.text) == "SWDIO")
        .map(|label| label.kind)
        .collect();
    assert_eq!(kinds, vec![LabelKind::Global; 2], "scopes split");
    let net = connect::extract(&doc)
        .nets
        .into_iter()
        .find(|net| net.name == "SWDIO")
        .expect("SWDIO exists");
    assert_eq!(net.pins.len(), 2, "the two labels did not join: {net:?}");
}

/// Seating a rail on a marked pin connects it, so the marker goes — the same
/// rule `connect` follows.
#[test]
fn add_power_clears_the_marker_on_the_pin_it_feeds() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    add(&ctx, json!([{"lib_id": "Device:R", "ref": "R1"}]));
    call(&ctx, "no_connect", json!({"pins": ["R1.2"]}));

    let powered = call(&ctx, "connect", json!({"pin": "R1.2", "net": "GND"}));

    assert!(powered.get("error").is_none(), "{powered}");
    assert!(
        markers(&SchDoc::read(ctx.sch_path()).unwrap()).is_empty(),
        "a marker was left on a pin that now sits on a rail"
    );
}

/// Cutting the middle of a run leaves the surviving half dangling at the cut —
/// the same `unconnected_wire_endpoint` a removed pin leaves behind.
#[test]
fn delete_wires_retracts_the_half_it_leaves_dangling() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCad detected");
        return;
    };
    add(&ctx, json!([{"lib_id": "Device:R", "ref": "R1"}]));
    let mut doc = SchDoc::read(ctx.sch_path()).unwrap();
    let pin = pin_at(&doc, "R1", "2");
    let mid = Point2::new(pin.x, pin.y + 2.54);
    let far = Point2::new(pin.x, pin.y + 5.08);
    doc.add_wire(pin, mid);
    doc.add_wire(mid, far);
    doc.write(ctx.sch_path()).unwrap();

    let cut = call(&ctx, "delete_wires", json!({"pins": ["R1.2"]}));

    assert!(cut.get("error").is_none(), "{cut}");
    let after = SchDoc::read(ctx.sch_path()).unwrap();
    assert_eq!(after.wires().count(), 0, "the far half was left dangling");
}

/// A run held only by hierarchical sheet pins is not floating: a sheet pin is a
/// connection point no symbol owns, and sweeping it cuts the child sheet loose.
#[test]
fn a_run_between_sheet_pins_survives_an_unrelated_removal() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: no KiCad detected");
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
    let a = Point2::new(50.8, 50.8);
    let b = Point2::new(50.8, 63.5);
    doc.add_wire(a, b);
    let text = doc.to_text();
    let close = text.rfind("\n)").expect("the sheet closes");
    std::fs::write(
        ctx.sch_path(),
        format!("{}\n{}{}", &text[..close], child_sheet(a, b), ")\n"),
    )
    .unwrap();
    let seeded = SchDoc::read(ctx.sch_path()).unwrap();
    assert_eq!(
        seeded
            .items()
            .iter()
            .filter(|item| matches!(item, Item::Sheet(_)))
            .count(),
        1,
        "fixture must place a hierarchical sheet"
    );

    let removed = call(&ctx, "remove_symbols", json!({"refs": ["R1"]}));

    assert!(removed.get("error").is_none(), "{removed}");
    let after = SchDoc::read(ctx.sch_path()).unwrap();
    assert_eq!(
        after.wires().count(),
        1,
        "the run between two sheet pins was swept as floating"
    );
}

/// A hierarchical sheet whose two pins sit exactly on `a` and `b`.
fn child_sheet(a: Point2, b: Point2) -> String {
    format!(
        "\t(sheet\n\t\t(at 40 40)\n\t\t(size 30 30)\n\
         \t\t(uuid \"4a1c0f2e-0000-4000-8000-0000000000dd\")\n\
         \t\t(property \"Sheetname\" \"child\")\n\
         \t\t(property \"Sheetfile\" \"child.kicad_sch\")\n\
         \t\t(pin \"IN\" input\n\t\t\t(at {} {} 0)\n\
         \t\t\t(uuid \"4a1c0f2e-0000-4000-8000-0000000000de\")\n\t\t)\n\
         \t\t(pin \"OUT\" output\n\t\t\t(at {} {} 0)\n\
         \t\t\t(uuid \"4a1c0f2e-0000-4000-8000-0000000000df\")\n\t\t)\n\t)\n",
        a.x, a.y, b.x, b.y
    )
}
