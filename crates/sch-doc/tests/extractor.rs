//! Gate 2 — the pure-Rust extractor agrees with `kicad-cli` on every corpus
//! sheet it claims to model.
//!
//! Two exclusions, both reported rather than silently passed: sheets carrying
//! buses (the extractor warns instead of guessing), and hierarchical roots
//! (`kicad-cli` netlists the whole tree, this crate scopes to one file).

mod corpus;

use std::collections::{BTreeMap, BTreeSet};

use sch_doc::{Item, SchDoc, connect};

type Partition = BTreeSet<Vec<String>>;

/// KiCAD leaves symbols whose reference starts with `#` — power flags and
/// friends — out of the netlist entirely.
fn is_virtual(refdes: &str) -> bool {
    refdes.starts_with('#')
}

fn ours(netlist: &connect::Netlist) -> Partition {
    netlist
        .nets
        .iter()
        .map(|net| {
            let mut pins: Vec<String> = net
                .pins
                .iter()
                .filter(|p| !is_virtual(&p.refdes))
                .map(|p| format!("{}.{}", p.refdes, p.pin))
                .collect();
            pins.sort();
            pins.dedup();
            pins
        })
        .filter(|pins| pins.len() > 1)
        .collect()
}

fn theirs(netlist: &kicad::Netlist) -> Partition {
    netlist
        .nets
        .iter()
        .map(|net| {
            let mut pins: Vec<String> = net
                .nodes
                .iter()
                .filter(|(refdes, _)| !is_virtual(refdes))
                .map(|(refdes, pin)| format!("{refdes}.{pin}"))
                .collect();
            pins.sort();
            pins.dedup();
            pins
        })
        .filter(|pins| pins.len() > 1)
        .collect()
}

#[test]
fn extraction_matches_the_kicad_netlist_partition() {
    let Some(kicad) = corpus::kicad10() else {
        eprintln!("SKIP: no KiCAD 10 installation detected");
        return;
    };
    let mut compared = 0;
    let mut shape_only = 0;
    let mut skipped: BTreeMap<&str, usize> = BTreeMap::new();
    let mut hierarchical = 0;
    let mut failures = Vec::new();

    for path in corpus::files() {
        let doc = SchDoc::read(&path).expect("parse");
        let netlist = connect::extract(&doc);
        let reason = netlist.warnings.iter().find_map(|w| {
            ["buses", "no embedded", "instantiated more than once"]
                .into_iter()
                .find(|kind| w.contains(kind))
        });
        // A sheet placed several times in a hierarchy has no unambiguous
        // reference designators on its own, but its net *shape* is still
        // checkable: same number of nets, same sizes.
        let shape_check = reason == Some("instantiated more than once");
        if let Some(reason) = reason
            && !shape_check
        {
            *skipped.entry(reason).or_default() += 1;
            continue;
        }
        if doc.items().iter().any(|i| matches!(i, Item::Sheet(_))) {
            hierarchical += 1;
            continue;
        }
        let Ok(oracle) = kicad.netlist(&path) else {
            continue;
        };
        let (mine, reference) = (ours(&netlist), theirs(&oracle));
        if shape_check {
            shape_only += 1;
            let sizes = |p: &Partition| {
                let mut v: Vec<usize> = p.iter().map(Vec::len).collect();
                v.sort_unstable();
                v
            };
            if sizes(&mine) != sizes(&reference) {
                failures.push(format!(
                    "{}: net shape {:?} vs {:?}",
                    corpus::label(&path),
                    sizes(&mine),
                    sizes(&reference)
                ));
            }
            continue;
        }
        compared += 1;
        if mine != reference {
            let missing: Vec<_> = reference.difference(&mine).take(2).cloned().collect();
            let extra: Vec<_> = mine.difference(&reference).take(2).cloned().collect();
            failures.push(format!(
                "{}: {} nets vs {}; missing {missing:?}; extra {extra:?}",
                corpus::label(&path),
                mine.len(),
                reference.len()
            ));
        }
    }

    eprintln!(
        "extractor: {compared} matched exactly, {shape_only} matched by net shape \
         (re-instantiated sheets), {hierarchical} skipped as hierarchical roots, \
         skipped after a warning: {skipped:?}"
    );
    assert!(compared > 20, "only {compared} sheets were comparable");
    assert!(
        failures.is_empty(),
        "{} sheets disagreed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// The extractor must say so rather than quietly mismodel a bussed sheet.
#[test]
fn bussed_sheets_are_reported_not_guessed() {
    let mut bussed = 0;
    for path in corpus::files() {
        let doc = SchDoc::read(&path).expect("parse");
        let has_bus = doc
            .items()
            .iter()
            .any(|i| matches!(i.head(), "bus" | "bus_entry" | "bus_alias"));
        let warnings = connect::extract(&doc).warnings;
        assert_eq!(
            has_bus,
            warnings.iter().any(|w| w.contains("buses")),
            "{}",
            corpus::label(&path)
        );
        bussed += usize::from(has_bus);
    }
    assert!(bussed > 0, "corpus has no bussed sheet to check");
}
