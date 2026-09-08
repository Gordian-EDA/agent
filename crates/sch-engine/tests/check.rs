//! `check.rs` parity: the checker must report the same issues, warnings and netlist as Python for
//! every laid-out fixture design.

mod common;

use common::{Case, cases, library};
use sch_engine::check;

fn strings(v: &serde_json::Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn report_matches_python() {
    let Some(_lib) = library() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let mut failures = Vec::new();
    for Case {
        name, raw, report, ..
    } in cases()
    {
        let built =
            check::build(&raw, &tmp.path().join(format!("{name}.kicad_sch")), None).unwrap();
        let want_issues = strings(&report, "issues");
        let want_warnings = strings(&report, "warnings");
        if built.issues != want_issues {
            failures.push(format!(
                "{name} issues:\n  rust:   {:#?}\n  python: {want_issues:#?}",
                built.issues
            ));
        }
        if built.warnings != want_warnings {
            failures.push(format!(
                "{name} warnings:\n  rust:   {:#?}\n  python: {want_warnings:#?}",
                built.warnings
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The netlist must match exactly, `N$n` numbering included (the union-find insertion order the
/// port reproduces makes Python's numbering deterministic on every fixture).
#[test]
fn netlist_matches_python() {
    let Some(_lib) = library() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let mut failures = Vec::new();
    for Case {
        name, raw, report, ..
    } in cases()
    {
        let built =
            check::build(&raw, &tmp.path().join(format!("{name}.kicad_sch")), None).unwrap();
        let want: std::collections::BTreeMap<String, Vec<String>> =
            serde_json::from_value(report["netlist"].clone()).unwrap();
        let got: std::collections::BTreeMap<String, Vec<String>> = built
            .nets
            .iter()
            .map(|(k, v)| (k.clone(), v.iter().cloned().collect()))
            .collect();
        if got != want {
            failures.push(format!(
                "{name} netlist:\n  rust:   {got:#?}\n  python: {want:#?}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// `resolve_pin_refs`, `annotate_wires` and `netlist_mismatch` on a design written the way the
/// model writes one: `"REF.PIN"` points, stubs, and the error messages the prompt quotes.
#[test]
fn pin_reference_resolution_matches_python() {
    let Some(_lib) = library() else { return };
    let f: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(common::fixtures().join("pinrefs/pinrefs.json")).unwrap(),
    )
    .unwrap();

    let mut d = sch_engine::model::normalize_json(&f["design"]);
    let errors = check::resolve_pin_refs(&mut d, None);
    assert_eq!(errors, strings(&f, "errors"));
    assert_eq!(d, f["resolved"], "resolved design");

    let mut with_ids = f["with_ids"].clone();
    check::annotate_wires(&mut with_ids, None);
    assert_eq!(with_ids, f["annotated"], "annotated wires");

    let nets: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> =
        serde_json::from_value(f["nets"].clone()).unwrap();
    assert_eq!(
        check::netlist_mismatch(&f["design"], &nets),
        strings(&f, "netlist_mismatch")
    );
}
