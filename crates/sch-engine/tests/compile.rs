//! `compile.rs` parity: the raw design JSON of each fixture must compile to the sheet the Python
//! reference produced (modulo uuids and the one deliberate divergence below), and KiCad must report
//! the same ERC violations on both.
//!
//! DELIBERATE DIVERGENCE — fused wire fragments. The reference router emits a pin's stub and the
//! trunk leaving that pin as two wires drawn on top of each other, and its junction inference then
//! reads the two coincident ends as two conductors: it dots the pin, and dots the fragment seam a
//! grid step away, where only a pin and one wire meet. Human sheets never carry those doubled dots.
//! `compile::fuse_segments` fuses overlapping collinear fragments before the wires are written and
//! before junctions are inferred, so this port writes fewer wires and fewer dots than the reference
//! on 17 of the 20 fixtures. The sheets are compared as: everything else byte-for-byte, the wires
//! against the reference's wires put through the same fusion, and the junctions as a subset of the
//! reference's (fusing can only remove a dot, never invent one).

mod common;

use common::{Case, cases, conductors, library, normalise_uuids, point_set, seg_set};
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
        let (got, got_wires, got_junctions) = conductors(&normalise_uuids(&text));
        let (want, want_wires, want_junctions) = conductors(&normalise_uuids(&sheet));
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
        let fused = sch_engine::compile::fuse_segments(&want_wires);
        if seg_set(&got_wires) != seg_set(&fused) {
            failures.push(format!(
                "{name}: wires are not the reference's fused fragments ({} vs {})",
                got_wires.len(),
                fused.len()
            ));
        }
        let (got_j, want_j) = (point_set(&got_junctions), point_set(&want_junctions));
        let invented: Vec<_> = got_j.difference(&want_j).collect();
        if !invented.is_empty() {
            failures.push(format!("{name}: junctions the reference does not have: {invented:?}"));
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
        // the label-overlap issues the reference's text measurement misses; see `tests/check.rs`
        let blocking: Vec<&String> = built
            .issues
            .iter()
            .filter(|s| !s.contains("overlaps label"))
            .collect();
        assert!(blocking.is_empty(), "{name}: {blocking:?}");
        std::fs::write(&theirs, &sheet).unwrap();
        let a = check::run_erc(&kicad, &ours).unwrap();
        let b = check::run_erc(&kicad, &theirs).unwrap();
        // Which unit instance of a multi-unit symbol KiCad blames for an unplaced unit, and the
        // order it lists violations in, follow the sheet's item order, which the fused wires change.
        let strip = |v: Vec<String>| {
            let mut v: Vec<String> = v
                .into_iter()
                .map(|s| s.split(" @(").next().unwrap_or_default().to_string())
                .collect();
            v.sort();
            v
        };
        let (a, b) = (strip(a), strip(b));
        if a != b {
            failures.push(format!("{name}:\n  rust:   {a:?}\n  python: {b:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
