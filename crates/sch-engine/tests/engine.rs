//! Parity tests for `sch_engine::engine` against golden values dumped from `~/sch-agent`
//! (`tests/fixtures/engine_helpers/helpers.json`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

use sch_engine::engine::{connector_text_slot, covers, is_power_net, rot_body, strip_power_parts, two_pin_axis};
use sch_engine::symlib::index;

fn fixture() -> Value {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/engine_helpers/helpers.json");
    serde_json::from_str(&std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))).unwrap()
}

/// The stock symbol dir the Python reference indexes (skips the test when it is not installed).
fn load_index() -> bool {
    for dir in [
        "/home/mimi/agent/.local/kicad-10.0.4/AppDir/usr/share/kicad/symbols",
        "/usr/share/kicad/symbols",
    ] {
        if Path::new(dir).is_dir() {
            let _ = sch_engine::Library::load(Path::new(dir));
            return index().len() > 0;
        }
    }
    false
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

#[test]
fn symbol_geometry_matches_python() {
    if !load_index() {
        eprintln!("no symbol library installed - skipping");
        return;
    }
    let fx = fixture();
    for c in fx["symbols"].as_array().unwrap() {
        let lib = c["lib"].as_str().unwrap();
        if c.get("missing").is_some() {
            continue;
        }
        let info = index().get(lib).unwrap_or_else(|| panic!("{lib} not in the index"));
        let (unit, rot, mirror) =
            (c["unit"].as_i64().unwrap() as i32, c["rot"].as_i64().unwrap() as i32, c["mirror"].as_str().unwrap());
        assert_eq!(info.pins_for_unit(unit).len(), c["npins"].as_u64().unwrap() as usize, "{lib}: pin count");
        let (hx, hy) = rot_body(&info, unit, rot, mirror);
        let want = c["rot_body"].as_array().unwrap();
        assert!(
            close(hx, want[0].as_f64().unwrap()) && close(hy, want[1].as_f64().unwrap()),
            "{lib} rot={rot} mirror={mirror:?}: rot_body {:?} != {want:?}",
            (hx, hy)
        );
        assert_eq!(two_pin_axis(&info, unit, rot, mirror), c["two_pin_axis"].as_str().unwrap(), "{lib} two_pin_axis");
        assert_eq!(
            connector_text_slot(&info, unit, rot, mirror),
            c["connector_text_slot"].as_str().unwrap(),
            "{lib} rot={rot} connector_text_slot"
        );
    }
}

#[test]
fn is_power_net_matches_python() {
    if !load_index() {
        eprintln!("no symbol library installed - skipping");
        return;
    }
    let fx = fixture();
    let extra: HashSet<String> = ["VMOT".to_string()].into_iter().collect();
    for (net, want) in fx["is_power_net"].as_object().unwrap() {
        let w = want.as_array().unwrap();
        assert_eq!(is_power_net(net, &HashSet::new()), w[0].as_bool().unwrap(), "{net} (no extras)");
        assert_eq!(is_power_net(net, &extra), w[1].as_bool().unwrap(), "{net} (extra VMOT)");
    }
}

#[test]
fn covers_matches_python() {
    let fx = fixture();
    for c in fx["covers"].as_array().unwrap() {
        let pt = |k: &str| {
            let v = c[k].as_array().unwrap();
            [v[0].as_f64().unwrap(), v[1].as_f64().unwrap()]
        };
        assert_eq!(
            covers(pt("a"), pt("b"), pt("p"), pt("q")),
            c["covers"].as_bool().unwrap(),
            "covers{:?}",
            (pt("a"), pt("b"), pt("p"), pt("q"))
        );
    }
}

#[test]
fn strip_power_parts_matches_python() {
    if !load_index() {
        eprintln!("no symbol library installed - skipping");
        return;
    }
    let fx = fixture();
    let mut d: Value = serde_json::json!({
        "parts": [
            {"id": "U1", "lib": "Timer:NE555P", "pins": {"1": "GND"}},
            {"id": "#FLG1", "lib": "power:PWR_FLAG", "pins": {"1": "+5V"}},
            {"id": "#PWR1", "lib": "power:GND", "pins": {"1": "GND"}}
        ],
        "layout": [{"title": "T", "tree": {"row": [
            {"part": "U1"}, {"part": "#FLG1"}, {"col": [{"part": "#PWR1"}]}
        ]}}]
    });
    let notes = strip_power_parts(&mut d);
    let want = &fx["strip_power_parts"];
    let want_notes: Vec<String> =
        want["notes"].as_array().unwrap().iter().map(|n| n.as_str().unwrap().to_string()).collect();
    assert_eq!(notes, want_notes);
    assert_eq!(d["parts"], want["design"]["parts"]);
    assert_eq!(d["flags"], want["design"]["flags"]);
    assert_eq!(d["layout"], want["design"]["layout"]);
}

// --------------------------------------------------------------- route + emit block parity

/// Replay every block of every fixture design through [`GroupLayout`] with the placement the Python
/// pipeline chose, and compare the emitted geometry (`tests/fixtures/engine_blocks/`, dumped by
/// instrumenting `flexlayout.circuit_geos`).
#[test]
fn group_layout_blocks_match_python() {
    use std::collections::HashMap;

    use sch_engine::engine::{GroupLayout, PartInst};

    if !load_index() {
        eprintln!("no symbol library installed - skipping");
        return;
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/engine_blocks");
    let idx: Value = serde_json::from_str(&std::fs::read_to_string(root.join("index.json")).unwrap()).unwrap();
    let mut checked = 0usize;
    let mut failures: Vec<String> = Vec::new();
    for entry in idx.as_array().unwrap() {
        let name = entry["name"].as_str().unwrap();
        let fx: Value = serde_json::from_str(&std::fs::read_to_string(root.join(format!("{name}.json"))).unwrap())
            .unwrap();
        for (bi, blk) in fx["blocks"].as_array().unwrap().iter().enumerate() {
            let mut parts: Vec<PartInst> = Vec::new();
            for pj in blk["parts"].as_array().unwrap() {
                let mut p = PartInst::new(pj).unwrap_or_else(|e| panic!("{name} block {bi}: {e}"));
                let at = pj["at"].as_array().unwrap();
                p.at = [at[0].as_f64().unwrap(), at[1].as_f64().unwrap()];
                p.rot = pj["rot"].as_i64().unwrap() as i32;
                p.mirror = pj["mirror"].as_str().unwrap().to_string();
                parts.push(p);
            }
            let power: HashSet<String> =
                blk["power"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
            let flags: Vec<String> =
                blk["flags"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
            let all_nets: HashMap<String, usize> = blk["all_nets"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), v.as_u64().unwrap() as usize))
                .collect();
            let label_pins: HashSet<(String, String)> = blk["label_pins"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| {
                    let a = v.as_array().unwrap();
                    (a[0].as_str().unwrap().to_string(), a[1].as_str().unwrap().to_string())
                })
                .collect();
            let mut gl = GroupLayout::new(parts, power, flags, all_nets, label_pins);
            gl.max_wire = blk["max_wire"].as_f64().unwrap();
            // the extents and component boxes the router works from (also flexlayout's _leaf_box)
            for (pi, pj) in blk["parts"].as_array().unwrap().iter().enumerate() {
                let p = &gl.parts[pi];
                assert_eq!(p.power_only, pj["power_only"].as_bool().unwrap(), "{name} block {bi} {}: power_only", p.id);
                let attach: Value = Value::Object(
                    p.attach.iter().map(|(k, v)| (k.clone(), serde_json::json!(v))).collect(),
                );
                assert_eq!(norm(&attach), norm(&pj["attach"]), "{name} block {bi} {}: attach", p.id);
                for (what, got) in [
                    ("extent", serde_json::to_value(p.extent()).unwrap()),
                    ("extent_no_attach", serde_json::to_value(p.extent_no_attach()).unwrap()),
                    ("boxes", serde_json::to_value(p.boxes(true)).unwrap()),
                ] {
                    // extents feed a grid router, so the last-ULP float noise between the two
                    // implementations is compared with a tolerance
                    if !approx(&got, &pj[what], 1e-6) {
                        failures.push(format!("{name} block {bi} {}: {what} differ\n  got  {got}\n  want {}", p.id, pj[what]));
                    }
                }
            }
            gl.route();
            let got_wires = norm(&serde_json::to_value(&gl.wires).unwrap());
            let g = gl.emit().unwrap_or_else(|e| panic!("{name} block {bi}: emit: {e}"));
            checked += 1;
            let want_wires = norm(&blk["wires_routed"]);
            if got_wires != want_wires {
                failures.push(format!("{name} block {bi}: routed wires differ"));
                continue;
            }
            let got = norm(&g.to_json());
            let want = norm(&blk["geo"]);
            for key in ["parts", "power", "wires", "labels", "nc", "texts", "rects"] {
                if got[key] != want[key] {
                    failures.push(format!(
                        "{name} block {bi}: {key} differ\n  got  {}\n  want {}",
                        got[key], want[key]
                    ));
                }
            }
        }
    }
    assert!(checked > 0, "no blocks checked");
    assert!(failures.is_empty(), "{}/{} blocks differ:\n{}", failures.len(), checked, failures.join("\n"));
    eprintln!("{checked} blocks match");
}

/// Structural equality with a tolerance on numbers.
fn approx(a: &Value, b: &Value, eps: f64) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => (x.as_f64().unwrap() - y.as_f64().unwrap()).abs() <= eps,
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| approx(p, q, eps))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, p)| y.get(k).is_some_and(|q| approx(p, q, eps)))
        }
        _ => a == b,
    }
}

/// All numbers as f64, so `8` and `8.0` compare equal.
fn norm(v: &Value) -> Value {
    match v {
        Value::Number(n) => serde_json::json!(n.as_f64().unwrap()),
        Value::Array(a) => Value::Array(a.iter().map(norm).collect()),
        Value::Object(o) => Value::Object(o.iter().map(|(k, x)| (k.clone(), norm(x))).collect()),
        other => other.clone(),
    }
}

/// `add_circuit_to_raw` places a new netlist circuit in the free space of an existing sheet.
/// Depends on `flexlayout::circuit_geos` and `geo::pack_blocks`.
#[test]
fn add_circuit_to_raw_matches_python() {
    use sch_engine::engine::add_circuit_to_raw;

    if !load_index() {
        eprintln!("no symbol library installed - skipping");
        return;
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let cases: Value =
        serde_json::from_str(&std::fs::read_to_string(root.join("engine_add_circuit/cases.json")).unwrap()).unwrap();
    for case in cases.as_array().unwrap() {
        let base = case["base"].as_str().unwrap();
        let raw: Value =
            serde_json::from_str(&std::fs::read_to_string(root.join(base).join("raw.json")).unwrap()).unwrap();
        let (out, errs) = add_circuit_to_raw(&raw, &case["circuit"], case["paper"].as_str().unwrap());
        let want_errs: Vec<String> =
            case["errors"].as_array().unwrap().iter().map(|e| e.as_str().unwrap().to_string()).collect();
        assert_eq!(errs, want_errs, "{base}: errors");
        assert_eq!(norm(&out), norm(&case["out"]), "{base}: raw design");
    }
}
