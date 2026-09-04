//! What the typesetter must never get wrong, over synthetic symbols: every part lands on
//! the lattice its pins connect on, a column beside an IC really sits on the IC's pin
//! lines, and no tree — however badly composed — loses a part or fails to terminate.

use geom::Point2;
use kicad_symbol::geometry::{PinGeom, SymbolGeometry};
use sch_model::item::Item;
use sch_model::tree::{Align, Axis, Container, Leaf, Tree, Trees};

/// A pin at `(x, y)` in symbol space (Y up) pointing outward along `angle`.
fn pin(number: &str, name: &str, x: f64, y: f64, angle: f64) -> PinGeom {
    PinGeom {
        number: number.into(),
        name: name.into(),
        at: Point2::new(x, y),
        angle,
        length: 2.54,
        unit: 1,
    }
}

fn item(refdes: &str, pins: Vec<PinGeom>, nets: &[(&str, &str)]) -> Item {
    Item {
        refdes: refdes.into(),
        block: "b".into(),
        part: if pins.len() > 2 { "Amp:U" } else { "Device:R" }.into(),
        value: "10k".into(),
        footprint: None,
        geom: SymbolGeometry {
            lib_id: "Device:R".into(),
            pins,
            raw_definition: String::new(),
        },
        pins: nets
            .iter()
            .map(|(number, net)| {
                (
                    (*number).to_string(),
                    String::new(),
                    Some((*net).to_string()),
                )
            })
            .collect(),
        at: Point2::new(0.0, 0.0),
        angle: 0.0,
        unit: 1,
        mirror: false,
        preseeded: false,
    }
}

/// A 2-pin part drawn vertically, as `Device:R` is.
fn passive(refdes: &str, top: &str, bottom: &str) -> Item {
    item(
        refdes,
        vec![
            pin("1", "~", 0.0, 3.81, 270.0),
            pin("2", "~", 0.0, -3.81, 90.0),
        ],
        &[("1", top), ("2", bottom)],
    )
}

/// A three-pin IC with all three pins on its west edge, one 50-mil step apart. A west-edge
/// pin is drawn at angle 0 — pointing right, INTO the body — so it faces west outward.
fn ic(refdes: &str, nets: [&str; 3]) -> Item {
    item(
        refdes,
        vec![
            pin("1", "A", -5.08, 1.27, 0.0),
            pin("2", "B", -5.08, -1.27, 0.0),
            pin("3", "C", -5.08, -3.81, 0.0),
        ],
        &[("1", nets[0]), ("2", nets[1]), ("3", nets[2])],
    )
}

fn leaf(part: &str) -> Tree {
    Tree::Leaf(Leaf {
        part: part.into(),
        ..Leaf::default()
    })
}

fn stack(axis: Axis, children: Vec<Tree>) -> Tree {
    Tree::Container(Container {
        axis,
        children,
        gap: None,
        align: Align::Center,
        wrap: None,
    })
}

fn trees(tree: Tree) -> Trees {
    [("b".to_string(), tree)].into_iter().collect()
}

/// The invariant a whole class of silent-open bugs hides behind: a sheet whose symbols are
/// a fraction of a millimetre off the wire lattice renders perfectly and extracts an empty
/// netlist. Every pose the typesetter writes must be on the 50-mil grid.
#[test]
fn every_part_lands_on_the_pin_lattice() {
    let mut items = vec![
        passive("R1", "IN", "MID"),
        passive("R2", "MID", "GND"),
        ic("U1", ["MID", "OUT", "GND"]),
        passive("C1", "OUT", "GND"),
    ];
    let tree = stack(
        Axis::Row,
        vec![
            stack(Axis::Col, vec![leaf("R1"), leaf("R2")]),
            leaf("U1"),
            leaf("C1"),
        ],
    );
    sch_flex::typeset(&mut items, &trees(tree));
    for it in &items {
        let on = geom::GRID_50_MIL.snap_point(it.at);
        assert!(
            (on.x - it.at.x).abs() < 1e-9 && (on.y - it.at.y).abs() < 1e-9,
            "{} is off the pin lattice at {:?}",
            it.refdes,
            it.at
        );
    }
}

/// A column beside an IC exists to put a child's own connecting PIN on the line of the IC
/// pin it wires to — not the child's origin, which for a standing passive is half a body
/// away. Children that cannot all fit on their lines keep their order and their clearance;
/// the first one on a line is what the seat is measured by.
#[test]
fn a_column_beside_an_ic_seats_its_pin_on_the_ic_pin_line() {
    let mut items = vec![
        passive("R1", "A", "GND"),
        passive("R2", "B", "GND"),
        ic("U1", ["A", "B", "GND"]),
    ];
    let tree = stack(
        Axis::Row,
        vec![
            stack(Axis::Col, vec![leaf("R1"), leaf("R2")]),
            leaf("U1"),
        ],
    );
    sch_flex::typeset(&mut items, &trees(tree));
    let at = |refdes: &str| items.iter().find(|i| i.refdes == refdes).unwrap().at;
    // R1's top pin carries net A; so does the IC's pin 1. Both are 3.81 / 1.27 mm above
    // their own origin in sheet space, so the wire between them is a straight run.
    let (r1, u1) = (at("R1"), at("U1"));
    assert!(
        ((r1.y - 3.81) - (u1.y - 1.27)).abs() < 1e-6,
        "R1's A pin at {} and U1's A pin at {} are not on one line",
        r1.y - 3.81,
        u1.y - 1.27
    );
}

/// A block whose author composed nothing still gets every part drawn, and says so.
#[test]
fn an_uncomposed_block_is_drawn_as_a_row_and_reported() {
    let mut items = vec![passive("R1", "A", "B"), passive("R2", "B", "C")];
    let report = sch_flex::typeset(&mut items, &Trees::new());
    assert_eq!(report.untreed, ["b"]);
    assert!(report.warnings()[0].contains("bare row"), "{report:?}");
    assert_ne!(items[0].at, items[1].at);
}

/// A tree that forgets a part draws it anyway, under a row of its own, and names it.
#[test]
fn a_part_left_out_of_the_tree_is_drawn_and_named() {
    let mut items = vec![passive("R1", "A", "B"), passive("R2", "B", "C")];
    let report = sch_flex::typeset(&mut items, &trees(leaf("R1")));
    assert_eq!(report.uncomposed["b"], ["R2"]);
    assert!(report.warnings()[0].contains("leaves out R2"), "{report:?}");
    assert_ne!(items[0].at, items[1].at);
}

/// A container that overflows the wrap limit on BOTH axes used to wrap forever, flipping
/// axis each time, and took the process down with a stack overflow rather than a refusal.
#[test]
fn a_block_too_big_for_any_page_still_terminates_with_every_part_placed() {
    let mut items: Vec<Item> = (0..120)
        .map(|i| passive(&format!("R{i}"), "A", "GND"))
        .collect();
    let report = sch_flex::typeset(&mut items, &Trees::new());
    assert_eq!(report.untreed, ["b"]);
    let seats: std::collections::BTreeSet<(i64, i64)> = items
        .iter()
        .map(|it| ((it.at.x * 100.0) as i64, (it.at.y * 100.0) as i64))
        .collect();
    assert_eq!(seats.len(), items.len(), "two parts share a seat");
}

/// `place_parts` appends, so a follow-up call states its block's whole tree while placing
/// only the new parts. The leaves for parts already on the sheet must leave no gap where
/// nothing is drawn.
#[test]
fn a_leaf_for_a_part_this_call_is_not_placing_leaves_no_hole() {
    let tree = stack(
        Axis::Row,
        vec![leaf("R0"), leaf("R1"), leaf("R9"), leaf("R2")],
    );
    let mut both = vec![passive("R1", "A", "B"), passive("R2", "B", "C")];
    sch_flex::typeset(&mut both, &trees(tree.clone()));
    let mut only = vec![passive("R1", "A", "B"), passive("R2", "B", "C")];
    sch_flex::typeset(&mut only, &trees(stack(Axis::Row, vec![leaf("R1"), leaf("R2")])));
    assert_eq!(
        (both[1].at.x - both[0].at.x, both[0].at),
        (only[1].at.x - only[0].at.x, only[0].at),
        "the absent leaves widened the row"
    );
}
