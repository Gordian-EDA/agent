//! Deterministic schematic facts for the quality harness.
//!
//! ```text
//! sch_facts <project-dir-or-sch>            # symbols + netlist + warnings
//! sch_facts --diff <before> <after>         # net partition delta
//! sch_facts --visual <project-dir-or-sch>   # measured drawing defects
//! ```
//!
//! Prints JSON on stdout. The quality runner is the only consumer, so the shape
//! is flat and stable rather than general.

use std::path::{Path, PathBuf};

use quality_facts::net::{self, Netlist, PinRef};
use quality_facts::sch::Schematic;
use quality_facts::visual;
use serde_json::{Value, json};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let out = match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["--diff", before, after] => diff(Path::new(before), Path::new(after)),
        ["--visual", path] => visual_facts(Path::new(path)),
        [path] if !path.starts_with("--") => facts(Path::new(path)),
        _ => {
            eprintln!(
                "usage: sch_facts <project> | sch_facts --diff <before> <after> \
                 | sch_facts --visual <project>"
            );
            std::process::exit(2);
        }
    };
    println!("{out}");
}

/// Every `.kicad_sch` under `root`, sorted; `root` itself if it is one. Hidden
/// directories hold generated runtime state, not sheets of the design.
fn sheet_paths(root: &Path) -> Vec<PathBuf> {
    if root.is_file() {
        return vec![root.to_path_buf()];
    }
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let hidden = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with('.'));
            if hidden {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "kicad_sch") {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

struct Sheet {
    name: String,
    doc: Schematic,
    netlist: Netlist,
}

fn load(root: &Path) -> (Vec<Sheet>, Vec<String>) {
    let mut sheets = Vec::new();
    let mut errors = Vec::new();
    for path in sheet_paths(root) {
        let name = path
            .strip_prefix(root)
            .ok()
            .filter(|rest| !rest.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new(path.file_name().unwrap_or_default()))
            .to_string_lossy()
            .into_owned();
        match Schematic::read(&path) {
            Ok(doc) => {
                let netlist = net::extract(&doc);
                sheets.push(Sheet { name, doc, netlist });
            }
            Err(error) => errors.push(format!("{name}: {error}")),
        }
    }
    (sheets, errors)
}

fn facts(root: &Path) -> Value {
    let (sheets, errors) = load(root);
    let mut symbols = Vec::new();
    let mut warnings = Vec::new();
    let mut partition: Vec<Vec<String>> = Vec::new();
    let mut unconnected = Vec::new();
    let mut no_connect = Vec::new();

    for sheet in &sheets {
        for symbol in &sheet.doc.symbols {
            let refdes = symbol.refdes().to_string();
            let fields: serde_json::Map<String, Value> = symbol
                .fields
                .iter()
                .map(|field| (field.name.clone(), Value::from(field.value.clone())))
                .collect();
            symbols.push(json!({
                // Always unit-qualified: a multi-unit part must stay
                // distinguishable, and a part that gains or loses units must
                // not silently re-key the ones it kept.
                "key": format!("{refdes}/{}", symbol.unit),
                "ref": refdes,
                "uuid": symbol.uuid,
                "sheet": sheet.name,
                "lib_id": symbol.lib_id,
                "unit": symbol.unit,
                "x": round(symbol.at.x),
                "y": round(symbol.at.y),
                "rot": round(symbol.rot),
                "mirror": format!("{:?}", symbol.mirror),
                "dnp": symbol.dnp,
                "fields": fields,
            }));
        }
        for warning in &sheet.netlist.warnings {
            warnings.push(format!("{}: {warning}", sheet.name));
        }
        partition.extend(sheet.netlist.partition());
        unconnected.extend(sheet.netlist.unconnected.iter().map(PinRef::label));
        no_connect.extend(sheet.netlist.no_connect.iter().map(PinRef::label));
    }
    symbols.sort_by_key(|s| {
        (
            s["sheet"].as_str().unwrap_or("").to_string(),
            s["key"].as_str().unwrap_or("").to_string(),
        )
    });
    partition.sort();
    unconnected.sort();
    no_connect.sort();

    let nets: Vec<&str> = sheets
        .iter()
        .flat_map(|s| s.netlist.nets.iter().map(|n| n.name.as_str()))
        .collect();
    let power_symbols = symbols
        .iter()
        .filter(|s| s["ref"].as_str().is_some_and(|r| r.starts_with('#')))
        .count();

    json!({
        "sheets": sheets.iter().map(|s| &s.name).collect::<Vec<_>>(),
        "symbol_count": symbols.len(),
        "power_symbols": power_symbols,
        "part_count": symbols.len() - power_symbols,
        "symbols": symbols,
        "nets": nets,
        "partition": partition,
        "unconnected_pins": unconnected,
        "no_connect_pins": no_connect,
        "extractor_warnings": warnings,
        "errors": errors,
    })
}

fn diff(before: &Path, after: &Path) -> Value {
    let (before_sheets, _) = load(before);
    let (after_sheets, _) = load(after);
    let delta = net::diff(&merge(&before_sheets), &merge(&after_sheets));
    json!({
        "created": delta.created,
        "removed": delta.removed,
        "merged": delta.merged.iter()
            .map(|(from, to)| json!({"from": from, "to": to}))
            .collect::<Vec<_>>(),
        "split": delta.split.iter()
            .map(|(from, to)| json!({"from": from, "to": to}))
            .collect::<Vec<_>>(),
        "renamed": delta.renamed.iter()
            .map(|(from, to)| json!({"from": from, "to": to}))
            .collect::<Vec<_>>(),
        "pins_now_unconnected": delta.pins_now_unconnected.iter()
            .map(PinRef::label).collect::<Vec<_>>(),
        "pins_now_connected": delta.pins_now_connected.iter()
            .map(PinRef::label).collect::<Vec<_>>(),
        "unchanged": delta.is_empty(),
    })
}

fn visual_facts(root: &Path) -> Value {
    let (sheets, errors) = load(root);
    let mut extent: Option<[f64; 4]> = None;
    let (mut overlaps, mut wires, mut texts) = (Vec::new(), Vec::new(), Vec::new());
    for sheet in &sheets {
        let facts = visual::measure(&sheet.doc);
        let seen = facts.sheet_extent;
        extent = Some(match extent {
            None => seen,
            Some(held) => [
                held[0].min(seen[0]),
                held[1].min(seen[1]),
                held[2].max(seen[2]),
                held[3].max(seen[3]),
            ],
        });
        overlaps.extend(facts.body_overlaps);
        wires.extend(
            facts
                .wires_through_bodies
                .into_iter()
                .map(|(net, reference)| json!({"net": net, "ref": reference})),
        );
        texts.extend(facts.text_collisions.into_iter().map(
            |(reference, field, with)| json!({"ref": reference, "field": field, "with": with}),
        ));
    }
    json!({
        "sheet_extent": extent.unwrap_or([0.0; 4]),
        "body_overlaps": overlaps,
        "wires_through_bodies": wires,
        "text_collisions": texts,
        "errors": errors,
    })
}

/// One netlist over the whole project: the sheets' nets side by side. Cases are
/// single-sheet, where this is that sheet's own netlist; on a multi-sheet
/// project it is a flat union, which is all a partition delta needs.
fn merge(sheets: &[Sheet]) -> Netlist {
    let mut merged = Netlist::default();
    for sheet in sheets {
        merged.nets.extend(sheet.netlist.nets.iter().cloned());
        merged
            .unconnected
            .extend(sheet.netlist.unconnected.iter().cloned());
        merged
            .no_connect
            .extend(sheet.netlist.no_connect.iter().cloned());
    }
    merged.nets.sort_by(|a, b| a.name.cmp(&b.name));
    merged
}

/// A coordinate rounded to the sub-micron, so a file re-saved by KiCad does not
/// read as a move.
fn round(value: f64) -> f64 {
    if value.is_finite() {
        (value * 10_000.0).round() / 10_000.0
    } else {
        0.0
    }
}
