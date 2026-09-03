//! One label scope per net, checked by `kicad-cli sch erc` rather than by the
//! engine's own opinion of what it drew.
//!
//! A block that declares `intent.ports` gets a port pennant at each declared
//! net's exit terminal AND a plain stub label on the same net at its pins. Two
//! scopes for one name is KiCAD's `same_local_global_label`: the drawing claims
//! a join the netlist does not make. A second block that reaches an existing net
//! by name must not flip that net's scope either.
//!
//! SKIPs cleanly without a KiCAD installation.

use std::collections::BTreeMap;
use std::path::Path;

use kicad::KicadInstallation;
use sch_check::place_parts::PlacePartsInput;
use sch_doc::{LabelKind, SchDoc};
use sch_floorplan::live;

fn engine() -> impl PlacementEngine {
    
}

const BLANK: &str = "(kicad_sch\n\
\t(version 20250114)\n\
\t(generator \"test\")\n\
\t(uuid \"4a1c0f2e-0000-4000-8000-0000000000cc\")\n\
\t(paper \"A4\")\n\
\t(lib_symbols)\n\
\t(sheet_instances\n\
\t\t(path \"/\"\n\
\t\t\t(page \"1\")\n\
\t\t)\n\
\t)\n\
)\n";

/// Every ERC violation of the sheet, counted by kind, warnings included: the
/// scope clash is a warning, and the library-table findings of a temp-dir sheet
/// are somebody else's lane.
fn erc_counts(env: &KicadInstallation, path: &Path) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for violation in env.erc(path).expect("erc").violations {
        if violation.kind == "lib_symbol_issues" || violation.kind == "footprint_link_issues" {
            continue;
        }
        *counts.entry(violation.kind).or_default() += 1;
    }
    counts
}

/// The scope each net's labels are drawn in, and a marker for the nets drawn in
/// both — the state that has to be impossible.
fn scopes(doc: &SchDoc) -> BTreeMap<String, &'static str> {
    let mut seen: BTreeMap<String, (bool, bool)> = BTreeMap::new();
    for label in doc.labels() {
        let entry = seen
            .entry(sch_doc::unescape(&label.text))
            .or_insert((false, false));
        match label.kind {
            LabelKind::Local => entry.0 = true,
            LabelKind::Global => entry.1 = true,
            LabelKind::Hier => {}
        }
    }
    seen.into_iter()
        .map(|(net, (local, global))| {
            (
                net,
                match (local, global) {
                    (true, true) => "both",
                    (_, true) => "global",
                    _ => "local",
                },
            )
        })
        .collect()
}

/// The SWD-and-USB corner of a BluePill: an MCU whose reset, debug and USB nets
/// are declared board I/O, wired out to a header.
fn ported_block() -> PlacePartsInput {
    serde_json::from_value(serde_json::json!({
        "name": "swd-usb",
        "parts": [
            {"ref": "U1", "part": "MCU_ST_STM32F1:STM32F103C8Tx", "pins": {
                "PA13": "SWDIO", "PA14": "SWCLK", "NRST": "NRST",
                "PA11": "USB_D-", "PA12": "USB_D+",
                "VDD": "+3V3", "VSS": "GND", "BOOT0": "BOOT0"}},
            {"ref": "J3", "part": "Connector_Generic:Conn_01x04", "pins": {
                "1": "+3V3", "2": "SWCLK", "3": "SWDIO", "4": "GND"}},
            {"ref": "R1", "part": "Device:R", "value": "10k",
             "pins": {"1": "+3V3", "2": "NRST"}},
            {"ref": "C1", "part": "Device:C", "value": "100n",
             "pins": {"1": "NRST", "2": "GND"}}
        ],
        "intent": {
            "flow": "lr",
            "rails": {"+3V3": "top", "GND": "bottom"},
            "ports": {"NRST": "right", "SWDIO": "right", "SWCLK": "right",
                      "USB_D+": "left", "USB_D-": "left", "BOOT0": "left"}
        }
    }))
    .expect("fixture parses")
}

/// A later block joining two of the sheet's nets by name — the case that used to
/// hang a pennant on a net the sheet already drew with plain labels.
fn joining_block() -> PlacePartsInput {
    serde_json::from_value(serde_json::json!({
        "name": "boot-jumper",
        "parts": [
            {"ref": "R7", "part": "Device:R", "value": "10k",
             "pins": {"1": "+3V3", "2": "BOOT0"}},
            {"ref": "R8", "part": "Device:R", "value": "10k",
             "pins": {"1": "BOOT0", "2": "GND"}}
        ]
    }))
    .expect("fixture parses")
}

fn place(env: &KicadInstallation, doc: &mut SchDoc, input: &PlacePartsInput) {
    let report = live::place_parts(env, doc, input, Box::new(engine()), None).expect("place_parts");
    assert!(report.committed, "rolled back — {:?}", report.mismatch);
}

/// A block with declared ports draws each of its nets in one scope, and KiCAD
/// agrees that nothing on the sheet is named twice over.
#[test]
fn a_ported_block_raises_no_same_local_global_label() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCad environment detected");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ported.kicad_sch");
    let mut doc = SchDoc::parse(BLANK).unwrap();

    place(&env, &mut doc, &ported_block());
    doc.write(&path).unwrap();

    let scopes = scopes(&doc);
    assert!(
        !scopes.values().any(|scope| *scope == "both"),
        "a net is drawn in two scopes: {scopes:?}"
    );
    for port in ["NRST", "SWDIO", "SWCLK", "USB_D+", "USB_D-", "BOOT0"] {
        if let Some(scope) = scopes.get(port) {
            assert_eq!(*scope, "global", "declared port {port} is not global");
        }
    }
    let counts = erc_counts(&env, &path);
    assert_eq!(
        counts.get("same_local_global_label"),
        None,
        "kicad still sees a scope clash: {counts:?}"
    );
}

/// Joining an existing net by name keeps the sheet's own scope for it: a plain
/// label stays plain, whatever the second block's router drew.
#[test]
fn joining_a_local_net_by_name_leaves_it_local() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCad environment detected");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("joined.kicad_sch");
    let mut doc = SchDoc::parse(BLANK).unwrap();
    place(&env, &mut doc, &ported_block());
    let before = scopes(&doc);

    place(&env, &mut doc, &joining_block());
    doc.write(&path).unwrap();

    let after = scopes(&doc);
    assert!(
        !after.values().any(|scope| *scope == "both"),
        "the join left a net in two scopes: {after:?}"
    );
    for (net, scope) in &before {
        if let Some(now) = after.get(net) {
            assert_eq!(now, scope, "the join changed `{net}`'s scope");
        }
    }
    let counts = erc_counts(&env, &path);
    assert_eq!(
        counts.get("same_local_global_label"),
        None,
        "kicad sees a scope clash after the join: {counts:?}"
    );
}

/// Nothing the writer draws is shorter than one grid step: a wire that cannot be
/// seen is a wire whose free end KiCAD reports and nobody can find.
#[test]
fn no_placed_wire_is_shorter_than_a_grid_step() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCad environment detected");
        return;
    };
    let mut doc = SchDoc::parse(BLANK).unwrap();
    place(&env, &mut doc, &ported_block());
    place(&env, &mut doc, &joining_block());

    let short: Vec<String> = doc
        .wires()
        .flat_map(|wire| {
            wire.points
                .windows(2)
                .map(|pair| (pair[0], pair[1]))
                .collect::<Vec<_>>()
        })
        .filter(|(a, b)| a.dist(*b) < 1.27 - 1e-6)
        .map(|(a, b)| format!("({},{})-({},{})", a.x, a.y, b.x, b.y))
        .collect();

    assert!(short.is_empty(), "sub-grid wire segments: {short:?}");

    let mut seen = std::collections::BTreeSet::new();
    let mut duplicates = Vec::new();
    for wire in doc.wires() {
        for pair in wire.points.windows(2) {
            let key = |p: geom::Point2| ((p.x * 100.0).round() as i64, (p.y * 100.0).round() as i64);
            let (a, b) = (key(pair[0]), key(pair[1]));
            let segment = if a <= b { (a, b) } else { (b, a) };
            if !seen.insert(segment) {
                duplicates.push(format!("{segment:?}"));
            }
        }
    }
    assert!(duplicates.is_empty(), "the writer drew a segment twice: {duplicates:?}");
}

/// Re-arranging erases the selection's drawing and draws it again, so it can
/// split a net's scope exactly the way a second block can. It must not.
#[test]
fn arranging_keeps_each_nets_scope() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCad environment detected");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("arranged.kicad_sch");
    let mut doc = SchDoc::parse(BLANK).unwrap();
    place(&env, &mut doc, &ported_block());
    let before = scopes(&doc);

    let selection = live::Selection::Refs(vec!["U1".into(), "J3".into(), "R1".into(), "C1".into()]);
    let report =
        live::arrange(
        &env,
        &mut doc,
        &selection,
        None,
        None,
        Box::new(engine()), None).expect("arrange");
    assert!(report.committed, "rolled back — {:?}", report.mismatch);
    doc.write(&path).unwrap();

    let after = scopes(&doc);
    assert!(
        !after.values().any(|scope| *scope == "both"),
        "arranging split a net's scope: {after:?}"
    );
    for (net, scope) in &before {
        if let Some(now) = after.get(net) {
            assert_eq!(now, scope, "arranging changed `{net}`'s scope");
        }
    }
    let counts = erc_counts(&env, &path);
    assert_eq!(counts.get("same_local_global_label"), None, "{counts:?}");
}
