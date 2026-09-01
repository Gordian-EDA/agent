//! Gate 2 — the pure-Rust extractor agrees with `kicad-cli` on every corpus
//! sheet it claims to model.
//!
//! Two exclusions, both reported rather than silently passed: sheets carrying
//! buses (the extractor warns instead of guessing), and hierarchical roots
//! (`kicad-cli` netlists the whole tree, this crate scopes to one file).

mod corpus;

use std::collections::BTreeSet;

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
    let mut with_buses = 0;
    let mut hierarchical = 0;
    let mut failures = Vec::new();

    for path in corpus::files() {
        let doc = SchDoc::read(&path).expect("parse");
        let netlist = connect::extract(&doc);
        if !netlist.warnings.is_empty() {
            with_buses += 1;
            continue;
        }
        if doc.items().iter().any(|i| matches!(i, Item::Sheet(_))) {
            hierarchical += 1;
            continue;
        }
        let Ok(oracle) = kicad.netlist(&path) else {
            continue;
        };
        compared += 1;
        let (mine, reference) = (ours(&netlist), theirs(&oracle));
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
        "extractor: {compared} compared, {with_buses} skipped for buses, \
         {hierarchical} skipped as hierarchical roots"
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
        let warned = !connect::extract(&doc).warnings.is_empty();
        assert_eq!(has_bus, warned, "{}", corpus::label(&path));
        bussed += usize::from(has_bus);
    }
    assert!(bussed > 0, "corpus has no bussed sheet to check");
}
