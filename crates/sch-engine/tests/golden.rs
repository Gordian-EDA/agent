//! End-to-end parity against the Python reference: design JSON -> laid-out raw design ->
//! compiled sheet -> report, for every fixture under `tests/fixtures/<name>/`.
//!
//! Ignored until `flexlayout` (S3) and `check`/`compile` (S4) are implemented; drop the
//! `` attributes once `sch_engine::build` no longer panics.

use sch_engine::Library;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const SYMBOL_DIR: &str = "/home/mimi/agent/.local/kicad-10.0.4/AppDir/usr/share/kicad/symbols";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn read(p: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

struct Case {
    name: String,
    design: Value,
    raw: Value,
    report: Value,
}

fn cases() -> Vec<Case> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(fixtures()).unwrap().flatten() {
        let dir = entry.path();
        if !dir.join("raw.json").is_file() {
            continue;
        }
        out.push(Case {
            name: entry.file_name().to_string_lossy().to_string(),
            design: read(&dir.join("design.json")),
            raw: read(&dir.join("raw.json")),
            report: read(&dir.join("report.json")),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn library() -> Option<Library> {
    let dir = PathBuf::from(std::env::var("KICAD_SYMBOL_DIR").unwrap_or_else(|_| SYMBOL_DIR.to_string()));
    dir.is_dir().then(|| Library::load(&dir).expect("index"))
}

/// The laid-out geometry must be identical: same paper, same parts/wires/labels/power at the
/// same integer grid coordinates.
#[test]
fn laid_out_geometry_matches_python() {
    let Some(lib) = library() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let mut failures = Vec::new();
    for case in cases() {
        let out = tmp.path().join(format!("{}.kicad_sch", case.name));
        let report = sch_engine::build(&lib, &case.design, &out).expect("build");
        for key in ["parts", "power", "wires", "labels", "nc", "texts", "rects"] {
            let got = report.raw.get(key).cloned().unwrap_or(Value::Null);
            let want = case.raw.get(key).cloned().unwrap_or(Value::Null);
            if got != want {
                failures.push(format!("{}: {key} differs", case.name));
            }
        }
        let want_paper = case.report["paper"].as_str().unwrap();
        if report.paper != want_paper {
            failures.push(format!("{}: paper {} vs {want_paper}", case.name, report.paper));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Same nets with the same pin members. Auto-generated `N$n` names depend on union-find
/// iteration order, so those are compared as a set of pin groups.
#[test]
fn netlist_matches_python() {
    let Some(lib) = library() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let mut failures = Vec::new();
    for case in cases() {
        let out = tmp.path().join(format!("{}.kicad_sch", case.name));
        let report = sch_engine::build(&lib, &case.design, &out).expect("build");
        let want: BTreeMap<String, BTreeSet<String>> = case.report["netlist"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.as_array().unwrap().iter().map(|p| p.as_str().unwrap().into()).collect()))
            .collect();
        let named = |m: &BTreeMap<String, BTreeSet<String>>| -> BTreeMap<String, BTreeSet<String>> {
            m.iter().filter(|(k, _)| !k.starts_with("N$")).map(|(k, v)| (k.clone(), v.clone())).collect()
        };
        let anon = |m: &BTreeMap<String, BTreeSet<String>>| -> BTreeSet<Vec<String>> {
            m.iter()
                .filter(|(k, _)| k.starts_with("N$"))
                .map(|(_, v)| v.iter().cloned().collect::<Vec<_>>())
                .collect()
        };
        if named(&report.netlist) != named(&want) {
            failures.push(format!("{}: named nets differ", case.name));
        }
        if anon(&report.netlist) != anon(&want) {
            failures.push(format!("{}: unnamed nets differ", case.name));
        }
        let want_issues = case.report["issues"].as_array().unwrap().len();
        if report.issues.len() != want_issues {
            failures.push(format!("{}: {} issues vs {want_issues}: {:?}", case.name, report.issues.len(), report.issues));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The compiled sheet must match the Python one apart from uuids.
#[test]
fn compiled_sheet_matches_python_modulo_uuids() {
    let Some(lib) = library() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let strip = |s: &str| -> String {
        let re = regex_lite();
        s.lines().filter(|l| !re(l)).map(|l| l.trim_end()).collect::<Vec<_>>().join("\n")
    };
    let mut failures = Vec::new();
    for case in cases() {
        let want_path = fixtures().join(&case.name).join("sheet.kicad_sch");
        if !want_path.is_file() {
            continue;
        }
        let out = tmp.path().join(format!("{}.kicad_sch", case.name));
        sch_engine::build(&lib, &case.design, &out).expect("build");
        let got = strip(&std::fs::read_to_string(&out).unwrap());
        let want = strip(&std::fs::read_to_string(&want_path).unwrap());
        if got != want {
            failures.push(case.name.clone());
        }
    }
    assert!(failures.is_empty(), "sheets differ: {failures:?}");
}

/// Lines carrying a uuid (which is random per run).
fn regex_lite() -> impl Fn(&str) -> bool {
    |line: &str| line.trim_start().starts_with("(uuid ")
}
