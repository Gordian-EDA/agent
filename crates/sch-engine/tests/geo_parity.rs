//! Parity of `geo::raw_boxes` (and through it `extent`/`bbox_for`/`rot_point`) against Python,
//! over every laid-out fixture design.

use sch_engine::geo::raw_boxes;
use sch_engine::symlib::Library;
use serde_json::Value;
use std::path::{Path, PathBuf};

const SYMBOL_DIR: &str = "/home/mimi/agent/.local/kicad-10.0.4/AppDir/usr/share/kicad/symbols";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Python's `Geo` keeps whole grid steps as ints and stub arithmetic as floats; `Geo::to_json`
/// writes every whole coordinate as an int. Compare numbers by value.
fn canon(v: &Value) -> Value {
    match v {
        Value::Array(a) => Value::Array(a.iter().map(canon).collect()),
        Value::Object(o) => Value::Object(o.iter().map(|(k, v)| (k.clone(), canon(v))).collect()),
        Value::Number(n) => match n.as_f64() {
            Some(f) if f.fract() == 0.0 => Value::from(f as i64),
            _ => v.clone(),
        },
        other => other.clone(),
    }
}

#[test]
fn raw_boxes_match_python() {
    let dir =
        PathBuf::from(std::env::var("KICAD_SYMBOL_DIR").unwrap_or_else(|_| SYMBOL_DIR.to_string()));
    if !dir.is_dir() {
        return;
    }
    Library::load(&dir).expect("index"); // also installs the process-wide index

    let mut checked = 0;
    for entry in std::fs::read_dir(fixtures()).unwrap().flatten() {
        let raw_path = entry.path().join("raw.json");
        let want_path = entry.path().join("raw_boxes.json");
        if !raw_path.is_file() || !want_path.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let raw: Value =
            serde_json::from_str(&std::fs::read_to_string(&raw_path).unwrap()).unwrap();
        let want: Vec<[f64; 4]> =
            serde_json::from_str(&std::fs::read_to_string(&want_path).unwrap()).unwrap();
        let got = raw_boxes(&raw);
        assert_eq!(got.len(), want.len(), "{name}: box count");
        for (i, (g, w)) in got.iter().zip(&want).enumerate() {
            for k in 0..4 {
                assert!(
                    (g[k] - w[k]).abs() < 1e-6,
                    "{name}: box {i}[{k}] {g:?} vs {w:?}"
                );
            }
        }
        checked += 1;
    }
    assert!(
        checked >= 15,
        "only {checked} fixtures had raw_boxes goldens"
    );
}

/// `pack_blocks` on synthetic blocks (boxes only): same packed geometry as Python.
#[test]
fn pack_blocks_matches_python() {
    let path = fixtures().join("pack_blocks.json");
    let cases: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let mut failures = Vec::new();
    for (i, c) in cases.iter().enumerate() {
        let geos: Vec<(String, sch_engine::Geo)> = c["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| {
                let mut g = sch_engine::Geo::new();
                g.boxes = serde_json::from_value(b[1].clone()).unwrap();
                (b[0].as_str().unwrap().to_string(), g)
            })
            .collect();
        let obstacles: Vec<[f64; 4]> = serde_json::from_value(c["obstacles"].clone()).unwrap();
        let tb = (c["tb"][0].as_f64().unwrap(), c["tb"][1].as_f64().unwrap());
        let got = sch_engine::geo::pack_blocks(
            &geos,
            c["x0"].as_f64().unwrap(),
            c["y0"].as_f64().unwrap(),
            c["xmax"].as_f64().unwrap(),
            c["ymax"].as_f64().unwrap(),
            tb,
            &obstacles,
            c["spread"].as_bool().unwrap(),
        );
        match (&got, c["result"].as_array()) {
            (None, None) => {}
            (Some(g), Some(want)) => {
                let want: Vec<[f64; 4]> = want
                    .iter()
                    .map(|b| serde_json::from_value(b.clone()).unwrap())
                    .collect();
                if g.boxes.len() != want.len()
                    || g.boxes
                        .iter()
                        .zip(&want)
                        .any(|(a, b)| (0..4).any(|k| (a[k] - b[k]).abs() > 1e-6))
                {
                    failures.push(format!("case {i}: got {:?} want {want:?}", g.boxes));
                }
            }
            _ => failures.push(format!(
                "case {i}: fits={} but Python fits={}",
                got.is_some(),
                c["result"].is_array()
            )),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert_eq!(cases.len(), 60);
}

/// `connect_pins` over 140 randomised (symbol, unit, rotation, mirror, pinmap, PWR_FLAG) cases.
#[test]
fn connect_pins_matches_python() {
    let dir =
        PathBuf::from(std::env::var("KICAD_SYMBOL_DIR").unwrap_or_else(|_| SYMBOL_DIR.to_string()));
    if !dir.is_dir() {
        return;
    }
    Library::load(&dir).expect("index");
    let cases: Vec<Value> = serde_json::from_str(
        &std::fs::read_to_string(fixtures().join("connect_pins.json")).unwrap(),
    )
    .unwrap();
    let mut failures = Vec::new();
    for (i, c) in cases.iter().enumerate() {
        let sym = sch_engine::geo::info(c["lib"].as_str().unwrap()).unwrap();
        sch_engine::geo::flag_at_symbol_clear();
        for (net, v) in c["flags"].as_object().unwrap() {
            sch_engine::geo::flag_at_symbol_set(net, v.as_bool().unwrap());
        }
        let at = [c["at"][0].as_f64().unwrap(), c["at"][1].as_f64().unwrap()];
        let skip: std::collections::BTreeSet<String> = c["skip"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap().to_string())
            .collect();
        let mut g = sch_engine::Geo::new();
        let r = sch_engine::geo::connect_pins(
            &mut g,
            &sym,
            at,
            c["rot"].as_i64().unwrap() as i32,
            c["mirror"].as_str().unwrap(),
            c["unit"].as_i64().unwrap() as i32,
            c["pinmap"].as_object().unwrap(),
            &c["default"],
            &skip,
        );
        if let Err(e) = r {
            failures.push(format!("case {i}: {e}"));
            continue;
        }
        if canon(&g.to_json()) != canon(&c["geo"]) {
            failures.push(format!(
                "case {i} ({}): geometry differs\n got  {}\n want {}",
                c["lib"],
                g.to_json(),
                c["geo"]
            ));
            continue;
        }
        let want_boxes: Vec<[f64; 4]> = serde_json::from_value(c["boxes"].clone()).unwrap();
        if g.boxes.len() != want_boxes.len()
            || g.boxes
                .iter()
                .zip(&want_boxes)
                .any(|(a, b)| (0..4).any(|k| (a[k] - b[k]).abs() > 1e-6))
        {
            failures.push(format!("case {i} ({}): boxes differ", c["lib"]));
            continue;
        }
        for (net, v) in c["flags_after"].as_object().unwrap() {
            if sch_engine::geo::flag_at_symbol_get(net) != v.as_bool() {
                failures.push(format!("case {i}: PWR_FLAG state of {net} differs"));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} cases differ:\n{}",
        failures.len(),
        cases.len(),
        failures
            .iter()
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert_eq!(cases.len(), 140);
}
