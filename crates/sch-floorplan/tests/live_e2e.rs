//! End-to-end gates for the live-edit surface.
//!
//! Three claims, each checked against KiCAD itself rather than against the engine's
//! own opinion:
//!
//! 1. **Bulk create is truthful.** Every validation fixture, lowered to a
//!    [`PlacePartsInput`] and placed onto a blank sheet, must produce a document whose
//!    pure-Rust net partition equals the one `kicad-cli` exports, with zero ERC errors.
//! 2. **Incremental create is additive.** A block placed onto a hand-drawn KiCAD demo
//!    sheet leaves every existing symbol byte-stable, overlaps nothing, and leaves the
//!    sheet's own nets exactly as they were.
//! 3. **Arranging is idempotent.** Re-arranging that block changes no net and creates
//!    no overlap.
//!
//! SKIPs cleanly without a KiCAD installation.
//!
//! ```sh
//! cargo test -p sch-floorplan --test live_e2e -- --nocapture
//! ```

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use geom::Rect;
use kicad::KicadInstallation;
use kicad_symbol::SymbolTable;
use sch_check::model::{Design, PinTarget};
use sch_check::place_parts::{PartSpec, PlacePartsInput};
use sch_doc::{SchDoc, connect};
use sch_floorplan::contract::PlacementEngine;
use sch_floorplan::live::{self, Selection};

/// The engine the agent ships with, so the gates measure what actually runs.
fn engine() -> impl PlacementEngine {
    cluster_place::ClusterPlace
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(format!("tests/fixtures/validation/{name}.circuit.yaml"))
}

/// One KiCAD demo sheet: hand-drawn, project-local symbol library, five parts.
const DEMO: &str = "share/kicad/demos/simulation/rectifier/rectifier.kicad_sch";

fn demo_sheet() -> Option<PathBuf> {
    let root = Path::new("/home/mimi/agent/.local/kicad-10.0.4/AppDir");
    let path = root.join(DEMO);
    path.is_file().then_some(path)
}

/// A compiled design restated as the tool input an LLM would send: parts, pins, nets —
/// no coordinates. This is the conversion the parity gate rests on.
fn as_input(design: &Design) -> PlacePartsInput {
    let mut parts = Vec::new();
    for block in design.blocks.values() {
        for (refdes, comp) in &block.components {
            let mut spec = PartSpec {
                refdes: refdes.clone(),
                part: comp.part.clone(),
                value: comp.value.clone(),
                footprint: comp.footprint.clone(),
                dnp: comp.dnp,
                ..PartSpec::default()
            };
            let unit_pins = comp.units.values().flat_map(|u| u.iter());
            for (pin, target) in comp.pins.iter().chain(unit_pins) {
                let net = match target {
                    PinTarget::Net(net) => net.clone(),
                    PinTarget::NoConnect => "nc".to_string(),
                };
                spec.pins.insert(pin.clone(), net);
            }
            parts.push(spec);
        }
    }
    PlacePartsInput {
        parts,
        ..PlacePartsInput::default()
    }
}

/// The extractor's partition, as `kicad-cli` would report it: power-symbol pins and
/// empty nets dropped, since the exporter never lists them.
fn extracted_partition(doc: &SchDoc) -> BTreeSet<Vec<String>> {
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

/// The same partition as `kicad-cli` exports it, with its generated single-pin
/// `unconnected-(…)` nets — which are loose ends, not nets — left out.
fn cli_partition(env: &KicadInstallation, path: &Path) -> BTreeSet<Vec<String>> {
    env.netlist(path)
        .expect("kicad-cli netlist")
        .nets
        .into_iter()
        .filter(|net| !net.name.contains("unconnected-"))
        .map(|net| {
            let mut pins: Vec<String> = net
                .nodes
                .iter()
                .map(|(refdes, pin)| format!("{refdes}.{pin}"))
                .collect();
            pins.sort();
            pins.dedup();
            pins
        })
        .filter(|pins| !pins.is_empty())
        .collect()
}

fn save(doc: &mut SchDoc, dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(format!("{name}.kicad_sch"));
    doc.write(&path).expect("write");
    path
}

/// With `LIVE_PARITY_DIR` set, keep both sheets — the one the whole-sheet pipeline
/// emits and the one `place_parts` drew — side by side, so the critic can score the
/// pair. The visual comparison is a VLM call, too slow and too networked to assert on
/// in a test; this is the hook that feeds it.
///
/// ```sh
/// LIVE_PARITY_DIR=/tmp/parity cargo test -p sch-floorplan --release --test live_e2e
/// for f in /tmp/parity/*/*.kicad_sch; do kicad-cli sch export svg -o "${f%/*}" "$f"; done
/// python3 tools/schematic_critic.py /tmp/parity/new/NAME.png --json-only
/// ```
fn dump_parity_pair(env: &KicadInstallation, name: &str, design: &Design, new_sheet: &Path) {
    let Ok(root) = std::env::var("LIVE_PARITY_DIR") else {
        return;
    };
    let (old, new) = (Path::new(&root).join("old"), Path::new(&root).join("new"));
    std::fs::create_dir_all(&old).unwrap();
    std::fs::create_dir_all(&new).unwrap();
    std::fs::copy(new_sheet, new.join(format!("{name}.kicad_sch"))).unwrap();
    let emitted = sch_floorplan::floorplan::emit_strategy(env, design, Box::new(engine()), None)
        .expect("whole-sheet emit");
    std::fs::write(old.join(format!("{name}.kicad_sch")), emitted.sch).unwrap();
}

/// Every validation fixture, placed from nothing: the extractor must agree with
/// `kicad-cli` on the partition, and ERC must report no errors.
#[test]
fn bulk_create_agrees_with_kicad() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCad environment detected");
        return;
    };
    let provider = SymbolTable::from_symbol_dir(env.symbol_dir().to_path_buf());
    let dir = tempfile::tempdir().unwrap();
    let engine = engine();

    let mut checked = 0;
    for entry in std::fs::read_dir(fixture("x").parent().unwrap()).unwrap() {
        let path = entry.unwrap().path();
        let Some(name) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".circuit.yaml"))
        else {
            continue;
        };
        let source = std::fs::read_to_string(&path).unwrap();
        let compiled = circuit_lang::compile(&source, &provider);
        let Some(design) = compiled.design else {
            continue;
        };

        let input = as_input(&design);
        let mut doc = live::blank_sheet().unwrap();
        let report = live::place_parts(&env, &mut doc, &input, &engine).unwrap();
        assert!(
            report.committed,
            "{name}: rolled back — {:?}",
            report.mismatch
        );

        let saved = save(&mut doc, dir.path(), name);
        assert_eq!(
            extracted_partition(&doc),
            cli_partition(&env, &saved),
            "{name}: the extractor and kicad-cli disagree"
        );
        dump_parity_pair(&env, name, &design, &saved);
        let erc = env.erc(&saved).expect("erc");
        assert_eq!(erc.error_count(), 0, "{name}: ERC errors");
        eprintln!(
            "{name}: {} parts, {} nets, {} ERC warnings, {} layout warnings",
            report.placed.len(),
            report.nets.len(),
            erc.warning_count(),
            report.warnings.len()
        );
        checked += 1;
    }
    assert!(checked >= 10, "only {checked} fixtures ran");
}

/// The block used for the incremental gates: an LDO and its passives, asked to sit to
/// the right of the demo's own R1.
fn ldo_block() -> PlacePartsInput {
    let json = serde_json::json!({
        "parts": [
            {"ref": "U9", "part": "Regulator_Linear:MCP1703Ax-330xxTT",
             "pins": {"VI": "VBULK", "VO": "V3P3", "GND": "AGND"}},
            {"ref": "C9", "part": "Device:C", "value": "10u", "pins": {"1": "VBULK", "2": "AGND"}},
            {"ref": "C10", "part": "Device:C", "value": "1u", "pins": {"1": "V3P3", "2": "AGND"}},
            {"ref": "R9", "part": "Device:R", "value": "330", "pins": {"1": "V3P3", "2": "LEDA"}},
            {"ref": "D9", "part": "Device:LED", "value": "red", "pins": {"1": "LEDA", "2": "AGND"}},
            {"ref": "C11", "part": "Device:C", "value": "100n", "pins": {"1": "V3P3", "2": "AGND"}}
        ],
        "intent": {
            "relations": [
                {"kind": "group", "name": "ldo",
                 "members": ["U9", "C9", "C10", "R9", "D9", "C11"],
                 "side": ["right", "R1"]}
            ]
        }
    });
    serde_json::from_value(json).unwrap()
}

const NEW_REFS: &[&str] = &["U9", "C9", "C10", "R9", "D9", "C11"];

/// The bodies of every symbol on the sheet, so a gate can say "nothing overlaps".
fn bodies(doc: &SchDoc) -> Vec<(String, Rect)> {
    doc.symbols()
        .filter(|s| !s.refdes().starts_with('#'))
        .map(|s| {
            // A 5.08 mm core box around the origin: coarse, but two parts sharing a
            // grid position is exactly what it has to catch.
            let (x, y) = (s.at.x, s.at.y);
            (
                s.refdes().to_string(),
                Rect::new(x - 2.0, y - 2.0, x + 2.0, y + 2.0),
            )
        })
        .collect()
}

fn assert_no_overlap(doc: &SchDoc) {
    let bodies = bodies(doc);
    for (i, (a, ra)) in bodies.iter().enumerate() {
        for (b, rb) in bodies.iter().skip(i + 1) {
            assert!(!ra.overlaps(rb), "{a} overlaps {b}");
        }
    }
}

/// Every `(symbol …)` block of the original file, verbatim — the byte-stability oracle.
fn symbol_blocks(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("\t(symbol\n") {
        let tail = &rest[start..];
        let end = tail.find("\n\t)").map(|i| i + 3).unwrap_or(tail.len());
        out.push(tail[..end].to_string());
        rest = &tail[end..];
    }
    out
}

/// Placing a block onto a hand-drawn sheet: the sheet keeps its symbols exactly, the
/// new parts land clear of everything, and the netlist grows without changing.
#[test]
fn incremental_place_is_additive() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCad environment detected");
        return;
    };
    let Some(demo) = demo_sheet() else {
        eprintln!("SKIP: KiCAD demo sheets not installed");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let original = std::fs::read_to_string(&demo).unwrap();
    let mut doc = SchDoc::parse(&original).unwrap();
    let before = connect::extract(&doc);

    let report = live::place_parts(&env, &mut doc, &ldo_block(), &engine()).unwrap();
    assert!(report.committed, "rolled back — {:?}", report.mismatch);
    assert_eq!(
        report.placed,
        NEW_REFS
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
    );

    // Every symbol the demo had is written back from its own bytes.
    let saved = save(&mut doc, dir.path(), "rectifier");
    let after_text = std::fs::read_to_string(&saved).unwrap();
    for block in symbol_blocks(&original) {
        assert!(
            after_text.contains(&block),
            "an existing symbol was rewritten:\n{block}"
        );
    }

    assert_no_overlap(&doc);

    // The sheet's own nets are untouched; only the new block's nets appeared.
    let after = connect::extract(&doc);
    let delta = sch_doc::Netlist::diff(&before, &after);
    assert!(delta.removed.is_empty(), "removed {:?}", delta.removed);
    assert!(delta.split.is_empty(), "split {:?}", delta.split);
    assert!(delta.merged.is_empty(), "merged {:?}", delta.merged);
    assert!(
        delta.pins_now_unconnected.is_empty(),
        "disconnected {:?}",
        delta.pins_now_unconnected
    );
    assert!(!delta.created.is_empty(), "the block added no net");
    assert_eq!(
        extracted_partition(&doc),
        cli_partition(&env, &saved),
        "the extractor and kicad-cli disagree after the graft"
    );
}

/// Re-arranging the block just placed must not change a single net.
#[test]
fn arrange_is_idempotent_on_connectivity() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCad environment detected");
        return;
    };
    let Some(demo) = demo_sheet() else {
        eprintln!("SKIP: KiCAD demo sheets not installed");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let mut doc = SchDoc::read(&demo).unwrap();
    let placed = live::place_parts(&env, &mut doc, &ldo_block(), &engine()).unwrap();
    assert!(placed.committed, "{:?}", placed.mismatch);
    let before = connect::extract(&doc);

    let selection = Selection::Refs(NEW_REFS.iter().map(|s| s.to_string()).collect());
    let report = live::arrange(&env, &mut doc, &selection, &engine()).unwrap();
    assert!(report.committed, "rolled back — {:?}", report.mismatch);
    assert!(sch_doc::Netlist::diff(&before, &connect::extract(&doc)).is_empty());
    assert_no_overlap(&doc);

    let saved = save(&mut doc, dir.path(), "arranged");
    assert_eq!(extracted_partition(&doc), cli_partition(&env, &saved));
    assert_eq!(env.erc(&saved).expect("erc").error_count(), 0);
}
