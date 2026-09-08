//! `compile.rs` parity: the raw design JSON of each fixture must compile to exactly the sheet the
//! Python reference produced (modulo uuids), and KiCad must report the same ERC violations on both.

mod common;

use common::{Case, cases, library, normalise_uuids};
use sch_engine::check;

#[test]
fn compiled_sheet_matches_python() {
    let Some(_lib) = library() else { return };
    let mut failures = Vec::new();
    for Case {
        name, raw, sheet, ..
    } in cases()
    {
        let Some(sheet) = sheet else { continue };
        let mut d = raw.clone();
        let errs = check::resolve_pin_refs(&mut d, None);
        assert!(errs.is_empty(), "{name}: resolve_pin_refs: {errs:?}");
        let des = sch_engine::model::design_from_json(&d, None);
        let (text, comp) = sch_engine::compile::compile_design(des);
        let comp_errs = comp.errors.borrow().clone();
        assert!(comp_errs.is_empty(), "{name}: compile: {comp_errs:?}");
        let (got, want) = (normalise_uuids(&text), normalise_uuids(&sheet));
        if got != want {
            let g: Vec<&str> = got.lines().collect();
            let w: Vec<&str> = want.lines().collect();
            let first = (0..g.len().max(w.len()))
                .find(|i| g.get(*i) != w.get(*i))
                .unwrap_or(0);
            failures.push(format!(
                "{name}: {} vs {} lines, first difference at line {}:\n  rust:   {:?}\n  python: {:?}",
                g.len(),
                w.len(),
                first + 1,
                g.get(first),
                w.get(first)
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// KiCad itself must read the Rust sheet the same way it reads the Python one.
#[test]
fn erc_matches_python() {
    let Some(kicad) = common::kicad_cli() else {
        return;
    };
    let Some(_lib) = library() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let mut failures = Vec::new();
    for Case {
        name, raw, sheet, ..
    } in cases()
    {
        let Some(sheet) = sheet else { continue };
        let ours = tmp.path().join(format!("{name}.kicad_sch"));
        let theirs = tmp.path().join(format!("{name}_py.kicad_sch"));
        let built = check::build(&raw, &ours, None).unwrap();
        assert!(built.issues.is_empty(), "{name}: {:?}", built.issues);
        std::fs::write(&theirs, &sheet).unwrap();
        let a = check::run_erc(&kicad, &ours).unwrap();
        let b = check::run_erc(&kicad, &theirs).unwrap();
        if a != b {
            failures.push(format!("{name}:\n  rust:   {a:?}\n  python: {b:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
