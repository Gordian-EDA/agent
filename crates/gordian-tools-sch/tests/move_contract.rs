//! KiCad-oracle contracts for dragging symbols with their drawn connections.

use std::collections::BTreeSet;

use geom::{EPS, Point2};
use gordian_runtime::AgentRuntime;
use kicad::Netlist;
use sch_doc::{LabelKind, Mirror, Pose, SchDoc, SymbolSource, placed_pins};
use serde_json::{Value, json};

const EMPTY_SHEET: &str = "(kicad_sch\n\
\t(version 20250114)\n\
\t(generator \"eeschema\")\n\
\t(generator_version \"10.0\")\n\
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

fn source(ctx: &AgentRuntime) -> SymbolSource {
    SymbolSource::new(ctx.env().symbol_dir().to_path_buf())
}

fn call(ctx: &AgentRuntime, input: Value) -> Value {
    gordian_tools_sch::run("move_symbols", input, ctx)
        .expect("move_symbols is registered")
        .expect("move_symbols executes")
}

fn partition(netlist: &Netlist) -> BTreeSet<Vec<String>> {
    netlist
        .nets
        .iter()
        .map(|net| {
            let mut pins: Vec<String> = net
                .nodes
                .iter()
                .filter(|(reference, _)| !reference.starts_with('#'))
                .map(|(reference, pin)| format!("{reference}.{pin}"))
                .collect();
            pins.sort();
            pins
        })
        .filter(|pins| !pins.is_empty())
        .collect()
}

fn net_of<'a>(netlist: &'a Netlist, reference: &str, pin: &str) -> Option<&'a str> {
    netlist
        .nets
        .iter()
        .find(|net| {
            net.nodes
                .iter()
                .any(|(found_ref, found_pin)| found_ref == reference && found_pin == pin)
        })
        .map(|net| net.name.trim_start_matches('/'))
}

fn assert_pins_drawn(doc: &SchDoc, reference: &str) {
    for pin in placed_pins(doc)
        .into_iter()
        .filter(|pin| pin.refdes == reference)
    {
        let wired = doc.wires().any(|wire| {
            wire.points
                .windows(2)
                .any(|ends| geom::Segment::new(ends[0], ends[1]).contains_point(pin.at))
        });
        let labelled = doc
            .labels()
            .any(|label| label.at.point().near_eq(pin.at, EPS));
        assert!(wired || labelled, "{reference}.{} is not drawn", pin.number);
    }
}

fn add_named_stubs(doc: &mut SchDoc, reference: &str) {
    for pin in placed_pins(doc)
        .into_iter()
        .filter(|pin| pin.refdes == reference)
    {
        let end = Point2::new(pin.at.x + pin.out.x * 12.7, pin.at.y + pin.out.y * 12.7);
        doc.add_wire(pin.at, end);
        doc.add_label(
            LabelKind::Local,
            &format!("SIG_{}", pin.number),
            Pose::new(end.x, end.y, 0.0),
        );
    }
}

fn wire_l(doc: &mut SchDoc, from: Point2, to: Point2) {
    let corner = Point2::new(to.x, from.y);
    if !from.near_eq(corner, EPS) {
        doc.add_wire(from, corner);
    }
    if !corner.near_eq(to, EPS) {
        doc.add_wire(corner, to);
    }
}

#[test]
fn move_rotate_and_mirror_a_multi_pin_symbol_keeps_kicad_netlist_and_drawing() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: KiCad 10 not configured");
        return;
    };
    let mut doc = SchDoc::read(ctx.sch_path()).unwrap();
    doc.add_symbol(
        "Connector_Generic:Conn_01x04",
        "J1",
        "Conn_01x04",
        Pose::new(100.0, 100.0, 0.0),
        &source(&ctx),
    )
    .unwrap();
    add_named_stubs(&mut doc, "J1");
    doc.write(ctx.sch_path()).unwrap();
    let before = ctx.env().netlist(ctx.sch_path()).unwrap();

    let result = call(
        &ctx,
        json!({"moves": [{
            "ref": "J1",
            "by": [20.32, 15.24],
            "rot": 90,
            "mirror": "x"
        }]}),
    );
    assert!(result.get("error").is_none(), "{result:#}");

    let after_doc = SchDoc::read(ctx.sch_path()).unwrap();
    let after = ctx.env().netlist(ctx.sch_path()).unwrap();
    assert_eq!(partition(&after), partition(&before));
    assert_pins_drawn(&after_doc, "J1");
    let symbol = after_doc.symbol_by_ref("J1").unwrap();
    assert_eq!(symbol.at.rot, 90.0);
    assert_eq!(symbol.mirror, Mirror::X);
    assert_eq!(result["changed"]["labels_added"], 0);
}

#[test]
fn moving_a_decoupling_cap_beside_its_ic_pin_leaves_two_straight_segments() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: KiCad 10 not configured");
        return;
    };
    let mut doc = SchDoc::read(ctx.sch_path()).unwrap();
    let symbols = source(&ctx);
    doc.add_symbol(
        "Regulator_Linear:LM7805_TO220",
        "U1",
        "LM7805",
        Pose::new(80.0, 80.0, 0.0),
        &symbols,
    )
    .unwrap();
    doc.add_symbol(
        "Device:C",
        "C1",
        "100nF",
        Pose::new(140.0, 120.0, 0.0),
        &symbols,
    )
    .unwrap();

    let pins = placed_pins(&doc);
    let ic_pin = pins
        .iter()
        .find(|pin| pin.refdes == "U1" && pin.number == "3")
        .expect("regulator output pin")
        .at;
    let old_cap: Vec<_> = pins
        .iter()
        .filter(|pin| pin.refdes == "C1")
        .cloned()
        .collect();

    let mut posed = doc.clone();
    posed
        .set_symbol_orientation("C1", 90.0, Mirror::None)
        .unwrap();
    let turned_pin = placed_pins(&posed)
        .into_iter()
        .find(|pin| pin.refdes == "C1" && pin.number == "1")
        .unwrap();
    let origin = posed.symbol_by_ref("C1").unwrap().at.point();
    let target = Point2::new(
        origin.x + ic_pin.x + 15.24 - turned_pin.at.x,
        origin.y + ic_pin.y - turned_pin.at.y,
    );
    posed.move_symbol("C1", target.x, target.y).unwrap();
    let target_pins = placed_pins(&posed);
    let target_ground = target_pins
        .iter()
        .find(|pin| pin.refdes == "C1" && pin.number == "2")
        .unwrap();
    let ground_anchor = Point2::new(
        target_ground.at.x + target_ground.out.x * 12.7,
        target_ground.at.y + target_ground.out.y * 12.7,
    );

    wire_l(
        &mut doc,
        old_cap.iter().find(|pin| pin.number == "1").unwrap().at,
        ic_pin,
    );
    wire_l(
        &mut doc,
        old_cap.iter().find(|pin| pin.number == "2").unwrap().at,
        ground_anchor,
    );
    doc.add_label(
        LabelKind::Local,
        "GND",
        Pose::new(ground_anchor.x, ground_anchor.y, 0.0),
    );
    doc.write(ctx.sch_path()).unwrap();
    let before = ctx.env().netlist(ctx.sch_path()).unwrap();

    let result = call(
        &ctx,
        json!({"moves": [{"ref": "C1", "to": [target.x, target.y], "rot": 90}]}),
    );
    assert!(result.get("error").is_none(), "{result:#}");

    let after_doc = SchDoc::read(ctx.sch_path()).unwrap();
    let after = ctx.env().netlist(ctx.sch_path()).unwrap();
    assert_eq!(partition(&after), partition(&before));
    assert_pins_drawn(&after_doc, "C1");

    let cap_pins: Vec<Point2> = placed_pins(&after_doc)
        .into_iter()
        .filter(|pin| pin.refdes == "C1")
        .map(|pin| pin.at)
        .collect();
    let attached: Vec<_> = after_doc
        .wires()
        .flat_map(|wire| wire.points.windows(2))
        .filter(|ends| {
            cap_pins
                .iter()
                .any(|pin| geom::Segment::new(ends[0], ends[1]).contains_point(*pin))
        })
        .collect();
    assert_eq!(attached.len(), 2, "cap routes: {attached:?}");
    assert!(attached.iter().all(|ends| {
        (ends[0].x - ends[1].x).abs() < EPS || (ends[0].y - ends[1].y).abs() < EPS
    }));
}

#[test]
fn turn_in_place_swaps_only_its_fixed_pin_nets_while_drag_preserves_them() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: KiCad 10 not configured");
        return;
    };
    let mut doc = SchDoc::read(ctx.sch_path()).unwrap();
    doc.add_symbol(
        "Device:LED",
        "D1",
        "LED",
        Pose::new(100.0, 100.0, 0.0),
        &source(&ctx),
    )
    .unwrap();
    add_named_stubs(&mut doc, "D1");
    doc.write(ctx.sch_path()).unwrap();
    let before = ctx.env().netlist(ctx.sch_path()).unwrap();
    assert_eq!(net_of(&before, "D1", "1"), Some("SIG_1"));
    assert_eq!(net_of(&before, "D1", "2"), Some("SIG_2"));

    let turned = call(&ctx, json!({"moves": [{"ref": "D1", "rot": 180}]}));
    assert!(turned.get("error").is_none(), "{turned:#}");
    assert_eq!(turned["changed"]["moved"][0]["turned_in_place"], true);
    assert_eq!(
        turned["changed"]["moved"][0]["pins_swapped"],
        json!([["1", "SIG_1"], ["2", "SIG_2"]])
    );
    assert_eq!(turned["changed"]["labels_added"], 0);
    assert!(turned["changed"]["moved"][0].get("nudged_to").is_none());
    let after_turn = ctx.env().netlist(ctx.sch_path()).unwrap();
    assert_eq!(net_of(&after_turn, "D1", "1"), Some("SIG_2"));
    assert_eq!(net_of(&after_turn, "D1", "2"), Some("SIG_1"));

    let dragged = call(
        &ctx,
        json!({"moves": [{"ref": "D1", "by": [20.32, 0.0], "rot": 0}]}),
    );
    assert!(dragged.get("error").is_none(), "{dragged:#}");
    assert!(
        dragged["changed"]["moved"][0]
            .get("turned_in_place")
            .is_none()
    );
    let after_drag = ctx.env().netlist(ctx.sch_path()).unwrap();
    assert_eq!(net_of(&after_drag, "D1", "1"), Some("SIG_2"));
    assert_eq!(net_of(&after_drag, "D1", "2"), Some("SIG_1"));
}

#[test]
fn explicit_turn_in_place_names_the_pin_offset_when_geometry_cannot_land() {
    let Some(ctx) = sheet() else {
        eprintln!("SKIP: KiCad 10 not configured");
        return;
    };
    let mut doc = SchDoc::read(ctx.sch_path()).unwrap();
    doc.add_symbol(
        "Connector_Generic:Conn_01x04",
        "J1",
        "Conn_01x04",
        Pose::new(100.0, 100.0, 0.0),
        &source(&ctx),
    )
    .unwrap();
    doc.write(ctx.sch_path()).unwrap();

    let refused = call(
        &ctx,
        json!({"moves": [{"ref": "J1", "turn_in_place": true, "rot": 90}]}),
    );
    let error = refused["error"].as_str().expect("turn must be refused");
    assert!(
        error.contains("pin ") && error.contains("offset ["),
        "{error}"
    );
    let unchanged = SchDoc::read(ctx.sch_path()).unwrap();
    assert_eq!(unchanged.symbol_by_ref("J1").unwrap().at.rot, 0.0);
}
