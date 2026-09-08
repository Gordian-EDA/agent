//! Parity of the Rust symbol index against the Python `schagent.symlib` reference.
//!
//! Fixtures come from `scratchpad/dump_symlib.py` (see `tests/fixtures/symlib/`).

use sch_engine::model::{GRID, rot_point};
use sch_engine::symlib::{Library, describe};
use serde_json::Value;
use std::path::{Path, PathBuf};

const SYMBOL_DIR: &str = "/home/mimi/agent/.local/kicad-10.0.4/AppDir/usr/share/kicad/symbols";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/symlib")
}

fn symbol_dir() -> Option<PathBuf> {
    let d = std::env::var("KICAD_SYMBOL_DIR").unwrap_or_else(|_| SYMBOL_DIR.to_string());
    let p = PathBuf::from(d);
    p.is_dir().then_some(p)
}

fn load() -> Option<(Library, Value)> {
    let dir = symbol_dir()?;
    let idx = Library::load(&dir).expect("index");
    let golden: Value =
        serde_json::from_str(&std::fs::read_to_string(fixtures().join("symbols.json")).ok()?)
            .ok()?;
    Some((idx, golden))
}

fn near(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

#[test]
fn index_covers_the_same_lib_ids() {
    let Some(dir) = symbol_dir() else { return };
    let idx = Library::load(&dir).expect("index");
    let golden: Vec<String> =
        serde_json::from_str(&std::fs::read_to_string(fixtures().join("lib_ids.json")).unwrap())
            .unwrap();
    let ours: Vec<String> = idx.symbols.keys().cloned().collect();
    let missing: Vec<&String> = golden
        .iter()
        .filter(|g| idx.get(g).is_none())
        .take(10)
        .collect();
    let extra: Vec<&String> = ours
        .iter()
        .filter(|o| !golden.contains(o))
        .take(10)
        .collect();
    assert!(
        missing.is_empty(),
        "missing from the Rust index: {missing:?}"
    );
    assert!(extra.is_empty(), "extra in the Rust index: {extra:?}");
    assert_eq!(ours.len(), golden.len());
}

#[test]
fn symbol_fields_match_python() {
    let Some((idx, golden)) = load() else { return };
    for (lib_id, g) in golden["symbols"].as_object().unwrap() {
        let info = idx
            .get(lib_id)
            .unwrap_or_else(|| panic!("{lib_id} not indexed"));
        assert_eq!(
            info.description,
            g["description"].as_str().unwrap(),
            "{lib_id} description"
        );
        assert_eq!(
            info.keywords,
            g["keywords"].as_str().unwrap(),
            "{lib_id} keywords"
        );
        assert_eq!(
            info.ref_prefix,
            g["ref_prefix"].as_str().unwrap(),
            "{lib_id} ref_prefix"
        );
        assert_eq!(
            info.footprint,
            g["footprint"].as_str().unwrap(),
            "{lib_id} footprint"
        );
        assert_eq!(
            info.datasheet,
            g["datasheet"].as_str().unwrap(),
            "{lib_id} datasheet"
        );
        assert_eq!(
            info.fp_filters,
            g["fp_filters"].as_str().unwrap(),
            "{lib_id} fp_filters"
        );
        assert_eq!(
            info.units as i64,
            g["units"].as_i64().unwrap(),
            "{lib_id} units"
        );
        assert_eq!(info.power, g["power"].as_bool().unwrap(), "{lib_id} power");
        assert_eq!(
            info.extends,
            g["extends"].as_str().unwrap(),
            "{lib_id} extends"
        );

        for (name, got) in [("bbox", info.bbox), ("body", info.body)] {
            let want = g[name].as_array().unwrap();
            let got = [got.0, got.1, got.2, got.3];
            for i in 0..4 {
                assert!(
                    near(got[i], want[i].as_f64().unwrap()),
                    "{lib_id} {name}[{i}]: {got:?} vs {want:?}"
                );
            }
        }
        for (name, map) in [
            ("unit_bbox", &info.unit_bbox),
            ("unit_body", &info.unit_body),
        ] {
            let want = g[name].as_object().unwrap();
            assert_eq!(map.len(), want.len(), "{lib_id} {name} size");
            for (u, v) in want {
                let got = map[&u.parse::<i32>().unwrap()];
                let got = [got.0, got.1, got.2, got.3];
                let v = v.as_array().unwrap();
                for i in 0..4 {
                    assert!(
                        near(got[i], v[i].as_f64().unwrap()),
                        "{lib_id} {name}[{u}][{i}]"
                    );
                }
            }
        }

        let want_pins = g["pins"].as_array().unwrap();
        assert_eq!(info.pins.len(), want_pins.len(), "{lib_id} pin count");
        for (p, w) in info.pins.iter().zip(want_pins) {
            assert_eq!(
                p.number,
                w["number"].as_str().unwrap(),
                "{lib_id} pin number"
            );
            assert_eq!(
                p.name,
                w["name"].as_str().unwrap(),
                "{lib_id} pin {} name",
                p.number
            );
            assert_eq!(
                p.etype,
                w["etype"].as_str().unwrap(),
                "{lib_id} pin {} etype",
                p.number
            );
            assert_eq!(
                p.side(),
                w["side"].as_str().unwrap(),
                "{lib_id} pin {} side",
                p.number
            );
            assert_eq!(
                p.angle as i64,
                w["angle"].as_i64().unwrap(),
                "{lib_id} pin {} angle",
                p.number
            );
            assert_eq!(
                p.unit as i64,
                w["unit"].as_i64().unwrap(),
                "{lib_id} pin {} unit",
                p.number
            );
            assert_eq!(
                p.hidden,
                w["hidden"].as_bool().unwrap(),
                "{lib_id} pin {} hidden",
                p.number
            );
            assert!(
                near(p.x, w["x"].as_f64().unwrap()),
                "{lib_id} pin {} x",
                p.number
            );
            assert!(
                near(p.y, w["y"].as_f64().unwrap()),
                "{lib_id} pin {} y",
                p.number
            );
            assert!(
                near(p.length, w["length"].as_f64().unwrap()),
                "{lib_id} pin {} length",
                p.number
            );
        }
    }
}

/// Pin positions and outward directions for every rotation/mirror the engine uses.
#[test]
fn pin_geometry_per_pose_matches_python() {
    let Some((idx, golden)) = load() else { return };
    for (lib_id, g) in golden["symbols"].as_object().unwrap() {
        let info = idx.get(lib_id).unwrap();
        for (pose, want) in g["poses"].as_object().unwrap() {
            let (rm, unit) = pose.split_once('u').unwrap();
            let unit: i32 = unit.parse().unwrap();
            let cut = rm.find(|c: char| !c.is_ascii_digit()).unwrap_or(rm.len());
            let (rot, mirror) = (rm[..cut].parse::<i32>().unwrap(), &rm[cut..]);
            for pin in info.pins_for_unit(unit) {
                let w = &want[&pin.number];
                if w.is_null() {
                    continue; // duplicate pin numbers: Python keeps the first
                }
                let (dx, dy) = rot_point(pin.x, pin.y, rot, mirror);
                let d = sch_engine::geo::dir_of(pin, rot, mirror);
                let w = w.as_array().unwrap();
                assert!(
                    (dx / GRID - w[0].as_f64().unwrap()).abs() < 1e-3,
                    "{lib_id} {pose} pin {} x: {} vs {}",
                    pin.number,
                    dx / GRID,
                    w[0]
                );
                assert!(
                    (dy / GRID - w[1].as_f64().unwrap()).abs() < 1e-3,
                    "{lib_id} {pose} pin {} y",
                    pin.number
                );
                let wd = w[2].as_array().unwrap();
                assert_eq!(
                    (d.0 as i64, d.1 as i64),
                    (wd[0].as_i64().unwrap(), wd[1].as_i64().unwrap()),
                    "{lib_id} {pose} pin {} dir",
                    pin.number
                );
            }
        }
    }
}

#[test]
fn describe_text_matches_python() {
    let Some((idx, golden)) = load() else { return };
    for (lib_id, g) in golden["symbols"].as_object().unwrap() {
        let info = idx.get(lib_id).unwrap();
        assert_eq!(
            describe(&info, None),
            g["describe"].as_str().unwrap(),
            "{lib_id} describe"
        );
    }
}

#[test]
fn search_ranking_matches_python() {
    let Some((idx, golden)) = load() else { return };
    let mut bad = Vec::new();
    for (query, want) in golden["searches"].as_object().unwrap() {
        let want: Vec<&str> = want
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        let got: Vec<String> = idx
            .search_infos(query, 15)
            .into_iter()
            .map(|i| i.lib_id.clone())
            .collect();
        if got != want {
            bad.push(format!("{query}:\n  got  {got:?}\n  want {want:?}"));
        }
    }
    assert!(
        bad.is_empty(),
        "search ranking differs:\n{}",
        bad.join("\n")
    );
}

/// `raw_symbol` must produce an embeddable node named by the full lib id, with `extends` flattened.
#[test]
fn raw_symbol_is_flattened() {
    let Some(dir) = symbol_dir() else { return };
    let idx = Library::load(&dir).expect("index");
    for lib_id in ["Device:R", "Device:C_Small", "power:GND", "Timer:NE555P"] {
        let Some(node) = idx.raw_symbol(lib_id) else {
            panic!("{lib_id} has no raw symbol")
        };
        assert_eq!(node.as_list().unwrap()[1].as_str().unwrap(), lib_id);
        assert!(node.child("extends").is_none(), "{lib_id} still extends");
        assert!(
            !node.children("symbol").is_empty(),
            "{lib_id} has no sub-symbols"
        );
    }
}
