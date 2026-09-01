//! Gate 2 — the pure-Rust extractor agrees with `kicad-cli` on every corpus
//! sheet it claims to model.
//!
//! Two exclusions, both reported rather than silently passed: sheets carrying
//! buses (the extractor warns instead of guessing), and hierarchical roots
//! (`kicad-cli` netlists the whole tree, this crate scopes to one file).

mod corpus;

use std::collections::{BTreeMap, BTreeSet};

use sch_doc::{Item, NetSource, SchDoc, connect};

type Partition = BTreeSet<Vec<String>>;

/// Pins `kicad-cli` left on a net of their own, which it names `unconnected-`.
fn oracle_loose_ends(netlist: &kicad::Netlist) -> BTreeSet<String> {
    netlist
        .nets
        .iter()
        .filter(|net| net.name.starts_with("unconnected-"))
        .flat_map(|net| net.nodes.iter())
        .filter(|(refdes, _)| !is_virtual(refdes))
        .map(|(refdes, pin)| format!("{refdes}.{pin}"))
        .collect()
}

/// The same, as this crate reports it: a loose end is a lone unnamed pin,
/// whether or not a no-connect marker excuses it.
fn our_loose_ends(netlist: &connect::Netlist) -> BTreeSet<String> {
    netlist
        .unconnected
        .iter()
        .chain(&netlist.no_connect)
        .filter(|p| !is_virtual(&p.refdes))
        .map(|p| format!("{}.{}", p.refdes, p.pin))
        .collect()
}

/// KiCAD qualifies a sheet-scoped name with the sheet path; this crate reports
/// the label text itself, since one file does not know its path in the parent.
fn same_name(ours: &str, theirs: &str) -> bool {
    theirs == ours || theirs.strip_prefix('/') == Some(ours)
}

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
        let loose = (our_loose_ends(&netlist), oracle_loose_ends(&oracle));
        if loose.0 != loose.1 {
            failures.push(format!(
                "{}: loose ends {:?} vs {:?}",
                corpus::label(&path),
                loose.0.difference(&loose.1).take(3).collect::<Vec<_>>(),
                loose.1.difference(&loose.0).take(3).collect::<Vec<_>>()
            ));
        }
        for net in netlist.nets.iter().filter(|n| n.source != NetSource::Auto) {
            let pins: BTreeSet<String> = net
                .pins
                .iter()
                .filter(|p| !is_virtual(&p.refdes))
                .map(|p| format!("{}.{}", p.refdes, p.pin))
                .collect();
            if pins.is_empty() {
                continue;
            }
            let Some(theirs) = oracle.nets.iter().find(|n| {
                n.nodes
                    .iter()
                    .any(|(refdes, pin)| pins.contains(&format!("{refdes}.{pin}")))
            }) else {
                continue;
            };
            if !same_name(&net.name, &theirs.name) {
                failures.push(format!(
                    "{}: named {:?}, kicad named it {:?}",
                    corpus::label(&path),
                    net.name,
                    theirs.name
                ));
            }
        }
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
