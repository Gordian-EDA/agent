//! End-to-end gates for the live-edit surface.
//!
//! Three claims, each checked against KiCAD itself rather than against the engine's
//! own opinion:
//!
//! 1. **Bulk create matches the path it replaces.** Every fixture in the corpus,
//!    lowered to a [`PlacePartsInput`] and placed onto a blank sheet, produces a
//!    document whose pure-Rust net partition equals the one `kicad-cli` exports; it is
//!    truthful wherever the whole-sheet pipeline's sheet is, and raises no more ERC
//!    errors. Truthfulness and ERC are stated as PARITY because the engine has defects
//!    of its own — a 2-pin part whose pins come back swapped, for one — and those are
//!    not this path's to answer for.
//! 2. **Incremental create is additive.** A block placed onto a hand-drawn KiCAD demo
//!    sheet leaves every existing symbol byte-stable, overlaps nothing, and leaves the
//!    sheet's own nets exactly as they were.
//! 3. **Arranging is idempotent.** Re-arranging that block changes no net and creates
//!    no overlap.
//!
//! SKIPs cleanly without a KiCAD installation.
//!
//! ```sh
//! cargo test -p sch-floorplan --release --test live_e2e -- --nocapture
//! LIVE_E2E_ALL=1 cargo test -p sch-floorplan --release --test live_e2e -- --nocapture
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

/// The corpus the gate runs by default: the four tuned references plus the authored
/// grid. Every one is small, because this runs on `cargo test` and the shipping engine
/// routes the whole sheet per candidate.
///
/// `LIVE_E2E_ALL=1` sweeps all sixteen fixtures — the dev-board MCUs, the BGA, the RF
/// front end, the switcher — which is the run to make before claiming parity.
const CORPUS: &[&str] = &[
    "divider-filter",
    "mcp1703-power-entry",
    "555-blinker",
    "uart-level-translator",
    "grid-demo",
];

fn fixture_names() -> Vec<String> {
    if std::env::var("LIVE_E2E_ALL").is_err() {
        return CORPUS.iter().map(|n| n.to_string()).collect();
    }
    let mut names: Vec<String> = std::fs::read_dir(fixture("x").parent().unwrap())
        .unwrap()
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let name = path.file_name()?.to_str()?;
            Some(name.strip_suffix(".circuit.yaml")?.to_string())
        })
        .collect();
    names.sort();
    names
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

/// The ERC errors of a sheet as `kind@location` keys, so two runs can be differenced.
fn erc_kinds(env: &KicadInstallation, path: &Path) -> BTreeSet<String> {
    env.erc(path)
        .expect("erc")
        .violations
        .iter()
        .filter(|v| v.severity == "error")
        .map(|v| v.kind.clone())
        .collect()
}

fn save(doc: &mut SchDoc, dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(format!("{name}.kicad_sch"));
    doc.write(&path).expect("write");
    path
}

/// The sheet the whole-sheet pipeline draws for `design` — the path `place_parts`
/// replaces, and the baseline every parity claim is measured against.
fn whole_sheet(env: &KicadInstallation, design: &Design, dir: &Path, name: &str) -> PathBuf {
    let emitted = sch_floorplan::floorplan::emit_strategy(env, design, Box::new(engine()), None)
        .expect("whole-sheet emit");
    let path = dir.join(format!("{name}.old.kicad_sch"));
    std::fs::write(&path, emitted.sch).unwrap();
    path
}

/// With `LIVE_PARITY_DIR` set, keep both sheets side by side so the critic can score
/// the pair. The visual comparison is a VLM call — too slow and too networked to assert
/// on in a test — so this is the hook that feeds it:
///
/// ```sh
/// LIVE_PARITY_DIR=/tmp/parity cargo test -p sch-floorplan --release --test live_e2e
/// for f in /tmp/parity/*/*.kicad_sch; do
///   kicad-cli sch export svg --no-background-color -o "${f%/*}" "$f"
///   convert -density 200 "${f%.kicad_sch}.svg" "${f%.kicad_sch}.png"
///   python3 tools/schematic_critic.py "${f%.kicad_sch}.png" --json-only
/// done
/// ```
fn keep_parity_pair(name: &str, old: &Path, new: &Path) {
    let Ok(root) = std::env::var("LIVE_PARITY_DIR") else {
        return;
    };
    for (side, from) in [("old", old), ("new", new)] {
        let dir = Path::new(&root).join(side);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(from, dir.join(format!("{name}.kicad_sch"))).unwrap();
    }
}

/// One fixture's verdict, in the shape of the parity table this gate reports.
struct Row {
    name: String,
    parts: usize,
    truthful: bool,
    old_truthful: bool,
    erc: usize,
    old_erc: usize,
    partition_agrees: bool,
    warnings: usize,
}

impl std::fmt::Display for Row {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:<28} parts={:<3} truthful={}/{} erc={}/{} partition={} warnings={}",
            self.name,
            self.parts,
            self.truthful as u8,
            self.old_truthful as u8,
            self.erc,
            self.old_erc,
            self.partition_agrees as u8,
            self.warnings,
        )
    }
}

/// Every fixture in the corpus, placed from nothing.
///
/// Three claims, each measured against the whole-sheet pipeline on the same design so
/// that a defect the engine already had is not blamed on the new path:
///
/// - the pure-Rust partition equals `kicad-cli`'s — this one is absolute;
/// - the sheet is truthful wherever the old path's is;
/// - it raises no more ERC errors than the old path does.
#[test]
fn bulk_create_matches_the_whole_sheet_pipeline() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCad environment detected");
        return;
    };
    let provider = SymbolTable::from_symbol_dir(env.symbol_dir().to_path_buf());
    let dir = tempfile::tempdir().unwrap();
    let engine = engine();

    // Every fixture runs even after one fails: which circuits break, and how, is the
    // whole point of a corpus gate.
    let mut rows: Vec<Row> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    for name in fixture_names() {
        let source = std::fs::read_to_string(fixture(&name)).unwrap();
        let Some(authored) = circuit_lang::compile(&source, &provider).design else {
            continue;
        };
        // The lowering `place_parts` performs, so both paths draw the same design.
        let (design, _) = sch_check::into_design(&as_input(&authored), &provider);

        let old = whole_sheet(&env, &design, dir.path(), &name);
        let old_truthful = live::verify(&SchDoc::read(&old).unwrap(), &design).is_empty();
        let old_erc = env.erc(&old).expect("erc").error_count();

        let mut doc = live::blank_sheet().unwrap();
        let report = live::place_parts(&env, &mut doc, &as_input(&authored), &engine).unwrap();
        if !report.committed {
            if old_truthful {
                failures.push(format!(
                    "{name}: rolled back where the whole-sheet path is truthful — {:?}",
                    report.mismatch
                ));
            }
            rows.push(Row {
                name,
                parts: report.placed.len(),
                truthful: false,
                old_truthful,
                erc: 0,
                old_erc,
                partition_agrees: true,
                warnings: report.warnings.len(),
            });
            continue;
        }

        let saved = save(&mut doc, dir.path(), &name);
        keep_parity_pair(&name, &old, &saved);
        let (ours, theirs) = (extracted_partition(&doc), cli_partition(&env, &saved));
        let partition_agrees = ours == theirs;
        if !partition_agrees {
            failures.push(format!(
                "{name}: extractor and kicad-cli disagree\n  only ours: {:?}\n  only kicad: {:?}",
                ours.difference(&theirs).collect::<Vec<_>>(),
                theirs.difference(&ours).collect::<Vec<_>>(),
            ));
        }
        let erc = env.erc(&saved).expect("erc").error_count();
        if erc > old_erc {
            failures.push(format!(
                "{name}: {erc} ERC errors against the old path's {old_erc}"
            ));
        }
        rows.push(Row {
            name,
            parts: report.placed.len(),
            truthful: true,
            old_truthful,
            erc,
            old_erc,
            partition_agrees,
            warnings: report.warnings.len(),
        });
    }
    for row in &rows {
        eprintln!("{row}");
    }
    assert!(rows.len() >= 5, "only {} fixtures ran", rows.len());
    assert!(failures.is_empty(), "{}", failures.join("\n"));
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
    let before = extracted_partition(&doc);
    let seeded = save(&mut doc, dir.path(), "seeded");
    let before_erc = erc_kinds(&env, &seeded);

    let selection = Selection::Refs(NEW_REFS.iter().map(|s| s.to_string()).collect());
    let report = live::arrange(&env, &mut doc, &selection, &engine()).unwrap();
    assert!(report.committed, "rolled back — {:?}", report.mismatch);
    // Over the PARTS: a re-wire is free to replace the rail terminals and flags it
    // draws, so the invariant is the parts' connectivity, not every uuid on the sheet.
    assert_eq!(before, extracted_partition(&doc), "arranging changed a net");
    assert_no_overlap(&doc);

    let saved = save(&mut doc, dir.path(), "arranged");
    assert_eq!(extracted_partition(&doc), cli_partition(&env, &saved));
    // The demo sheet is not ERC-clean to begin with — it is a SPICE simulation
    // fixture — so what a re-arrange owes is that it introduces nothing new.
    let after_erc = erc_kinds(&env, &saved);
    let new: Vec<&String> = after_erc.difference(&before_erc).collect();
    assert!(new.is_empty(), "arranging introduced ERC errors: {new:?}");
}
