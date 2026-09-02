//! A part's pins must reach the nets the design gave them, in every pose.
//!
//! The engine's `(angle, mirror)` transform is only as good as KiCAD's agreement
//! with it: mirroring the LOCAL x before the rotation matches `(mirror y)` at
//! 0°/180° and is its point-reflection at 90°/270°, which on a symmetric 2-pin part
//! reads as its two pins TRANSPOSED — every wire drawn to the other pin's net. That
//! is invisible to the pure-Rust oracle (the writer and the extractor would share the
//! mistake), so the pose gate below asks `kicad-cli` itself.
//!
//! SKIPs without a KiCAD installation.

use kicad::KicadInstallation;
use kicad_symbol::SymbolTable;
use sch_doc::SchDoc;
use sch_floorplan::live;
use sch_floorplan::write::SchematicWriter;

const ANGLES: [f64; 4] = [0.0, 90.0, 180.0, 270.0];

fn detect() -> Option<KicadInstallation> {
    match KicadInstallation::detect() {
        Some(env) => Some(env),
        None => {
            eprintln!("SKIP: no KiCAD environment detected");
            None
        }
    }
}

/// `refdes.pin -> net name` as `kicad-cli` reports it.
fn cli_pin_nets(env: &KicadInstallation, path: &std::path::Path) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = env
        .netlist(path)
        .expect("kicad-cli netlist")
        .nets
        .into_iter()
        .flat_map(|net| {
            net.nodes
                .into_iter()
                .map(move |(refdes, pin)| (format!("{refdes}.{pin}"), net.name.clone()))
        })
        .collect();
    out.sort();
    out
}

/// The writer's idea of where pin 1 and pin 2 are must be KiCAD's, at every one of
/// the eight `(angle, mirror)` poses. Labelling each pin by NUMBER and asking
/// `kicad-cli` which pin carries which net catches a transposed pose directly.
#[test]
fn a_two_pin_part_keeps_its_pin_order_in_every_pose() {
    let Some(env) = detect() else { return };
    let dir = tempfile::tempdir().unwrap();
    let mut wrong = Vec::new();
    for angle in ANGLES {
        for mirror in [false, true] {
            let mut w = SchematicWriter::new();
            w.add_symbol_full(&env, "Device:R", "R1", "1k", [100.0, 100.0], angle, None, &[], None)
                .unwrap();
            if mirror {
                w.set_mirror_last();
            }
            w.add_pin_label(&env, "R1", "1", "NET_ONE").unwrap();
            w.add_pin_label(&env, "R1", "2", "NET_TWO").unwrap();
            let path = dir.path().join(format!("pose-{angle}-{mirror}.kicad_sch"));
            std::fs::write(&path, w.finish()).unwrap();
            let got = cli_pin_nets(&env, &path);
            let want = vec![
                ("R1.1".to_string(), "/NET_ONE".to_string()),
                ("R1.2".to_string(), "/NET_TWO".to_string()),
            ];
            if got != want {
                wrong.push(format!("angle={angle} mirror={mirror}: {got:?}"));
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "a pose transposed the pins KiCAD reads:\n{}",
        wrong.join("\n")
    );
}

/// The same claim one level up: a 2-pin part authored with each [`Orient`], mirrored
/// or not, placed by the shipping engine, must draw the pin->net map it was given.
///
/// [`Orient`]: sch_place::ir::Orient
#[test]
fn placing_a_two_pin_part_honours_every_authored_orientation() {
    let Some(env) = detect() else { return };
    let provider = SymbolTable::from_symbol_dir(env.symbol_dir().to_path_buf());
    let dir = tempfile::tempdir().unwrap();
    let mut wrong = Vec::new();
    for orient in ["up", "down", "left", "right"] {
        for mirror in [false, true] {
            // X1 under test, flanked so BOTH its nets carry a second pin: a
            // transposition on a net with one pin is not observable.
            let input: sch_check::PlacePartsInput = serde_json::from_value(serde_json::json!({
                "parts": [
                    {"ref": "X1", "part": "Device:C_Small", "pins": {"1": "N1", "2": "N2"}},
                    {"ref": "R8", "part": "Device:R", "pins": {"1": "N1", "2": "N0"}},
                    {"ref": "R9", "part": "Device:R", "pins": {"1": "N2", "2": "N0"}}
                ],
                "intent": {
                    "place": {"X1": {"col": 1, "row": 1, "orient": orient}},
                    "mirror": if mirror { vec!["X1"] } else { vec![] },
                }
            }))
            .unwrap();
            let (design, diagnostics, _) =
                sch_check::into_design(&input, &provider, &Default::default());
            assert!(!diagnostics.has_errors(), "{diagnostics:#?}");

            let mut doc = live::blank_sheet().unwrap();
            let report =
                live::place_parts(&env, &mut doc, &input, &cluster_place::ClusterPlace, None).unwrap();
            if !report.mismatch.is_empty() {
                wrong.push(format!("{orient}/mirror={mirror}: {:?}", report.mismatch));
                continue;
            }
            // `verify` shares the writer's pose transform, so confirm against KiCAD too.
            let path = dir.path().join(format!("place-{orient}-{mirror}.kicad_sch"));
            doc.write(&path).unwrap();
            let nets: std::collections::BTreeMap<String, String> =
                cli_pin_nets(&env, &path).into_iter().collect();
            let (one, two) = (nets.get("X1.1"), nets.get("X1.2"));
            if one.is_none() || one == two {
                wrong.push(format!("{orient}/mirror={mirror}: X1 pins on {one:?}/{two:?}"));
            }
            if nets.get("X1.1") != nets.get("R8.1") || nets.get("X1.2") != nets.get("R9.1") {
                wrong.push(format!(
                    "{orient}/mirror={mirror}: X1 pins TRANSPOSED — {nets:?}"
                ));
            }
            let _ = SchDoc::read(&path).unwrap();
            let _ = &design;
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
