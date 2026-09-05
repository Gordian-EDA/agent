//! Junction dots sit exactly where conductors meet, on every corpus sheet.
//!
//! KiCAD's rule ([`sch_doc::Meet`]): a dot belongs where three conductors meet and
//! nowhere else. A dot at a plain bend reads as a branch that is not on the sheet;
//! a missing one where a wire ends on another wire's interior reads as a crossing,
//! which is a different netlist. Both were endemic — 167 of 314 corpus dots sat at
//! bends — because the router decided dots while it routed, before its own tap
//! splits and the label stubs existed.

use std::path::{Path, PathBuf};

use kicad::KicadInstallation;
use kicad_symbol::SymbolTable;
use sch_doc::SchDoc;
use sch_floorplan::floorplan;

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/validation")
}

#[test]
fn every_corpus_dot_sits_where_three_conductors_meet() {
    if !corpus().is_dir() {
        eprintln!("SKIP: validation corpus not present");
        return;
    }
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let provider = SymbolTable::from_symbol_dir(env.symbol_dir().to_path_buf());
    let mut names: Vec<String> = std::fs::read_dir(corpus())
        .unwrap()
        .flatten()
        .filter_map(|e| {
            e.file_name()
                .to_str()?
                .strip_suffix(".place-parts.json")
                .map(str::to_string)
        })
        .collect();
    names.sort();
    let mut problems: Vec<String> = Vec::new();
    for name in names {
        let src =
            std::fs::read_to_string(corpus().join(format!("{name}.place-parts.json"))).unwrap();
        let input: sch_check::PlacePartsInput = serde_json::from_str(&src).unwrap();
        let (design, diags, _) = sch_check::into_design(&input, &provider, &Default::default());
        if diags.has_errors() {
            continue;
        }
        let mut ir = floorplan::infer_ir(&env, &design);
        if let Some(intent) = input.intent.clone() {
            floorplan::apply_intent(&mut ir, intent.into_layout_ir());
        }
        let Ok(result) = floorplan::emit_strategy(&env, &design, Some(ir)) else {
            continue;
        };
        let doc = SchDoc::parse(&result.sch).unwrap();
        let meets = sch_doc::meets(&doc);
        let dots = sch_doc::drawn_dots(&doc);
        for (k, at) in &dots {
            let m = meets.get(k).copied().unwrap_or_default();
            if !m.needs_dot() {
                problems.push(format!(
                    "{name}: dot at ({:.2},{:.2}) joins nothing — ends={} passes={} pins={}",
                    at[0], at[1], m.ends, m.passes, m.pins
                ));
            }
        }
        // The half that changes the netlist a reader takes off the sheet.
        for (k, m) in &meets {
            if m.ends >= 1 && m.passes >= 1 && !dots.contains_key(k) {
                problems.push(format!(
                    "{name}: wire ends inside another wire at ({:.2},{:.2}) with no dot",
                    k.0 as f64 / 1000.0,
                    k.1 as f64 / 1000.0
                ));
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
