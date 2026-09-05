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
        text: Default::default(),
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
        supports: None,
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

/// A three-terminal `Device:` discrete — a potentiometer's two ends and its wiper.
fn pot(refdes: &str, nets: [&str; 3]) -> Item {
    let mut it = item(
        refdes,
        vec![
            pin("1", "1", 0.0, 3.81, 270.0),
            pin("2", "W", -3.81, 0.0, 0.0),
            pin("3", "3", 0.0, -3.81, 90.0),
        ],
        &[("1", nets[0]), ("2", nets[1]), ("3", nets[2])],
    );
    it.part = "Device:R_Potentiometer".into();
    it
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
    sch_flex::typeset(&mut items, &trees(tree), &[]);
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
        vec![stack(Axis::Col, vec![leaf("R1"), leaf("R2")]), leaf("U1")],
    );
    sch_flex::typeset(&mut items, &trees(tree), &[]);
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
    let report = sch_flex::typeset(&mut items, &Trees::new(), &[]);
    assert_eq!(report.untreed, ["b"]);
    assert!(report.warnings()[0].contains("bare row"), "{report:?}");
    assert_ne!(items[0].at, items[1].at);
}

/// A tree that forgets a part draws it anyway, under a row of its own, and names it.
#[test]
fn a_part_left_out_of_the_tree_is_drawn_and_named() {
    let mut items = vec![passive("R1", "A", "B"), passive("R2", "B", "C")];
    let report = sch_flex::typeset(&mut items, &trees(leaf("R1")), &[]);
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
    let report = sch_flex::typeset(&mut items, &Trees::new(), &[]);
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
    sch_flex::typeset(&mut both, &trees(tree.clone()), &[]);
    let mut only = vec![passive("R1", "A", "B"), passive("R2", "B", "C")];
    sch_flex::typeset(
        &mut only,
        &trees(stack(Axis::Row, vec![leaf("R1"), leaf("R2")])),
        &[],
    );
    assert_eq!(
        (both[1].at.x - both[0].at.x, both[0].at),
        (only[1].at.x - only[0].at.x, only[0].at),
        "the absent leaves widened the row"
    );
}

/// A `decouple` cap is synthesized after the author composed the block, so the author
/// could not have named it. The bank is drawn as one row beside the part it supports,
/// inside that part's own group — not in the leftovers row at the bottom of the block. It
/// touches no signal pin, so no seating of it makes a wire; it takes the shape that costs
/// the drawing least.
#[test]
fn a_synthesized_decoupler_is_seated_beside_the_part_it_supports() {
    let cap = |refdes: &str| Item {
        supports: Some("U1".into()),
        ..passive(refdes, "+3V3", "GND")
    };
    let mut items = vec![
        passive("R1", "A", "GND"),
        ic("U1", ["A", "+3V3", "GND"]),
        passive("R2", "B", "GND"),
        cap("C1"),
        cap("C2"),
        cap("C3"),
    ];
    let tree = stack(Axis::Row, vec![leaf("R1"), leaf("U1"), leaf("R2")]);
    let report = sch_flex::typeset(&mut items, &trees(tree), &[]);
    assert!(report.uncomposed.is_empty(), "{report:?}");
    let at = |refdes: &str| items.iter().find(|i| i.refdes == refdes).unwrap().at;
    let (u1, c1, c2, c3, r2) = (at("U1"), at("C1"), at("C2"), at("C3"), at("R2"));
    assert_eq!((c1.y, c2.y), (c3.y, c3.y), "the caps are not on one line");
    assert!(
        u1.x < c1.x && c1.x < c2.x && c2.x < c3.x && c3.x < r2.x,
        "the bank does not sit between U1 and the rest of the row"
    );
    assert!(
        ((c2.x - c1.x) - (c3.x - c2.x)).abs() < 1e-6,
        "the bank is not evenly pitched"
    );
}

/// A part the AUTHOR left out is still their own gap: it goes in the trailing row with the
/// note that says so. Only a synthesized part is seated silently.
#[test]
fn a_part_the_author_forgot_is_still_reported() {
    let mut items = vec![ic("U1", ["A", "+3V3", "GND"]), passive("C1", "+3V3", "GND")];
    let report = sch_flex::typeset(&mut items, &trees(stack(Axis::Row, vec![leaf("U1")])), &[]);
    assert_eq!(report.uncomposed["b"], ["C1"]);
}

/// A synthesized cap whose parent the author ALSO left out has no slot to be seated
/// beside, and a tree that is one bare leaf has no slot at all. It still has to be DRAWN:
/// a part missing from the tree is never placed, and a stack of symbols at the origin
/// renders as one part and extracts as none.
#[test]
fn a_decoupler_with_nowhere_to_sit_is_still_drawn() {
    let mut items = vec![
        passive("R1", "A", "GND"),
        Item {
            supports: Some("U9".into()),
            ..passive("C1", "+3V3", "GND")
        },
    ];
    sch_flex::typeset(&mut items, &trees(leaf("R1")), &[]);
    assert_ne!(items[0].at, items[1].at, "C1 was never placed");
}

/// A label is TEXT, not width. The pitch of a bank of like parts is set by their bodies,
/// so writing a long net name beside one of them must not push its neighbours apart —
/// the label hangs into the white space that was already there.
///
/// This is the whole point of measuring a part's box without its overhang: a column that
/// pairs a bare capacitor with a labelled one used to inflate to the label's width, and
/// every alignment the typesetter bought was paid for in ragged pitch and a bigger page.
#[test]
fn a_long_label_beside_one_part_does_not_widen_the_bank() {
    let row = || stack(Axis::Row, vec![leaf("R1"), leaf("R2"), leaf("R3")]);
    let pitch = |nets: [&str; 3]| {
        let mut items: Vec<Item> = ["R1", "R2", "R3"]
            .iter()
            .zip(nets)
            .map(|(refdes, net)| passive(refdes, net, "GND"))
            .collect();
        // A second pin somewhere else makes each net leave the block, so it is labelled.
        items.push(passive("R4", nets[0], nets[1]));
        items.push(passive("R5", nets[2], "GND"));
        sch_flex::typeset(&mut items, &trees(row()), &[]);
        [items[1].at.x - items[0].at.x, items[2].at.x - items[1].at.x]
    };
    assert_eq!(
        pitch(["A", "B", "C"]),
        pitch(["A", "A_VERY_LONG_SIGNAL_NAME_INDEED", "C"]),
        "the middle part's label set the bank's pitch"
    );
}

/// A row too long for its page folds into stacked bands, and the k-th part of every band
/// then stands on ONE vertical line — the column alignment a person draws repeated
/// channels with, and the thing our sheets most visibly lacked.
#[test]
fn wrapped_bands_stand_on_shared_column_lines() {
    let parts: Vec<String> = (1..=8).map(|i| format!("R{i}")).collect();
    let mut items: Vec<Item> = parts
        .iter()
        .map(|refdes| passive(refdes, "VCC", "GND"))
        .collect();
    let tree = Tree::Container(Container {
        axis: Axis::Row,
        children: parts.iter().map(|p| leaf(p)).collect(),
        gap: None,
        align: Align::Center,
        // Four of these to a band, so the fold is two bands of four.
        wrap: Some(22.0),
    });
    sch_flex::typeset(&mut items, &trees(tree), &[]);
    let x = |i: usize| items[i].at.x;
    let y = |i: usize| items[i].at.y;
    assert!(y(0) < y(4), "expected two bands, got one row");
    for k in 0..4 {
        assert_eq!(x(k), x(k + 4), "column {k} is not one line");
    }
}

/// The defect a reader names as "sibling orientation": a pull-up and a pull-down are the
/// same part playing the same role — a leg off a rail — so they must stand the same way on
/// the same baseline, however differently their rails are named. The engine used to stand
/// only the pull-down and lay the pull-up along the row.
#[test]
fn a_pull_up_and_a_pull_down_in_one_row_stand_alike() {
    let mut items = vec![
        passive("R1", "3V3", "SCL"),
        passive("R2", "SDA", "GND"),
        passive("R3", "3V3", "NRST"),
    ];
    let tree = stack(Axis::Row, vec![leaf("R1"), leaf("R2"), leaf("R3")]);
    sch_flex::typeset(&mut items, &trees(tree), &[]);
    let angles: Vec<f64> = items.iter().map(|it| it.angle).collect();
    let ys: Vec<f64> = items.iter().map(|it| it.at.y).collect();
    assert!(
        angles.windows(2).all(|w| w[0] == w[1]),
        "siblings off one rail took different angles: {angles:?}"
    );
    assert!(
        ys.windows(2).all(|w| (w[0] - w[1]).abs() < 1e-9),
        "siblings off one rail took different baselines: {ys:?}"
    );
    assert!(
        angles[0] % 180.0 == 0.0,
        "a leg off a rail stands: {angles:?}"
    );
}

/// The same three, with their signals now reaching an IC in the same row. An IC pin is
/// where a leg ENDS, so the row still stands as one. Reading the block's LABEL assignment
/// instead of its topology gets this wrong: a pull-up wired to the MCU beside it carries
/// no label, and used to lie back down while its pull-down sibling stood.
#[test]
fn a_pull_up_wired_to_an_ic_keeps_ranks() {
    let mut items = vec![
        passive("R1", "3V3", "SCL"),
        passive("R2", "SDA", "GND"),
        passive("R3", "3V3", "NRST"),
        ic("U1", ["SCL", "SDA", "NRST"]),
    ];
    let tree = stack(
        Axis::Row,
        vec![leaf("R1"), leaf("R2"), leaf("R3"), leaf("U1")],
    );
    sch_flex::typeset(&mut items, &trees(tree), &[]);
    let angles: Vec<f64> = items[..3].iter().map(|it| it.angle).collect();
    assert!(
        angles.iter().all(|a| a % 180.0 == 0.0),
        "an IC pin is a leg's far end, not a chain: {angles:?}"
    );
}

/// The limit of the rule, and why it is not "everything touching a rail stands": a
/// divider's top resistor also touches a rail, but its far pin CONTINUES to the resistor
/// beside it. Standing it would bend that wire back around its own body, so it lies along
/// the chain it is a link of.
#[test]
fn a_series_element_that_touches_a_rail_lies_along_its_chain() {
    let mut items = vec![passive("R1", "VCC", "OUT"), passive("R2", "OUT", "GND")];
    let tree = stack(Axis::Row, vec![leaf("R1"), leaf("R2")]);
    sch_flex::typeset(&mut items, &trees(tree), &[]);
    assert_eq!(
        items[0].angle % 180.0,
        90.0,
        "the top of a divider stood up"
    );
}

/// A potentiometer has three pins and is still a link of the chain, not a place a leg
/// ends: the resistor feeding its top from a rail lies along the divider it is half of.
#[test]
fn a_resistor_in_series_with_a_potentiometer_lies_along_it() {
    let mut items = vec![
        passive("R2", "VCC", "ADJ"),
        pot("RV1", ["ADJ", "OUT", "GND"]),
    ];
    let tree = stack(Axis::Row, vec![leaf("R2"), leaf("RV1")]);
    sch_flex::typeset(&mut items, &trees(tree), &[]);
    assert_eq!(
        items[0].angle % 180.0,
        90.0,
        "a resistor feeding a pot stood up"
    );
}

/// A support part the author left out is drawn beside the pin it serves, not in a row of
/// strangers: the wire between them is a few millimetres and never needs a name.
#[test]
fn a_support_part_left_out_is_seated_beside_the_pin_it_serves() {
    let mut items = vec![
        ic("U1", ["A", "B", "GND"]),
        passive("R9", "B", "VCC"),
        passive("R1", "IN", "OUT"),
        passive("R2", "OUT", "GND"),
    ];
    let tree = stack(Axis::Row, vec![leaf("R1"), leaf("R2"), leaf("U1")]);
    sch_flex::typeset(&mut items, &trees(tree), &[]);
    let at = |refdes: &str| items.iter().find(|i| i.refdes == refdes).unwrap().at;
    // U1's pin 2 carries B and so does R9's top pin; both are west-facing, so R9 sits one
    // gap to the left of the IC with its pin on that pin's line.
    let (r9, u1) = (at("R9"), at("U1"));
    assert!(r9.x < u1.x, "R9 at {r9:?} is not on U1's pin side {u1:?}");
    assert!(
        r9.dist(u1) < 30.0,
        "R9 at {r9:?} is a page from the U1 pin it serves at {u1:?}"
    );
}

/// The same, for a block whose author composed nothing at all: a bare row is still not a
/// composition, but the parts that plainly serve a pin are drawn beside it.
#[test]
fn a_bare_row_still_seats_supports_beside_what_they_serve() {
    let mut items = vec![
        passive("R1", "IN", "OUT"),
        ic("U1", ["A", "B", "GND"]),
        passive("C1", "A", "GND"),
    ];
    let report = sch_flex::typeset(&mut items, &Trees::new(), &[]);
    assert_eq!(report.untreed, ["b"]);
    let at = |refdes: &str| items.iter().find(|i| i.refdes == refdes).unwrap().at;
    assert!(
        at("C1").dist(at("U1")) < at("R1").dist(at("U1")),
        "C1 is further from the pin it serves than a part that serves nothing"
    );
}
