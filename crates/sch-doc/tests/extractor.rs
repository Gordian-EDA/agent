//! Gate 2 — the pure-Rust extractor agrees with `kicad-cli` on every corpus
//! sheet it claims to model.
//!
//! Two exclusions, both reported rather than silently passed: sheets carrying
//! buses (the extractor warns instead of guessing), and hierarchical roots,
//! whose partition `kicad-cli` takes over the whole tree while this crate
//! scopes to one file — those are still held to their loose ends.

mod corpus;

use std::collections::{BTreeMap, BTreeSet};

use sch_doc::{Item, SchDoc, connect};

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
    let mut unreadable: Vec<String> = Vec::new();
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
        let hierarchical_root = doc.items().iter().any(|i| matches!(i, Item::Sheet(_)));
        let oracle = match kicad.netlist(&path) {
            Ok(oracle) => oracle,
            Err(err) => {
                unreadable.push(format!("{}: {err}", corpus::label(&path)));
                continue;
            }
        };
        // A hierarchical root's partition is not comparable — kicad-cli
        // netlists the whole tree — but its loose ends are: a pin this file
        // calls loose has to be loose there too.
        if hierarchical_root {
            hierarchical += 1;
            let (mine, theirs) = (our_loose_ends(&netlist), oracle_loose_ends(&oracle));
            if !mine.is_subset(&theirs) {
                failures.push(format!(
                    "{}: called {:?} loose, kicad did not",
                    corpus::label(&path),
                    mine.difference(&theirs).take(3).collect::<Vec<_>>()
                ));
            }
            continue;
        }
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
        for net in &netlist.nets {
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
         (re-instantiated sheets), {hierarchical} hierarchical roots checked for \
         loose ends only, \
         skipped after a warning: {skipped:?}"
    );
    assert!(
        unreadable.is_empty(),
        "kicad-cli could not netlist {} sheets, which would silently shrink this \
         gate: {unreadable:?}",
        unreadable.len()
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

/// What the extractor may never do, on *any* sheet: connect two pins KiCAD
/// keeps apart.
///
/// Buses, hierarchy and missing definitions all make the extractor see *less*
/// connectivity than KiCAD, never more, so on every sheet — including the ones
/// the exact gate has to skip — each of its nets must fit inside one of
/// KiCAD's. Only a re-instantiated sheet is exempt, because there the two sides
/// do not even agree on reference designators.
#[test]
fn no_sheet_is_ever_over_connected() {
    let Some(kicad) = corpus::kicad10() else {
        eprintln!("SKIP: no KiCAD 10 installation detected");
        return;
    };
    let mut checked = 0;
    let mut ambiguous = 0;
    let mut unreadable = 0;
    let mut failures = Vec::new();
    for path in corpus::files() {
        let doc = SchDoc::read(&path).expect("parse");
        let netlist = connect::extract(&doc);
        if netlist
            .warnings
            .iter()
            .any(|w| w.contains("instantiated more than once") || w.contains("not unique"))
        {
            ambiguous += 1;
            continue;
        }
        let Ok(oracle) = kicad.netlist(&path) else {
            unreadable += 1;
            continue;
        };
        checked += 1;
        let theirs = theirs(&oracle);
        for ours in ours(&netlist) {
            if !theirs
                .iter()
                .any(|net| ours.iter().all(|p| net.contains(p)))
            {
                failures.push(format!(
                    "{}: {:?} is not inside any kicad net",
                    corpus::label(&path),
                    &ours[..ours.len().min(6)]
                ));
                break;
            }
        }
    }
    eprintln!(
        "over-connection: {checked} sheets checked, {ambiguous} with ambiguous \
         references, {unreadable} kicad-cli could not netlist"
    );
    assert_eq!(
        unreadable, 0,
        "kicad-cli could not netlist {unreadable} sheets"
    );
    assert!(checked > 90, "only {checked} sheets were checked");
    assert!(
        failures.is_empty(),
        "{} sheets over-connected:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// A local label and a global label of one name are ONE net, and a hierarchical
/// label joins them too — the scope is about where the name reaches beyond this
/// sheet, not about whether it reaches across it.
///
/// This was reported as an extractor blind spot: that the two scopes are separate
/// nets and the guard therefore cannot see a clash. `kicad-cli` says otherwise, so
/// the merge in `connect::names` is correct and this pins it — what the clash
/// actually costs is the `same_local_global_label` ERC warning, which is a rule
/// about the drawing and is why `enforce_label_scopes` exists.
#[test]
fn one_name_in_two_scopes_is_one_net_here_and_in_kicad() {
    let Some(kicad) = kicad::KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCAD installation");
        return;
    };
    let source = corpus::repo_roots()
        .into_iter()
        .map(|root| root.join("crates/kicad/tests/fixtures/rc_pair.kicad_sch"))
        .find(|path| path.is_file())
        .expect("the rc_pair fixture");
    let text = std::fs::read_to_string(&source).expect("read fixture");
    let dir = tempfile::tempdir().expect("tempdir");

    for (scope, head) in [
        ("global", "global_label \"FOO\"\n\t\t(shape input)"),
        ("hierarchical", "hierarchical_label \"FOO\"\n\t\t(shape input)"),
    ] {
        let clashed = text
            .replace("(label \"VIN\"", "(label \"FOO\"")
            .replace("(label \"VOUT\"", &format!("({head}"));
        let path = dir.path().join(format!("{scope}.kicad_sch"));
        std::fs::write(&path, &clashed).expect("write fixture");

        let doc = SchDoc::read(&path).expect("parse");
        assert_eq!(
            ours(&connect::extract(&doc)),
            theirs(&kicad.netlist(&path).expect("kicad-cli netlist")),
            "{scope} label of the same name",
        );
    }
}
