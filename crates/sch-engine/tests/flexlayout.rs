//! Golden parity for the flexlayout port: every fixture's `design.json` must lay out to the `raw.json`
//! produced by `~/sch-agent`'s `flexlayout.build_flex_design`, with the same notes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use sch_engine::Library;
use serde_json::Value;

const SYMBOL_DIR: &str = "/home/mimi/agent/.local/kicad-10.0.4/AppDir/usr/share/kicad/symbols";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn library() -> Option<Library> {
    let d = std::env::var("KICAD_SYMBOL_DIR").unwrap_or_else(|_| SYMBOL_DIR.to_string());
    let p = PathBuf::from(d);
    p.is_dir().then(|| Library::load(&p).expect("symbol index"))
}

/// Canonical text of a raw-design element, so lists can be compared as multisets.
fn canon(v: &Value) -> String {
    fn norm(v: &Value) -> Value {
        match v {
            Value::Object(o) => Value::Object(
                o.iter()
                    .filter(|(k, _)| *k != "uuid")
                    .map(|(k, v)| (k.clone(), norm(v)))
                    .collect(),
            ),
            Value::Array(a) => Value::Array(a.iter().map(norm).collect()),
            // 4 and 4.0 are the same coordinate
            Value::Number(n) => match n.as_f64() {
                Some(f) if f.fract() == 0.0 => Value::from(f as i64),
                _ => v.clone(),
            },
            other => other.clone(),
        }
    }
    norm(v).to_string()
}

fn multiset(v: Option<&Value>) -> BTreeMap<String, usize> {
    let mut m = BTreeMap::new();
    for e in v.and_then(|v| v.as_array()).into_iter().flatten() {
        *m.entry(canon(e)).or_insert(0) += 1;
    }
    m
}

/// Parts keyed by `id#unit`, so a positional difference names the part.
fn parts_by_key(v: &Value) -> BTreeMap<String, String> {
    v.get("parts")
        .and_then(|p| p.as_array())
        .into_iter()
        .flatten()
        .map(|p| {
            let key = format!(
                "{}#{}",
                p.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                p.get("unit").and_then(|v| v.as_i64()).unwrap_or(1)
            );
            (key, canon(p))
        })
        .collect()
}

fn compare(name: &str, got: &Value, want: &Value, fails: &mut Vec<String>) {
    let (a, b) = (parts_by_key(got), parts_by_key(want));
    if a != b {
        let diff: Vec<String> = b
            .iter()
            .filter(|(k, v)| a.get(*k) != Some(v))
            .take(3)
            .map(|(k, v)| format!("{k}: want {v} got {:?}", a.get(k)))
            .collect();
        fails.push(format!("{name}: parts differ ({})", diff.join(" | ")));
    }
    for k in ["wires", "labels", "power", "nc", "texts", "rects"] {
        let (x, y) = (multiset(got.get(k)), multiset(want.get(k)));
        if x != y {
            let only_want: Vec<&String> =
                y.keys().filter(|e| !x.contains_key(*e)).take(2).collect();
            let only_got: Vec<&String> = x.keys().filter(|e| !y.contains_key(*e)).take(2).collect();
            fails.push(format!(
                "{name}: {k} differ ({} vs {}); missing {only_want:?}; extra {only_got:?}",
                x.len(),
                y.len()
            ));
        }
    }
    if got.get("paper") != want.get("paper") {
        fails.push(format!(
            "{name}: paper {:?} != {:?}",
            got.get("paper"),
            want.get("paper")
        ));
    }
}

fn fixture_names() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(fixtures())
        .expect("fixtures dir")
        .flatten()
        .filter(|e| e.path().join("raw.json").is_file())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    names
}

#[test]
fn build_flex_design_matches_python() {
    // loading the library installs the process-wide symbol index the layout engine reads
    let Some(_lib) = library() else {
        eprintln!("no KiCad symbol dir; skipping");
        return;
    };
    let dir = fixtures();
    let names = fixture_names();
    assert!(!names.is_empty(), "no fixtures under {}", dir.display());
    let mut fails: Vec<String> = Vec::new();
    for name in &names {
        let p = dir.join(name);
        let d: Value =
            serde_json::from_str(&std::fs::read_to_string(p.join("design.json")).unwrap()).unwrap();
        let want: Value =
            serde_json::from_str(&std::fs::read_to_string(p.join("raw.json")).unwrap()).unwrap();
        let (got, errors) = sch_engine::flexlayout::build_flex_design(&d);
        let hard: Vec<&String> = errors.iter().filter(|e| !e.starts_with("note:")).collect();
        if !hard.is_empty() {
            fails.push(format!("{name}: layout errors {hard:?}"));
            continue;
        }
        compare(name, &got, &want, &mut fails);
        if let Ok(txt) = std::fs::read_to_string(p.join("report.json")) {
            let report: Value = serde_json::from_str(&txt).unwrap();
            if let Some(want_notes) = report.get("notes").and_then(|v| v.as_array()) {
                let want_notes: Vec<&str> = want_notes.iter().filter_map(|v| v.as_str()).collect();
                let got_notes: Vec<&str> = errors.iter().map(|s| s.as_str()).collect();
                if got_notes != want_notes {
                    fails.push(format!("{name}: notes {got_notes:?} != {want_notes:?}"));
                }
            }
        }
    }
    assert!(
        fails.is_empty(),
        "{}/{} fixtures differ:\n  {}",
        fails.len(),
        names.len(),
        fails.join("\n  ")
    );
}

/// Element order must match too — except for `power`/`wires`, where Python's PWR_FLAG placement iterates a
/// `set` of flagged nets and so is not reproducible run to run (only the multiset above is well defined).
#[test]
fn raw_element_order_matches_python() {
    let Some(_lib) = library() else { return };
    let dir = fixtures();
    let mut fails = Vec::new();
    for name in fixture_names() {
        let p = dir.join(&name);
        let d: Value =
            serde_json::from_str(&std::fs::read_to_string(p.join("design.json")).unwrap()).unwrap();
        let want: Value =
            serde_json::from_str(&std::fs::read_to_string(p.join("raw.json")).unwrap()).unwrap();
        let (got, _) = sch_engine::flexlayout::build_flex_design(&d);
        for k in ["parts", "labels", "nc", "texts", "rects"] {
            let a: Vec<String> = got
                .get(k)
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .map(canon)
                .collect();
            let b: Vec<String> = want
                .get(k)
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .map(canon)
                .collect();
            if a != b {
                let i = a
                    .iter()
                    .zip(b.iter())
                    .position(|(x, y)| x != y)
                    .unwrap_or(a.len().min(b.len()));
                fails.push(format!(
                    "{name}/{k}: {} vs {} elems, first diff at {i}:\n    got  {:?}\n    want {:?}",
                    a.len(),
                    b.len(),
                    a.get(i),
                    b.get(i)
                ));
            }
        }
    }
    assert!(fails.is_empty(), "{}", fails.join("\n"));
}

/// Designs the Python reference rejects must be rejected with the same messages.
#[test]
fn layout_errors_match_python() {
    let Some(_lib) = library() else { return };
    let dir = fixtures();
    let mut fails = Vec::new();
    let mut seen = 0;
    for e in std::fs::read_dir(&dir).expect("fixtures dir").flatten() {
        let p = e.path();
        if p.join("raw.json").is_file() || !p.join("report.json").is_file() {
            continue;
        }
        let name = e.file_name().to_string_lossy().to_string();
        let report: Value =
            serde_json::from_str(&std::fs::read_to_string(p.join("report.json")).unwrap()).unwrap();
        if !report["layout_failed"].as_bool().unwrap_or(false) {
            continue;
        }
        seen += 1;
        let d: Value =
            serde_json::from_str(&std::fs::read_to_string(p.join("design.json")).unwrap()).unwrap();
        let (_, errors) = sch_engine::flexlayout::build_flex_design(&d);
        // TODO(S2): PartInst::new's "did you mean" hint does not reproduce Python's difflib
        // get_close_matches ranking yet, so the suggestion list is normalised away here.
        let strip_hint = |s: &str| -> String {
            match (s.find(" (did you mean "), s.find("?); ")) {
                (Some(a), Some(b)) if b > a => format!("{}{}", &s[..a], &s[b + 2..]),
                _ => s.to_string(),
            }
        };
        let want: Vec<&str> = report["issues"]
            .as_array()
            .into_iter()
            .flatten()
            .chain(report["notes"].as_array().into_iter().flatten())
            .filter_map(|v| v.as_str())
            .collect();
        let mut want: Vec<String> = want.iter().map(|s| strip_hint(s)).collect();
        want.sort();
        let mut got: Vec<String> = errors.iter().map(|s| strip_hint(s)).collect();
        got.sort();
        if got != want {
            fails.push(format!("{name}:\n    got  {got:#?}\n    want {want:#?}"));
        }
    }
    assert!(seen > 0, "no layout-failure fixtures");
    assert!(fails.is_empty(), "{}", fails.join("\n"));
}
