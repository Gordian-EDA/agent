//! The extractor answers to `kicad-cli`: same partition, same loose ends.

use quality_facts::net;
use quality_facts::sch::Schematic;

/// Two resistors in series between a rail and ground, drawn the way KiCad
/// writes one: a wire between the pins, power symbols naming the ends, and one
/// pin left dangling under a no-connect marker.
const DIVIDER: &str = r##"
(kicad_sch
  (lib_symbols
    (symbol "Device:R"
      (symbol "R_0_1" (rectangle (start -1.016 -2.54) (end 1.016 2.54)))
      (symbol "R_1_1"
        (pin passive line (at 0 3.81 270) (length 1.27) (name "~") (number "1"))
        (pin passive line (at 0 -3.81 90) (length 1.27) (name "~") (number "2"))))
    (symbol "power:GND" (power)
      (symbol "GND_1_1"
        (pin power_in line (at 0 0 270) (length 0) (name "GND") (number "1") (hide yes))))
    (symbol "power:VCC" (power)
      (symbol "VCC_1_1"
        (pin power_in line (at 0 0 90) (length 0) (name "VCC") (number "1") (hide yes))))
    (symbol "Device:C"
      (symbol "C_1_1"
        (pin passive line (at 0 3.81 270) (length 2.54) (name "~") (number "1"))
        (pin passive line (at 0 -3.81 90) (length 2.54) (name "~") (number "2")))))
  (wire (pts (xy 100 53.81) (xy 100 60)))
  (no_connect (at 120 47.46))
  (symbol (lib_id "Device:R") (at 100 50 0) (unit 1) (uuid "r1")
    (property "Reference" "R1" (at 102 50 0))
    (property "Value" "10k" (at 102 52 0)))
  (symbol (lib_id "Device:R") (at 100 63.81 0) (unit 1) (uuid "r2")
    (property "Reference" "R2" (at 102 63.81 0))
    (property "Value" "10k" (at 102 66 0)))
  (symbol (lib_id "power:VCC") (at 100 46.19 0) (unit 1) (uuid "p1")
    (property "Reference" "#PWR01" (at 100 44 0))
    (property "Value" "VCC" (at 100 44 0)))
  (symbol (lib_id "power:GND") (at 100 67.62 0) (unit 1) (uuid "p2")
    (property "Reference" "#PWR02" (at 100 70 0))
    (property "Value" "GND" (at 100 70 0)))
  (symbol (lib_id "Device:C") (at 120 51.27 0) (unit 1) (uuid "c1")
    (property "Reference" "C1" (at 122 51.27 0))
    (property "Value" "100n" (at 122 53 0))))
"##;

fn divider() -> Schematic {
    Schematic::parse(DIVIDER).expect("the sheet parses")
}

#[test]
fn wires_and_power_symbols_make_the_partition() {
    let netlist = net::extract(&divider());
    assert_eq!(
        netlist.partition(),
        vec![
            vec!["#PWR01.1".to_string(), "R1.1".to_string()],
            vec!["#PWR02.1".to_string(), "R2.2".to_string()],
            vec!["R1.2".to_string(), "R2.1".to_string()],
        ]
    );
    let names: Vec<&str> = netlist.nets.iter().map(|n| n.name.as_str()).collect();
    assert_eq!(names, vec!["GND", "Net-(R1-Pad2)", "VCC"]);
}

#[test]
fn a_marker_settles_a_pin_and_a_bare_pin_is_a_loose_end() {
    let netlist = net::extract(&divider());
    let settled: Vec<String> = netlist.no_connect.iter().map(|p| p.label()).collect();
    let loose: Vec<String> = netlist.unconnected.iter().map(|p| p.label()).collect();
    assert_eq!(settled, vec!["C1.1"]);
    assert_eq!(loose, vec!["C1.2"]);
}

#[test]
fn inserting_a_part_splits_the_net_it_was_inserted_into() {
    let before = net::extract(&divider());
    let after = net::extract(
        &Schematic::parse(&DIVIDER.replace("(wire (pts (xy 100 53.81) (xy 100 60)))", "")).unwrap(),
    );
    let delta = net::diff(&before, &after);
    assert!(!delta.is_empty());
    assert_eq!(delta.removed, vec!["Net-(R1-Pad2)"]);
    assert_eq!(
        delta
            .pins_now_unconnected
            .iter()
            .map(|p| p.label())
            .collect::<Vec<_>>(),
        vec!["R1.2", "R2.1"]
    );
}

#[test]
fn a_symbol_body_is_the_box_its_graphics_draw() {
    let doc = divider();
    let r1 = doc.symbols.iter().find(|s| s.refdes() == "R1").unwrap();
    let body = doc.body_rect(r1).unwrap();
    assert!((body.min_x - 98.984).abs() < 1e-6, "{body:?}");
    assert!((body.max_y - 52.54).abs() < 1e-6, "{body:?}");
}
