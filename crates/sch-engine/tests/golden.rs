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

/// Python's `Geo` keeps whole grid steps as ints and stub arithmetic as floats; Rust is all `f64`.
/// The distinction carries no geometry, so compare numbers by value.
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
    let dir =
        PathBuf::from(std::env::var("KICAD_SYMBOL_DIR").unwrap_or_else(|_| SYMBOL_DIR.to_string()));
    dir.is_dir().then(|| Library::load(&dir).expect("index"))
}

/// Multiset of canonical element texts, for lists whose order Python does not fix.
fn multiset(v: Option<&Value>) -> BTreeMap<String, usize> {
    let mut m = BTreeMap::new();
    for e in v.and_then(|v| v.as_array()).into_iter().flatten() {
        *m.entry(canon(e).to_string()).or_insert(0) += 1;
    }
    m
}

/// The laid-out geometry must be identical: same paper, same parts in the same order, and the same
/// wires/labels/power/no-connects/texts.
///
/// Only `parts` is compared positionally. Python emits power symbols, their wires and the PWR_FLAGs
/// while iterating `set`s, so their order changes with `PYTHONHASHSEED` between runs of the
/// reference itself; those lists are compared as multisets.
#[test]
fn laid_out_geometry_matches_python() {
    let Some(lib) = library() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let mut failures = Vec::new();
    for case in cases() {
        let out = tmp.path().join(format!("{}.kicad_sch", case.name));
        let report = sch_engine::build(&lib, &case.design, &out).expect("build");
        let (got_parts, want_parts) = (
            canon(report.raw.get("parts").unwrap_or(&Value::Null)),
            canon(case.raw.get("parts").unwrap_or(&Value::Null)),
        );
        if got_parts != want_parts {
            let first = got_parts
                .as_array()
                .zip(want_parts.as_array())
                .and_then(|(g, w)| g.iter().zip(w).position(|(a, b)| a != b))
                .map(|i| format!(" (first at {i}: {} vs {})", got_parts[i], want_parts[i]))
                .unwrap_or_default();
            failures.push(format!("{}: parts differ{first}", case.name));
        }
        for key in ["power", "wires", "labels", "nc", "texts", "rects"] {
            let (got, want) = (multiset(report.raw.get(key)), multiset(case.raw.get(key)));
            if got != want {
                let missing: Vec<&String> = want
                    .keys()
                    .filter(|k| !got.contains_key(*k))
                    .take(2)
                    .collect();
                let extra: Vec<&String> = got
                    .keys()
                    .filter(|k| !want.contains_key(*k))
                    .take(2)
                    .collect();
                failures.push(format!(
                    "{}: {key} differs (missing {missing:?}, extra {extra:?})",
                    case.name
                ));
            }
        }
        let want_paper = case.report["paper"].as_str().unwrap();
        if report.paper != want_paper {
            failures.push(format!(
                "{}: paper {} vs {want_paper}",
                case.name, report.paper
            ));
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
            .map(|(k, v)| {
                (
                    k.clone(),
                    v.as_array()
                        .unwrap()
                        .iter()
                        .map(|p| p.as_str().unwrap().into())
                        .collect(),
                )
            })
            .collect();
        let named = |m: &BTreeMap<String, BTreeSet<String>>| -> BTreeMap<String, BTreeSet<String>> {
            m.iter()
                .filter(|(k, _)| !k.starts_with("N$"))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
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
            failures.push(format!(
                "{}: {} issues vs {want_issues}: {:?}",
                case.name,
                report.issues.len(),
                report.issues
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The compiled sheet must contain the same elements as the Python one, apart from uuids
/// (and apart from the element order Python leaves to `set` iteration).
#[test]
fn compiled_sheet_matches_python_modulo_uuids() {
    let Some(lib) = library() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let mut failures = Vec::new();
    for case in cases() {
        let want_path = fixtures().join(&case.name).join("sheet.kicad_sch");
        if !want_path.is_file() {
            continue;
        }
        let out = tmp.path().join(format!("{}.kicad_sch", case.name));
        sch_engine::build(&lib, &case.design, &out).expect("build");
        let got = sheet_elements(&std::fs::read_to_string(&out).unwrap());
        let want = sheet_elements(&std::fs::read_to_string(&want_path).unwrap());
        if got != want {
            let missing: Vec<&String> = want
                .keys()
                .filter(|k| !got.contains_key(*k))
                .take(2)
                .collect();
            let extra: Vec<&String> = got
                .keys()
                .filter(|k| !want.contains_key(*k))
                .take(2)
                .collect();
            failures.push(format!(
                "{}: missing {missing:?}, extra {extra:?}",
                case.name
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "sheets differ:\n{}",
        failures.join("\n")
    );
}

/// Top-level sheet elements as a multiset of their text, with every `uuid` node dropped.
fn sheet_elements(text: &str) -> BTreeMap<String, usize> {
    fn is_uuid(s: &str) -> bool {
        let s = s.strip_prefix('/').unwrap_or(s);
        s.len() == 36
            && s.bytes().enumerate().all(|(i, c)| {
                if matches!(i, 8 | 13 | 18 | 23) {
                    c == b'-'
                } else {
                    c.is_ascii_hexdigit()
                }
            })
    }
    fn is_auto_ref(s: &str) -> bool {
        s.len() > 4
            && (s.starts_with("#PWR") || s.starts_with("#FLG"))
            && s[4..].bytes().all(|c| c.is_ascii_digit())
    }
    fn strip(node: &sch_engine::sexp::Sexp) -> sch_engine::sexp::Sexp {
        match node {
            sch_engine::sexp::Sexp::List(items) => sch_engine::sexp::Sexp::List(
                items
                    .iter()
                    .filter(|c| c.tag() != "uuid")
                    .map(strip)
                    .collect(),
            ),
            // the sheet-path uuid appears as an atom inside (instances (project (path "/<uuid>" ..)))
            sch_engine::sexp::Sexp::Str(s) if is_uuid(s) => {
                sch_engine::sexp::Sexp::Str("<uuid>".into())
            }
            // auto-annotated power/flag references are numbered in the order the symbols were
            // emitted, which Python leaves to `set` iteration
            sch_engine::sexp::Sexp::Str(s) if is_auto_ref(s) => {
                sch_engine::sexp::Sexp::Str(format!("{}nn", &s[..4]))
            }
            other => other.clone(),
        }
    }
    let root = sch_engine::sexp::loads(text).expect("sheet parses");
    let mut m = BTreeMap::new();
    for child in root.as_list().unwrap().iter().filter(|c| c.is_list()) {
        *m.entry(sch_engine::sexp::dumps(&strip(child), 0))
            .or_insert(0) += 1;
    }
    m
}
