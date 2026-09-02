//! Adversarial fixtures for the drag primitive and the evaluator.
//!
//! Every sheet here is written out by hand, so what the assertions are about is
//! visible in the test rather than hidden in a binary corpus.

use geom::Point2;
use sch_doc::{Mirror, SchDoc};
use sch_drag::drag::{DragError, Placement, drag, partition};
use sch_drag::{Sheet, measure};

const ROOT: &str = "00000000-0000-0000-0000-000000000001";

/// A two-pin part whose pins sit 3.81 mm above and below its origin.
fn resistor_definition() -> String {
    r#"(symbol "Device:R"
			(pin_numbers (hide yes))
			(pin_names (offset 0))
			(exclude_from_sim no) (in_bom yes) (on_board yes)
			(symbol "R_0_1"
				(rectangle (start -1.016 -2.54) (end 1.016 2.54)
					(stroke (width 0.254) (type default)) (fill (type none))))
			(symbol "R_1_1"
				(pin passive line (at 0 3.81 270) (length 1.27)
					(name "~" (effects (font (size 1.27 1.27))))
					(number "1" (effects (font (size 1.27 1.27)))))
				(pin passive line (at 0 -3.81 90) (length 1.27)
					(name "~" (effects (font (size 1.27 1.27))))
					(number "2" (effects (font (size 1.27 1.27)))))))"#
        .to_string()
}

/// A one-pin power symbol, the thing that gets welded onto a pin.
fn power_definition() -> String {
    r#"(symbol "power:GND"
			(power) (pin_numbers (hide yes)) (pin_names (offset 0))
			(exclude_from_sim no) (in_bom no) (on_board yes)
			(symbol "GND_0_1"
				(polyline (pts (xy -1.27 -1.27) (xy 0 -2.54) (xy 1.27 -1.27))
					(stroke (width 0) (type default)) (fill (type none))))
			(symbol "GND_1_1"
				(pin power_in line (at 0 0 90) (length 0) (hide yes)
					(name "GND" (effects (font (size 1.27 1.27))))
					(number "1" (effects (font (size 1.27 1.27)))))))"#
        .to_string()
}

fn symbol(lib: &str, refdes: &str, uuid: &str, at: Point2, rot: f64, mirror: &str) -> String {
    let mirror = match mirror {
        "" => String::new(),
        axis => format!("(mirror {axis})"),
    };
    format!(
        r#"	(symbol (lib_id "{lib}") (at {} {} {rot}) {mirror} (unit 1)
		(exclude_from_sim no) (in_bom yes) (on_board yes) (dnp no)
		(uuid "{uuid}")
		(property "Reference" "{refdes}" (at {} {} 0) (effects (font (size 1.27 1.27))))
		(property "Value" "x" (at {} {} 0) (effects (font (size 1.27 1.27)) (hide yes)))
		(pin "1" (uuid "{uuid}-p1")) (pin "2" (uuid "{uuid}-p2"))
		(instances (project "t" (path "/{ROOT}" (reference "{refdes}") (unit 1)))))"#,
        at.x,
        at.y,
        at.x + 2.54,
        at.y,
        at.x + 2.54,
        at.y + 2.54,
    )
}

fn wire(a: Point2, b: Point2, uuid: &str) -> String {
    format!(
        "\t(wire (pts (xy {} {}) (xy {} {})) (stroke (width 0) (type default)) (uuid \"{uuid}\"))",
        a.x, a.y, b.x, b.y
    )
}

fn sheet(definitions: &[String], body: &[String]) -> SchDoc {
    let text = format!(
        "(kicad_sch\n\t(version 20231120)\n\t(generator \"test\")\n\t(uuid \"{ROOT}\")\n\t(paper \"A4\")\n\t(lib_symbols\n\t\t{}\n\t)\n{}\n)\n",
        definitions.join("\n\t\t"),
        body.join("\n")
    );
    SchDoc::parse(&text).expect("fixture parses")
}

fn at(x: f64, y: f64) -> Point2 {
    Point2::new(x, y)
}

/// Two resistors in series, wired pin 2 of R1 down to pin 1 of R2.
fn series_pair() -> SchDoc {
    sheet(
        &[resistor_definition()],
        &[
            symbol("Device:R", "R1", "r1", at(100.0, 100.0), 0.0, ""),
            symbol("Device:R", "R2", "r2", at(100.0, 120.0), 0.0, ""),
            wire(at(100.0, 103.81), at(100.0, 116.19), "w1"),
        ],
    )
}

#[test]
fn a_dragged_pin_keeps_its_net() {
    let mut doc = series_pair();
    let before = partition(&Sheet::of(&doc));
    let report = drag(
        &mut doc,
        "R1",
        Placement::new(at(120.0, 100.0), 0.0, Mirror::None),
    )
    .expect("the sheet has room");

    assert_eq!(partition(&Sheet::of(&doc)), before);
    assert!(report.redrawn_segments > 0, "the wire has to be re-drawn");
    assert!(report.labels_added == 0, "a clear sheet needs no label");
    let sheet = Sheet::of(&doc);
    assert!(
        sheet.wires.iter().all(|w| w.horizontal() || w.vertical()),
        "every re-drawn segment is orthogonal"
    );
    assert_eq!(measure(&sheet).faults(), 0);
}

#[test]
fn rotating_re_derives_the_pins_and_keeps_the_net() {
    let mut doc = series_pair();
    let before = partition(&Sheet::of(&doc));
    drag(
        &mut doc,
        "R1",
        Placement::new(at(100.0, 100.0), 90.0, Mirror::None),
    )
    .expect("rotation in place");

    let sheet = Sheet::of(&doc);
    assert_eq!(partition(&sheet), before);
    let pins: Vec<Point2> = sheet
        .pins
        .iter()
        .filter(|p| p.refdes == "R1")
        .map(|p| p.at)
        .collect();
    assert!(
        pins.iter().all(|p| (p.y - 100.0).abs() < 1e-6),
        "a quarter turn lays the part on its side: {pins:?}"
    );
}

#[test]
fn mirroring_applies_after_rotation() {
    let mut doc = series_pair();
    drag(
        &mut doc,
        "R1",
        Placement::new(at(100.0, 100.0), 90.0, Mirror::X),
    )
    .expect("mirrored quarter turn");
    let sheet = Sheet::of(&doc);
    let pin1 = sheet
        .pins
        .iter()
        .find(|p| p.refdes == "R1" && p.number == "1")
        .expect("R1 pin 1");
    // `(mirror x)` reflects the *sheet* offset, so on a part already turned a
    // quarter it leaves an offset along x where negating the local one would not.
    assert!(
        (pin1.at.x - 96.19).abs() < 1e-6 && (pin1.at.y - 100.0).abs() < 1e-6,
        "pin 1 landed at {:?}",
        pin1.at
    );
}

#[test]
fn a_welded_power_flag_travels_with_the_pin() {
    let mut doc = sheet(
        &[resistor_definition(), power_definition()],
        &[
            symbol("Device:R", "R1", "r1", at(100.0, 100.0), 0.0, ""),
            symbol("power:GND", "#PWR01", "g1", at(100.0, 103.81), 0.0, ""),
        ],
    );
    let before = partition(&Sheet::of(&doc));
    assert!(
        before.iter().any(|net| net.contains(&"R1.2".to_string())),
        "the fixture starts with R1 pin 2 on the ground symbol"
    );

    drag(
        &mut doc,
        "R1",
        Placement::new(at(140.0, 130.0), 0.0, Mirror::None),
    )
    .expect("room to move");
    assert_eq!(partition(&Sheet::of(&doc)), before);
    let flag = doc
        .symbol_by_ref("#PWR01")
        .expect("the flag is still there");
    assert_eq!(
        (flag.at.x, flag.at.y),
        (140.0, 133.81),
        "the flag rode along with the pin"
    );
}

#[test]
fn a_label_sitting_on_a_pin_rides_along() {
    let mut doc = sheet(
        &[resistor_definition()],
        &[
            symbol("Device:R", "R1", "r1", at(100.0, 100.0), 0.0, ""),
            symbol("Device:R", "R2", "r2", at(140.0, 100.0), 0.0, ""),
            r#"	(label "SIG" (at 100 96.19 0) (effects (font (size 1.27 1.27))) (uuid "l1"))"#
                .to_string(),
            r#"	(label "SIG" (at 140 96.19 0) (effects (font (size 1.27 1.27))) (uuid "l2"))"#
                .to_string(),
        ],
    );
    let before = partition(&Sheet::of(&doc));
    drag(
        &mut doc,
        "R1",
        Placement::new(at(100.0, 140.0), 0.0, Mirror::None),
    )
    .expect("room to move");

    assert_eq!(partition(&Sheet::of(&doc)), before);
    let label = doc.labels().find(|l| l.uuid == "l1").expect("label kept");
    assert_eq!((label.at.x, label.at.y), (100.0, 136.19));
}

#[test]
fn a_move_that_would_short_two_nets_is_refused_and_rolled_back() {
    let mut doc = sheet(
        &[resistor_definition()],
        &[
            symbol("Device:R", "R1", "r1", at(100.0, 100.0), 0.0, ""),
            symbol("Device:R", "R2", "r2", at(140.0, 100.0), 0.0, ""),
            symbol("Device:R", "R3", "r3", at(180.0, 100.0), 0.0, ""),
            wire(at(140.0, 96.19), at(180.0, 96.19), "w1"),
        ],
    );
    let text = doc.to_text();
    let before = partition(&Sheet::of(&doc));

    // Landing R1's own pin on R2's would join two nets that were apart.
    let outcome = drag(
        &mut doc,
        "R1",
        Placement::new(at(140.0, 100.0), 0.0, Mirror::None),
    );
    assert!(
        matches!(
            outcome,
            Err(DragError::Truthfulness(_) | DragError::Disconnection(_))
        ),
        "expected a refusal, got {outcome:?}"
    );
    assert_eq!(partition(&Sheet::of(&doc)), before);
    assert_eq!(doc.to_text(), text, "a refused drag changes nothing");
}

#[test]
fn a_dot_that_connects_nothing_is_taken_away() {
    let doc = series_pair();
    let junction =
        r#"	(junction (at 100 110) (diameter 0) (color 0 0 0 0) (uuid "j1"))"#.to_string();
    let text = doc.to_text().replace("\n)", &format!("\n{junction}\n)"));
    let mut doc = SchDoc::parse(&text).expect("fixture with a stray dot");
    assert_eq!(Sheet::of(&doc).junctions.len(), 1);

    drag(
        &mut doc,
        "R1",
        Placement::new(at(100.0, 90.0), 0.0, Mirror::None),
    )
    .expect("room to move");
    let sheet = Sheet::of(&doc);
    assert_eq!(sheet.junction_faults(), 0, "no dot is left connecting air");
}

#[test]
fn the_evaluator_sees_what_a_reader_sees() {
    let doc = series_pair();
    let clean = measure(&Sheet::of(&doc));
    assert_eq!(clean.faults(), 0);
    assert_eq!(clean.crossings, 0);
    assert_eq!(clean.dangling_ends, 0);
    assert!(clean.wire_length > 12.0 && clean.wire_length < 13.0);

    // A wire ruled straight across R2's body, ending on nothing.
    let text = doc.to_text().replace(
        "\n)",
        &format!("\n{}\n)", wire(at(90.0, 120.0), at(110.0, 120.0), "w9")),
    );
    let dirty = measure(&Sheet::of(&SchDoc::parse(&text).expect("parses")));
    assert_eq!(dirty.through_bodies, 1);
    assert_eq!(dirty.dangling_ends, 2);
    assert!(dirty.faults() > clean.faults());
}

#[test]
fn measuring_a_sheet_is_fast_enough_to_search_on() {
    let mut body = vec![];
    for i in 0..90 {
        let (x, y) = (30.0 + (i % 10) as f64 * 25.0, 30.0 + (i / 10) as f64 * 20.0);
        body.push(symbol(
            "Device:R",
            &format!("R{i}"),
            &format!("r{i}"),
            at(x, y),
            0.0,
            "",
        ));
        body.push(wire(at(x, y + 3.81), at(x, y + 10.0), &format!("w{i}")));
    }
    let doc = sheet(&[resistor_definition()], &body);
    let sheet = Sheet::of(&doc);
    assert_eq!(sheet.bodies.len(), 90);

    let start = std::time::Instant::now();
    for _ in 0..20 {
        std::hint::black_box(measure(&sheet));
    }
    let each = start.elapsed() / 20;
    assert!(
        each < std::time::Duration::from_millis(5),
        "measuring 90 symbols took {each:?}"
    );
}
