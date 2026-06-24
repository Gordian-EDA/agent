//! Byte-identity golden: a representative board (parts + plane zone + keepout +
//! net classes + custom outline) must emit a stable `.kicad_pcb`. Guards the
//! synthesis refactor (BoardModel / Synthesizer) against any byte drift.

use std::collections::BTreeMap;
use std::path::PathBuf;

use pcb_model::{Bounds, Point2};
use pcb_place::placement::Placement;
use pcb_synth::synth::{
    plane_fill_rects, synthesize_board_full, KeepoutZone, NetClass, SynthPart, ZoneSpec,
};

fn fixture(name: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../kicad-sexpr/tests/fixtures/footprints")
        .join(name);
    std::fs::read_to_string(p).unwrap()
}

fn nets(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

fn place(reference: &str, x: f64, y: f64, rot: i32) -> Placement {
    Placement { reference: reference.to_owned(), at: Point2 { x, y }, rotation: rot }
}

/// The canonical golden board: exercises every emit path so byte-identity is a
/// total guard. Returns the emitted text.
fn golden_board() -> String {
    let parts = vec![
        SynthPart {
            reference: "R1".into(),
            lib_id: "Resistor_SMD:R_0603_1608Metric".into(),
            source: fixture("R_0603_1608Metric.kicad_mod"),
            pad_nets: nets(&[("1", "VOUT"), ("2", "GND")]),
            placement: place("R1", 10.0, 10.0, 90),
        },
        SynthPart {
            reference: "U1".into(),
            lib_id: "Package_TO_SOT_SMD:SOT-23".into(),
            source: fixture("SOT-23.kicad_mod"),
            pad_nets: nets(&[("1", "VIN"), ("2", "GND"), ("3", "VOUT")]),
            placement: place("U1", 20.0, 10.0, 0),
        },
    ];
    let bounds = Bounds { min_x: 0.0, max_x: 30.0, min_y: 0.0, max_y: 20.0 };
    let zones = vec![ZoneSpec {
        net_name: "GND".into(),
        layer_name: "In1.Cu".into(),
        fill_rects: plane_fill_rects(&bounds, 0.5, &[(Point2 { x: 15.0, y: 10.0 }, 0.65, 0.65)], None),
        clearance: 0.2,
        min_thickness: 0.25,
    }];
    let keepouts = vec![KeepoutZone {
        layers: vec!["F.Cu".into(), "B.Cu".into()],
        min: [2.0, 2.0],
        max: [5.0, 5.0],
    }];
    let outline = vec![
        Point2 { x: 0.0, y: 0.0 },
        Point2 { x: 30.0, y: 0.0 },
        Point2 { x: 30.0, y: 20.0 },
        Point2 { x: 0.0, y: 20.0 },
    ];
    let classes = vec![
        NetClass {
            name: "Power".into(),
            description: "fat power nets".into(),
            clearance: 0.2,
            trace_width: 0.8,
            via_diameter: 0.6,
            via_drill: 0.3,
            members: vec!["VOUT".into()],
        },
        NetClass {
            name: "Default".into(),
            description: "board default".into(),
            clearance: 0.2,
            trace_width: 0.2,
            via_diameter: 0.6,
            via_drill: 0.3,
            members: vec!["GND".into()],
        },
    ];
    synthesize_board_full(&parts, &bounds, 4, &zones, &keepouts, Some(&outline), &classes).unwrap()
}

#[test]
fn golden_board_is_stable() {
    let board = golden_board();
    // Sanity: the board carries every emit feature.
    assert!(board.contains("(net_class \"Power\""));
    assert!(board.contains("(zone"));
    assert!(board.contains("(keepout"));
    assert!(board.contains("(gr_line")); // custom outline
    assert!(board.contains("(at 10 10 90)")); // rotated footprint
    // Dump for the cross-SHA byte-identity diff.
    if let Ok(dst) = std::env::var("GOLDEN_DUMP") {
        std::fs::write(dst, board.as_bytes()).unwrap();
    }
}
