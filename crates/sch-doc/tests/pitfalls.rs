//! Gate 4 — one adversarial sheet per rule the extractor has to get right.
//!
//! Each fixture is the smallest schematic that distinguishes the rule from the
//! plausible wrong answer.

use sch_doc::{NetSource, SchDoc, connect};

const ROOT: &str = "00000000-0000-4000-8000-000000000001";

/// A two-pin passive: pin 1 at local `(0, 3.81)`, pin 2 at `(0, -3.81)`.
const RESISTOR: &str = r#"(symbol "Device:R"
    (symbol "R_1_1"
      (pin passive line (at 0 3.81 270) (length 1.27) (name "~") (number "1"))
      (pin passive line (at 0 -3.81 90) (length 1.27) (name "~") (number "2"))))"#;

/// A rail symbol: `(power)` plus one hidden power *input*.
const GROUND: &str = r#"(symbol "power:GND" (power)
    (symbol "GND_1_1"
      (pin power_in line (at 0 0 270) (length 0) (hide yes) (name "GND") (number "1"))))"#;

/// A flag symbol: `(power)` but its pin is a power *output*, so it names nothing.
const FLAG: &str = r#"(symbol "power:PWR_FLAG" (power)
    (symbol "PWR_FLAG_1_1"
      (pin power_out line (at 0 0 90) (length 0) (hide yes) (name "pwr_flag") (number "1"))))"#;

/// A pre-modern part that expects its supply pins to connect invisibly by name.
const LEGACY: &str = r#"(symbol "Legacy:U"
    (symbol "U_1_1"
      (pin passive line (at -5.08 0 0) (length 2.54) (name "IO") (number "1"))
      (pin power_in line (at 0 5.08 270) (length 0) (hide yes) (name "VDD") (number "8"))))"#;

/// Two gates sharing one supply pin, the classic multi-unit shape: unit 1 owns
/// pin 1, unit 2 owns pin 2, and unit 0 owns the pin every unit shares.
const DUAL: &str = r#"(symbol "Dual:OP"
    (symbol "OP_0_1"
      (pin power_in line (at 0 7.62 270) (length 0) (name "V+") (number "8")))
    (symbol "OP_1_1"
      (pin input line (at -5.08 0 0) (length 2.54) (name "A") (number "1")))
    (symbol "OP_2_1"
      (pin input line (at -5.08 0 0) (length 2.54) (name "B") (number "2"))))"#;

fn sheet(defs: &[&str], items: &str) -> SchDoc {
    let text = format!(
        "(kicad_sch (version 20250114) (generator \"test\") (uuid \"{ROOT}\") (paper \"A4\")\n\
         (lib_symbols {})\n{items})\n",
        defs.join("\n")
    );
    SchDoc::parse(&text).expect("fixture parses")
}

/// A placed symbol. `extra` carries whatever the case needs — `(mirror y)`,
/// `(dnp yes)`, `(unit 2)`.
fn place(lib_id: &str, refdes: &str, value: &str, x: f64, y: f64, rot: f64, extra: &str) -> String {
    format!(
        "(symbol (lib_id \"{lib_id}\") (at {x} {y} {rot}) {extra} (uuid \"{refdes}-uuid\")\n\
         (property \"Reference\" \"{refdes}\" (at {x} {y} 0))\n\
         (property \"Value\" \"{value}\" (at {x} {y} 0))\n\
         (instances (project \"t\" (path \"/{ROOT}\" (reference \"{refdes}\") (unit 1)))))"
    )
}

fn wire(x1: f64, y1: f64, x2: f64, y2: f64) -> String {
    format!("(wire (pts (xy {x1} {y1}) (xy {x2} {y2})) (uuid \"w-{x1}-{y1}-{x2}-{y2}\"))")
}

/// Nets as sorted pin lists, ignoring names.
fn nets(doc: &SchDoc) -> Vec<Vec<String>> {
    connect::extract(doc).partition()
}

fn net_named<'a>(netlist: &'a connect::Netlist, name: &str) -> &'a connect::Net {
    netlist
        .nets
        .iter()
        .find(|n| n.name == name)
        .unwrap_or_else(|| panic!("no net {name} in {:?}", netlist.nets))
}

/// R1's pin 1 sits partway along a wire. KiCAD needs no junction dot there.
#[test]
fn a_pin_inside_a_wire_joins_it() {
    let doc = sheet(
        &[RESISTOR],
        &format!(
            "{}\n{}\n{}",
            place("Device:R", "R1", "1k", 100.0, 100.0, 0.0, "(unit 1)"),
            place("Device:R", "R2", "1k", 120.0, 100.0, 0.0, "(unit 1)"),
            wire(100.0, 96.19, 140.0, 96.19),
        ),
    );
    assert_eq!(nets(&doc), vec![vec!["R1.1".to_string(), "R2.1".to_string()]]);
}

/// Two wires whose interiors cross are not connected — that is a drawing, not
/// a node. Each wire carries a real two-pin net so the crossing is the only
/// thing under test.
#[test]
fn crossing_wire_interiors_stay_apart() {
    let items = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        place("Device:R", "R1", "1k", 0.0, 13.81, 0.0, "(unit 1)"),
        place("Device:R", "R3", "1k", 20.0, 13.81, 0.0, "(unit 1)"),
        place("Device:R", "R2", "1k", 10.0, 23.81, 0.0, "(unit 1)"),
        place("Device:R", "R4", "1k", 13.81, 0.0, 90.0, "(unit 1)"),
        wire(0.0, 10.0, 20.0, 10.0),
        wire(10.0, 0.0, 10.0, 20.0),
    );
    let doc = sheet(&[RESISTOR], &items);
    assert_eq!(
        nets(&doc),
        vec![
            vec!["R1.1".to_string(), "R3.1".to_string()],
            vec!["R2.1".to_string(), "R4.1".to_string()],
        ]
    );

    // A junction at the crossing is what makes them one net.
    let with_dot = sheet(
        &[RESISTOR],
        &format!("{items}\n(junction (at 10 10) (uuid \"j\"))"),
    );
    assert_eq!(
        nets(&with_dot),
        vec![vec![
            "R1.1".to_string(),
            "R2.1".to_string(),
            "R3.1".to_string(),
            "R4.1".to_string(),
        ]]
    );
}

/// A wire *ending* on another wire is a T, and connects with no dot.
#[test]
fn a_wire_end_inside_a_wire_joins_it() {
    let doc = sheet(
        &[RESISTOR],
        &format!(
            "{}\n{}\n{}\n{}",
            place("Device:R", "R1", "1k", 0.0, 13.81, 0.0, "(unit 1)"),
            place("Device:R", "R2", "1k", 10.0, 23.81, 0.0, "(unit 1)"),
            wire(0.0, 10.0, 20.0, 10.0),
            wire(10.0, 10.0, 10.0, 20.0),
        ),
    );
    assert_eq!(nets(&doc), vec![vec!["R1.1".to_string(), "R2.1".to_string()]]);
}

/// Rail symbols name their net from `Value`, and same-named rails are one net
/// however far apart they are drawn.
#[test]
fn power_symbols_name_their_net_from_value_and_merge_by_it() {
    let doc = sheet(
        &[RESISTOR, GROUND],
        &format!(
            "{}\n{}\n{}\n{}",
            place("Device:R", "R1", "1k", 0.0, 3.81, 0.0, "(unit 1)"),
            place("Device:R", "R2", "1k", 50.0, 3.81, 0.0, "(unit 1)"),
            place("power:GND", "#PWR01", "GND", 0.0, 0.0, 0.0, "(unit 1)"),
            place("power:GND", "#PWR02", "GND", 50.0, 0.0, 0.0, "(unit 1)"),
        ),
    );
    let netlist = connect::extract(&doc);
    let gnd = net_named(&netlist, "GND");
    assert_eq!(gnd.source, NetSource::Power);
    let mut refs: Vec<&str> = gnd.pins.iter().map(|p| p.refdes.as_str()).collect();
    refs.sort_unstable();
    assert_eq!(refs, ["#PWR01", "#PWR02", "R1", "R2"]);

    // Renaming one rail splits them.
    let split = SchDoc::parse(&doc.to_text().replace("\"GND\" (at 50", "\"VSS\" (at 50"))
        .expect("reparse");
    let after = connect::extract(&split);
    assert_eq!(net_named(&after, "GND").pins.len(), 2);
    assert_eq!(net_named(&after, "VSS").pins.len(), 2);
}

/// A PWR_FLAG is a power symbol whose pin is an output; it must not name — and
/// so must not merge — the nets it is attached to.
#[test]
fn a_power_flag_names_nothing() {
    let doc = sheet(
        &[RESISTOR, FLAG],
        &format!(
            "{}\n{}\n{}\n{}",
            place("Device:R", "R1", "1k", 0.0, 3.81, 0.0, "(unit 1)"),
            place("Device:R", "R2", "1k", 50.0, 3.81, 0.0, "(unit 1)"),
            place("power:PWR_FLAG", "#FLG01", "PWR_FLAG", 0.0, 0.0, 0.0, "(unit 1)"),
            place("power:PWR_FLAG", "#FLG02", "PWR_FLAG", 50.0, 0.0, 0.0, "(unit 1)"),
        ),
    );
    assert_eq!(nets(&doc).len(), 2, "{:?}", connect::extract(&doc).nets);
}

/// Legacy parts hide their supply pins and expect them to connect by name.
#[test]
fn hidden_power_inputs_connect_globally_by_pin_name() {
    let doc = sheet(
        &[LEGACY],
        &format!(
            "{}\n{}",
            place("Legacy:U", "U1", "x", 0.0, 0.0, 0.0, "(unit 1)"),
            place("Legacy:U", "U2", "x", 100.0, 0.0, 0.0, "(unit 1)"),
        ),
    );
    let netlist = connect::extract(&doc);
    let vdd = net_named(&netlist, "VDD");
    assert_eq!(vdd.source, NetSource::Power);
    assert_eq!(vdd.pins.len(), 2);
    assert!(vdd.pins.iter().all(|p| p.pin == "8"));
}

/// Do-not-populate is a build instruction, not an electrical one.
#[test]
fn dnp_symbols_still_connect_and_say_so() {
    let doc = sheet(
        &[RESISTOR],
        &format!(
            "{}\n{}\n{}",
            place("Device:R", "R1", "1k", 100.0, 100.0, 0.0, "(unit 1) (dnp yes)"),
            place("Device:R", "R2", "1k", 120.0, 100.0, 0.0, "(unit 1)"),
            wire(100.0, 96.19, 120.0, 96.19),
        ),
    );
    let netlist = connect::extract(&doc);
    assert_eq!(netlist.nets.len(), 1);
    let pins = &netlist.nets[0].pins;
    assert!(pins.iter().find(|p| p.refdes == "R1").expect("R1").dnp);
    assert!(!pins.iter().find(|p| p.refdes == "R2").expect("R2").dnp);
}

/// A no-connect marker is an answer: the pin stops being a loose end without
/// becoming a net.
#[test]
fn a_no_connect_settles_a_pin() {
    let placed = place("Device:R", "R1", "1k", 100.0, 100.0, 0.0, "(unit 1)");
    let bare = connect::extract(&sheet(&[RESISTOR], &placed));
    assert_eq!(bare.unconnected.len(), 2);
    assert!(bare.no_connect.is_empty());

    let marked = connect::extract(&sheet(
        &[RESISTOR],
        &format!("{placed}\n(no_connect (at 100 96.19) (uuid \"nc\"))"),
    ));
    assert_eq!(marked.unconnected.len(), 1);
    assert_eq!(marked.unconnected[0].pin, "2");
    assert_eq!(marked.no_connect.len(), 1);
    assert_eq!(marked.no_connect[0].pin, "1");
}

/// A wire is not a connection. `kicad-cli` calls a lone pin on a dangling wire
/// `unconnected-(…)`, and so does this.
#[test]
fn a_dangling_wire_does_not_make_a_net() {
    let netlist = connect::extract(&sheet(
        &[RESISTOR],
        &format!(
            "{}\n{}",
            place("Device:R", "R1", "1k", 100.0, 100.0, 0.0, "(unit 1)"),
            wire(100.0, 96.19, 120.0, 96.19),
        ),
    ));
    assert!(netlist.nets.is_empty(), "{:?}", netlist.nets);
    assert_eq!(netlist.unconnected.len(), 2);
}

/// Placing unit 2 must bring unit 2's own pins and the shared unit-0 pins, and
/// nothing from unit 1.
#[test]
fn only_the_placed_unit_and_the_shared_pins_appear() {
    let doc = sheet(
        &[DUAL],
        &format!(
            "{}\n{}",
            place("Dual:OP", "U1", "op", 0.0, 0.0, 0.0, "(unit 1)"),
            place("Dual:OP", "U1", "op", 50.0, 0.0, 0.0, "(unit 2)"),
        ),
    );
    assert!(
        connect::extract(&doc).warnings.is_empty(),
        "the units of one part share a reference on purpose"
    );
    let mut placed: Vec<(u32, String)> = sch_doc::placed_pins(&doc)
        .into_iter()
        .map(|p| (p.unit, p.number))
        .collect();
    placed.sort();
    assert_eq!(
        placed,
        vec![
            (1, "1".to_string()),
            (1, "8".to_string()),
            (2, "2".to_string()),
            (2, "8".to_string()),
        ]
    );
}

/// A rotated *and* mirrored symbol is where the naive "negate the local
/// coordinate" transform goes wrong. At 90 degrees pin 1 lands 3.81 to the
/// left; `(mirror x)` reflects the sheet *y* offset, which is zero here, so
/// pin 1 stays left, while `(mirror y)` reflects x and swaps the pins over.
#[test]
fn rotation_and_mirror_place_pins_where_kicad_does() {
    let doc = sheet(
        &[RESISTOR],
        &format!(
            "{}\n{}\n{}",
            place("Device:R", "R1", "1k", 100.0, 100.0, 90.0, "(unit 1) (mirror x)"),
            place("Device:R", "R2", "1k", 120.0, 100.0, 90.0, "(unit 1) (mirror y)"),
            wire(103.81, 100.0, 116.19, 100.0),
        ),
    );
    let mut placed: Vec<(String, String, f64)> = sch_doc::placed_pins(&doc)
        .into_iter()
        .map(|p| (p.refdes, p.number, p.at.x))
        .collect();
    placed.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
    assert_eq!(
        placed,
        vec![
            ("R1".to_string(), "1".to_string(), 96.19),
            ("R1".to_string(), "2".to_string(), 103.81),
            ("R2".to_string(), "1".to_string(), 123.81),
            ("R2".to_string(), "2".to_string(), 116.19),
        ]
    );
    assert_eq!(nets(&doc), vec![vec!["R1.2".to_string(), "R2.2".to_string()]]);
}

/// Same-named labels merge inside a sheet, and the strongest source names the
/// result.
#[test]
fn labels_merge_by_name_and_the_strongest_source_names_the_net() {
    let doc = sheet(
        &[RESISTOR],
        &format!(
            "{}\n{}\n(label \"SIG\" (at 0 10 0) (uuid \"l1\"))\n\
             (global_label \"BUSY\" (at 50 10 0) (uuid \"l2\"))\n\
             (label \"BUSY\" (at 0 10 0) (uuid \"l3\"))",
            place("Device:R", "R1", "1k", 0.0, 13.81, 0.0, "(unit 1)"),
            place("Device:R", "R2", "1k", 50.0, 13.81, 0.0, "(unit 1)"),
        ),
    );
    let netlist = connect::extract(&doc);
    let busy = net_named(&netlist, "BUSY");
    assert_eq!(busy.source, NetSource::Global);
    let mut refs: Vec<&str> = busy.pins.iter().map(|p| p.refdes.as_str()).collect();
    refs.sort_unstable();
    assert_eq!(refs, ["R1", "R2"], "the global label did not merge the sheet");
    assert!(
        !netlist.nets.iter().any(|n| n.name == "SIG"),
        "SIG should have lost the naming race"
    );
}

/// With nothing to name it, a net falls back to KiCAD's generated form.
#[test]
fn unnamed_nets_get_a_generated_name() {
    let doc = sheet(
        &[RESISTOR],
        &format!(
            "{}\n{}\n{}",
            place("Device:R", "R1", "1k", 100.0, 100.0, 0.0, "(unit 1)"),
            place("Device:R", "R2", "1k", 120.0, 100.0, 0.0, "(unit 1)"),
            wire(100.0, 96.19, 120.0, 96.19),
        ),
    );
    let netlist = connect::extract(&doc);
    assert_eq!(netlist.nets[0].name, "Net-(R1-Pad1)");
    assert_eq!(netlist.nets[0].source, NetSource::Auto);
}

/// A hierarchical sheet's pins sit on its border in sheet coordinates, not
/// relative to the sheet box, and one is a connection point: two wires crossing
/// over it are joined by it, exactly as a junction or a symbol pin would.
#[test]
fn a_sheet_pin_joins_the_wires_that_cross_it() {
    let wires = format!(
        "{}\n{}\n{}\n{}",
        place("Device:R", "R1", "1k", 20.0, 58.81, 0.0, "(unit 1)"),
        place("Device:R", "R2", "1k", 53.81, 30.0, 90.0, "(unit 1)"),
        wire(20.0, 55.0, 80.0, 55.0),
        wire(50.0, 30.0, 50.0, 80.0),
    );
    let bare = sheet(&[RESISTOR], &wires);
    assert!(nets(&bare).is_empty(), "the wires crossed without a node");

    let doc = sheet(
        &[RESISTOR],
        &format!(
            "{wires}\n(sheet (at 50 55) (size 20 20) (uuid \"s\")\n\
             (property \"Sheetname\" \"child\" (at 50 54 0))\n\
             (property \"Sheetfile\" \"child.kicad_sch\" (at 50 76 0))\n\
             (pin \"IN\" input (at 50 55 180) (uuid \"sp\")))"
        ),
    );
    let pins = doc
        .items()
        .iter()
        .find_map(|item| match item {
            sch_doc::Item::Sheet(s) => Some(&s.pins),
            _ => None,
        })
        .expect("sheet");
    assert_eq!(pins[0].at.point(), geom::Point2::new(50.0, 55.0));
    assert_eq!(nets(&doc), vec![vec!["R1.1".to_string(), "R2.1".to_string()]]);
}

/// A hierarchical label names its sheet-scoped net, and loses to a local label
/// on the same net — KiCAD ranks local above hierarchical.
#[test]
fn hierarchical_labels_name_and_rank_below_local_ones() {
    let alone = connect::extract(&sheet(
        &[RESISTOR],
        &format!(
            "{}\n(hierarchical_label \"BUS_REQ\" input (at 0 10 0) (uuid \"h1\"))",
            place("Device:R", "R1", "1k", 0.0, 13.81, 0.0, "(unit 1)"),
        ),
    ));
    let hier = net_named(&alone, "BUS_REQ");
    assert_eq!(hier.source, NetSource::Hier);
    assert_eq!(hier.pins.len(), 1);

    let with_local = connect::extract(&sheet(
        &[RESISTOR],
        &format!(
            "{}\n(hierarchical_label \"BUS_REQ\" input (at 0 10 0) (uuid \"h1\"))\n\
             (label \"REQ\" (at 0 10 0) (uuid \"l1\"))",
            place("Device:R", "R1", "1k", 0.0, 13.81, 0.0, "(unit 1)"),
        ),
    ));
    assert_eq!(net_named(&with_local, "REQ").source, NetSource::Local);
    assert!(!with_local.nets.iter().any(|n| n.name == "BUS_REQ"));
}

/// KiCAD escapes characters it cannot store literally; a net named after a
/// label must come back decoded.
#[test]
fn label_escapes_are_decoded_into_the_net_name() {
    let netlist = connect::extract(&sheet(
        &[RESISTOR],
        &format!(
            "{}\n(label \"FB{{slash}}VSET\" (at 0 10 0) (uuid \"l1\"))",
            place("Device:R", "R1", "1k", 0.0, 13.81, 0.0, "(unit 1)"),
        ),
    ));
    assert_eq!(net_named(&netlist, "FB/VSET").source, NetSource::Local);
}

/// Before annotation every part is `R?`, so nothing keyed by reference can be
/// trusted — including the rail names. Say so instead of shorting the rails.
#[test]
fn duplicate_references_are_reported_and_do_not_short_the_rails() {
    let doc = sheet(
        &[RESISTOR, GROUND],
        &format!(
            "{}\n{}\n{}\n{}",
            place("Device:R", "R?", "1k", 0.0, 3.81, 0.0, "(unit 1)"),
            place("Device:R", "R?", "1k", 50.0, 3.81, 0.0, "(unit 1)")
                .replace("R?-uuid", "r-second"),
            place("power:GND", "#PWR?", "GND", 0.0, 0.0, 0.0, "(unit 1)"),
            place("power:GND", "#PWR?", "VCC", 50.0, 0.0, 0.0, "(unit 1)")
                .replace("#PWR?-uuid", "pwr-second"),
        ),
    );
    let netlist = connect::extract(&doc);
    assert!(
        netlist.warnings.iter().any(|w| w.contains("not unique")),
        "{:?}",
        netlist.warnings
    );
    let mut names: Vec<&str> = netlist.nets.iter().map(|n| n.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, ["GND", "VCC"], "the rails were shorted together");
}

/// A partition that gains or loses every pin is a creation or a removal, not a
/// rename, and a pin that ends up alone is reported.
#[test]
fn the_delta_reports_creation_removal_and_loose_ends() {
    let wired = sheet(
        &[RESISTOR],
        &format!(
            "{}\n{}\n{}",
            place("Device:R", "R1", "1k", 100.0, 100.0, 0.0, "(unit 1)"),
            place("Device:R", "R2", "1k", 120.0, 100.0, 0.0, "(unit 1)"),
            wire(100.0, 96.19, 120.0, 96.19),
        ),
    );
    let cut = sheet(
        &[RESISTOR],
        &format!(
            "{}\n{}",
            place("Device:R", "R1", "1k", 100.0, 100.0, 0.0, "(unit 1)"),
            place("Device:R", "R2", "1k", 120.0, 100.0, 0.0, "(unit 1)"),
        ),
    );
    let delta = connect::Netlist::diff(&connect::extract(&wired), &connect::extract(&cut));
    assert_eq!(delta.removed, vec!["Net-(R1-Pad1)".to_string()]);
    assert!(delta.created.is_empty(), "{delta:?}");
    assert_eq!(
        delta
            .pins_now_unconnected
            .iter()
            .map(|p| format!("{}.{}", p.refdes, p.pin))
            .collect::<Vec<_>>(),
        ["R1.1", "R2.1"]
    );

    let back = connect::Netlist::diff(&connect::extract(&cut), &connect::extract(&wired));
    assert_eq!(back.created, vec!["Net-(R1-Pad1)".to_string()]);
    assert!(back.pins_now_unconnected.is_empty(), "{back:?}");
}

#[test]
fn buses_and_missing_definitions_are_reported() {
    let bussed = sheet(
        &[RESISTOR],
        "(bus (pts (xy 0 0) (xy 10 0)) (uuid \"b\"))\n(bus_alias \"D\" (members \"D0\" \"D1\"))",
    );
    assert!(
        connect::extract(&bussed)
            .warnings
            .iter()
            .any(|w| w.contains("buses"))
    );

    let unknown = sheet(&[], &place("Device:R", "R1", "1k", 0.0, 0.0, 0.0, "(unit 1)"));
    let warnings = connect::extract(&unknown).warnings;
    assert!(
        warnings.iter().any(|w| w.contains("Device:R")),
        "{warnings:?}"
    );
}

/// The delta an editing tool reports has to name what actually happened.
#[test]
fn the_delta_distinguishes_a_merge_from_a_rename() {
    let apart = sheet(
        &[RESISTOR],
        &format!(
            "{}\n{}\n(label \"A\" (at 0 10 0) (uuid \"l1\"))\n\
             (label \"B\" (at 50 10 0) (uuid \"l2\"))",
            place("Device:R", "R1", "1k", 0.0, 13.81, 0.0, "(unit 1)"),
            place("Device:R", "R2", "1k", 50.0, 13.81, 0.0, "(unit 1)"),
        ),
    );
    let renamed = SchDoc::parse(&apart.to_text().replace("\"B\"", "\"C\"")).expect("reparse");
    let delta = connect::Netlist::diff(&connect::extract(&apart), &connect::extract(&renamed));
    assert_eq!(delta.renamed, vec![("B".to_string(), "C".to_string())]);
    assert!(delta.merged.is_empty() && delta.split.is_empty(), "{delta:?}");

    let joined = SchDoc::parse(&apart.to_text().replace("\"B\"", "\"A\"")).expect("reparse");
    let delta = connect::Netlist::diff(&connect::extract(&apart), &connect::extract(&joined));
    assert_eq!(
        delta.merged,
        vec![(vec!["A".to_string(), "B".to_string()], "A".to_string())]
    );

    let delta = connect::Netlist::diff(&connect::extract(&joined), &connect::extract(&apart));
    assert_eq!(
        delta.split,
        vec![("A".to_string(), vec!["A".to_string(), "B".to_string()])]
    );
}
