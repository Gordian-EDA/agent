//! Two `place_parts` calls that declare the same net BY NAME must land on one net.
//!
//! The prescribed workflow is one call per block, so almost every signal net of a real
//! board is declared twice — once in the block that drives it, once in the block that
//! receives it. A name the first call drops is a net the second call cannot find, and
//! both pins come back `unconnected-(…)` with the call still reporting `committed`.
//!
//! Gates the join at both arities, because the two have different causes: a net with one
//! pin in the block is dropped by the dangling-port retraction, one with two or more is
//! drawn as a bare wire that carries no name at all.
//!
//! SKIPs cleanly without a KiCAD installation.

use std::collections::BTreeSet;

use kicad::KicadInstallation;
use tempfile::tempdir;
use sch_check::place_parts::PlacePartsInput;
use sch_doc::{SchDoc, connect};

fn block(json: serde_json::Value) -> PlacePartsInput {
    serde_json::from_value(json).unwrap()
}

/// The nets of `doc` as pin sets, power terminals and flags dropped.
fn partition(doc: &SchDoc) -> BTreeSet<Vec<String>> {
    connect::extract(doc)
        .partition()
        .into_iter()
        .map(|net| {
            net.into_iter()
                .filter(|pin| !pin.starts_with('#'))
                .collect::<Vec<_>>()
        })
        .filter(|net: &Vec<String>| !net.is_empty())
        .collect()
}

/// Place each payload in turn on a fresh sheet, asserting every call commits.
fn place_all(env: &KicadInstallation, blocks: &[PlacePartsInput]) -> SchDoc {
    let mut doc = sch_floorplan::live::blank_sheet().expect("blank sheet");
    for (i, input) in blocks.iter().enumerate() {
        let report = sch_floorplan::live::place_parts(env, &mut doc, input)
            .unwrap_or_else(|e| panic!("block {i}: {e}"));
        assert!(report.committed, "block {i} was not committed: {report:?}");
    }
    doc
}

/// Every pin on `net`'s name, as the sheet reads them.
fn pins_on(doc: &SchDoc, net: &str) -> Vec<String> {
    connect::extract(doc)
        .nets
        .iter()
        .filter(|n| n.name == net)
        .flat_map(|n| n.pins.iter().map(|p| format!("{}.{}", p.refdes, p.pin)))
        .filter(|pin| !pin.starts_with('#'))
        .collect()
}

/// One pin each side: the net is dangling in both blocks, and each call must still
/// write the name down so the other can find it.
#[test]
fn single_pin_each_side_joins_by_name() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("no KiCAD environment; skipping");
        return;
    };
    let doc = place_all(
        &env,
        &[
            block(serde_json::json!({"name": "mcu", "parts": [
                {"ref": "U1", "part": "MCU_ST_STM32F1:STM32F103C8Tx",
                 "pins": {"PA9": "FOO", "VDD": "+3V3", "VSS": "GND"}}
            ]})),
            block(serde_json::json!({"name": "header", "parts": [
                {"ref": "J1", "part": "Connector_Generic:Conn_01x02",
                 "pins": {"1": "FOO", "2": "GND"}}
            ]})),
        ],
    );
    assert_eq!(
        pins_on(&doc, "FOO"),
        vec!["J1.1".to_string(), "U1.30".to_string()],
        "FOO did not join across the two calls; sheet nets: {:?}",
        partition(&doc)
    );
}

/// Two pins on the first side: the block wires them together and the name has nowhere
/// to live unless the engine writes it down.
#[test]
fn wired_net_still_joins_a_later_block() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("no KiCAD environment; skipping");
        return;
    };
    let doc = place_all(
        &env,
        &[
            block(serde_json::json!({"name": "mcu", "parts": [
                {"ref": "U1", "part": "MCU_ST_STM32F1:STM32F103C8Tx",
                 "pins": {"PA9": "FOO", "VDD": "+3V3", "VSS": "GND"}},
                {"ref": "R1", "part": "Device:R", "value": "1k",
                 "pins": {"1": "FOO", "2": "GND"}}
            ]})),
            block(serde_json::json!({"name": "header", "parts": [
                {"ref": "J1", "part": "Connector_Generic:Conn_01x02",
                 "pins": {"1": "FOO", "2": "GND"}}
            ]})),
        ],
    );
    assert_eq!(
        pins_on(&doc, "FOO"),
        vec!["J1.1".to_string(), "R1.1".to_string(), "U1.30".to_string()],
        "FOO did not join across the two calls; sheet nets: {:?}",
        partition(&doc)
    );
}

/// A net entirely inside one block is finished business: naming it would put a label on
/// every RC node on the sheet, the pathology that once shipped 5 wires and 124 labels.
#[test]
fn a_nets_own_block_keeps_it_unlabelled() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("no KiCAD environment; skipping");
        return;
    };
    let doc = place_all(
        &env,
        &[block(serde_json::json!({"name": "filter", "parts": [
            {"ref": "R1", "part": "Device:R", "value": "1k", "pins": {"1": "VIN", "2": "MID"}},
            {"ref": "C1", "part": "Device:C", "value": "100n", "pins": {"1": "MID", "2": "GND"}}
        ]}))],
    );
    let labels: Vec<String> = doc.labels().map(|l| sch_doc::unescape(&l.text)).collect();
    assert!(
        !labels.contains(&"MID".to_string()),
        "the block's own node was labelled: {labels:?}"
    );
}

/// A net declared once, on one pin, and never mentioned again is not a contract with a
/// second block — it is an open end. The audit reports it; the sheet must not paper
/// over it with a label nobody asked for.
#[test]
fn a_net_named_once_gains_no_stray_label() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("no KiCAD environment; skipping");
        return;
    };
    let doc = place_all(
        &env,
        &[block(serde_json::json!({"name": "header", "parts": [
            {"ref": "J1", "part": "Connector_Generic:Conn_01x02",
             "pins": {"1": "LONELY", "2": "GND"}}
        ]}))],
    );
    let labels: Vec<String> = doc.labels().map(|l| sch_doc::unescape(&l.text)).collect();
    assert!(
        !labels.contains(&"LONELY".to_string()),
        "an open end was labelled: {labels:?}"
    );
}

/// The joined sheet has to survive KiCAD, not just the extractor: one label scope per
/// net, and no no-connect marker left on the pin the join wired up.
#[test]
fn the_join_raises_no_new_erc_error() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("no KiCAD environment; skipping");
        return;
    };
    let mut doc = place_all(
        &env,
        &[
            block(serde_json::json!({"name": "mcu", "parts": [
                {"ref": "U1", "part": "MCU_ST_STM32F1:STM32F103C8Tx",
                 "pins": {"PA9": "FOO", "PA10": "BAR", "VDD": "+3V3", "VSS": "GND"}},
                {"ref": "R1", "part": "Device:R", "value": "1k",
                 "pins": {"1": "BAR", "2": "GND"}}
            ]})),
            block(serde_json::json!({"name": "header", "parts": [
                {"ref": "J1", "part": "Connector_Generic:Conn_01x03",
                 "pins": {"1": "FOO", "2": "BAR", "3": "GND"}}
            ]})),
        ],
    );
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("joined.kicad_sch");
    doc.write(&path).expect("write");
    let violations: Vec<String> = env
        .erc(&path)
        .expect("erc")
        .violations
        .iter()
        .filter(|v| v.severity == "error")
        .map(|v| v.kind.clone())
        .collect();
    for kind in ["same_local_global_label", "no_connect_connected"] {
        assert!(
            !violations.contains(&kind.to_string()),
            "the join raised {kind}: {violations:?}"
        );
    }
}
