//! `extract.rs` parity: loading a sheet the engine wrote and compiling it again must reproduce the
//! file byte-for-byte apart from the uuids the compiler regenerates (pins, wires, junctions).
//!
//! Verified against the Python reference first: `extract.load` + `compile_design` on
//! `~/sch-agent/out/runs/bluepill.kicad_sch` reproduces it exactly modulo uuids. The same does NOT
//! hold for arbitrary human sheets - KiCad wraps `(xy ...)` points differently from this
//! pretty-printer - so the round trip is asserted on engine-written sheets only.

mod common;

use common::{Case, cases, library, normalise_uuids};

#[test]
fn round_trip_is_byte_for_byte() {
    let Some(_lib) = library() else { return };
    let mut failures = Vec::new();
    for Case { name, sheet, .. } in cases() {
        let Some(sheet) = sheet else { continue };
        let des = sch_engine::extract::parse(&sheet).unwrap();
        let (text, comp) = sch_engine::compile::compile_design(des);
        let errs = comp.errors.borrow().clone();
        assert!(errs.is_empty(), "{name}: {errs:?}");
        let (got, want) = (normalise_uuids(&text), normalise_uuids(&sheet));
        if got != want {
            let g: Vec<&str> = got.lines().collect();
            let w: Vec<&str> = want.lines().collect();
            let first = (0..g.len().max(w.len()))
                .find(|i| g.get(*i) != w.get(*i))
                .unwrap_or(0);
            failures.push(format!(
                "{name}: first difference at line {}:\n  round-tripped: {:?}\n  original:      {:?}",
                first + 1,
                g.get(first),
                w.get(first)
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The parsed design must carry what Python's `extract` reported for the same sheet.
#[test]
fn parsed_design_matches_python() {
    let Some(_lib) = library() else { return };
    let mut failures = Vec::new();
    for Case {
        name,
        sheet,
        extract,
        ..
    } in cases()
    {
        let (Some(sheet), Some(want)) = (sheet, extract.as_object()) else {
            continue;
        };
        let des = sch_engine::extract::parse(&sheet).unwrap();
        let n = |k: &str| want.get(k).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let mut got: Vec<(String, String)> = vec![
            ("title".into(), des.title.clone()),
            ("paper".into(), des.paper.clone()),
            ("uuid".into(), des.uuid.clone()),
            ("sheet_path_uuid".into(), des.sheet_path_uuid.clone()),
            ("project".into(), des.project.clone()),
        ];
        got.retain(|(k, _)| want.contains_key(k.as_str()));
        for (k, v) in &got {
            let w = want[k.as_str()].as_str().unwrap_or("");
            if v != w {
                failures.push(format!("{name}: {k}: rust {v:?} vs python {w:?}"));
            }
        }
        for (k, v) in [
            ("n_parts", des.parts.len()),
            ("n_wires", des.wires.len()),
            ("n_power", des.power.len()),
            ("n_labels", des.labels.len()),
        ] {
            if want.contains_key(k) && v != n(k) {
                failures.push(format!("{name}: {k}: rust {v} vs python {}", n(k)));
            }
        }
        if let Some(uuids) = want.get("part_uuids").and_then(|v| v.as_object()) {
            // keys are "REF#unit"
            for (key, u) in uuids {
                let (id, unit) = key.split_once('#').unwrap_or((key.as_str(), "1"));
                let unit: i32 = unit.parse().unwrap_or(1);
                let got = des
                    .parts
                    .iter()
                    .find(|p| p.id == id && p.unit == unit)
                    .map(|p| p.uuid.clone())
                    .unwrap_or_default();
                if got != u.as_str().unwrap_or("") {
                    failures.push(format!(
                        "{name}: part {key} uuid: rust {got:?} vs python {u:?}"
                    ));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
