//! Junction dots sit exactly where conductors meet, and a straight run is one wire —
//! on every corpus sheet.
//!
//! KiCAD's rule ([`sch_doc::Meet`]): a dot belongs where three conductors meet and
//! nowhere else. A dot at a plain bend reads as a branch that is not on the sheet;
//! a missing one where a wire ends on another wire's interior reads as a crossing,
//! which is a different netlist. Both were endemic — 167 of 314 corpus dots sat at
//! bends — because the router decided dots while it routed, before its own tap
//! splits and the label stubs existed.
//!
//! The second gate is the same geometry read the other way: a seam splitting a straight
//! run that nothing meets is ink with nothing to say.

use std::path::{Path, PathBuf};

use kicad::KicadInstallation;
use kicad_symbol::SymbolTable;
use sch_doc::SchDoc;
use sch_floorplan::floorplan;

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/validation")
}

/// Every validation fixture, emitted the way the production path emits it.
fn corpus_docs() -> Vec<(String, SchDoc)> {
    if !corpus().is_dir() {
        eprintln!("SKIP: validation corpus not present");
        return Vec::new();
    }
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return Vec::new();
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
    let mut out = Vec::new();
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
        out.push((name, SchDoc::parse(&result.sch).unwrap()));
    }
    out
}

#[test]
fn every_corpus_dot_sits_where_three_conductors_meet() {
    let mut problems: Vec<String> = Vec::new();
    for (name, doc) in corpus_docs() {
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

/// A straight run drawn in two pieces is a seam a reader has to rule out as a branch.
///
/// The writer draws a run in pieces — a lead-out, a riser, a tap split — and closes the
/// seam again wherever nothing else lands on it. What is left is the seam something DOES
/// land on: a pin, or a dot. Anything else is redundant ink.
#[test]
fn no_corpus_seam_splits_a_run_nothing_meets() {
    let mut problems: Vec<String> = Vec::new();
    for (name, doc) in corpus_docs() {
        let meets = sch_doc::meets(&doc);
        let dots = sch_doc::drawn_dots(&doc);
        for (k, m) in &meets {
            if m.ends != 2 || m.passes > 0 || m.pins > 0 || dots.contains_key(k) {
                continue;
            }
            let at = geom::Point2::new(k.0 as f64 / 1000.0, k.1 as f64 / 1000.0);
            let mut arms = doc
                .wires()
                .flat_map(|w| w.points.windows(2).map(|p| (p[0], p[1])).collect::<Vec<_>>())
                .filter_map(|(a, b)| match () {
                    _ if a.near_eq(at, 1e-6) => Some(b),
                    _ if b.near_eq(at, 1e-6) => Some(a),
                    _ => None,
                });
            let (Some(p), Some(q)) = (arms.next(), arms.next()) else {
                continue;
            };
            let (u, v) = ((p.x - at.x, p.y - at.y), (q.x - at.x, q.y - at.y));
            if (u.0 * v.1 - u.1 * v.0).abs() < 1e-6 && u.0 * v.0 + u.1 * v.1 < 0.0 {
                problems.push(format!(
                    "{name}: straight run split at ({:.2},{:.2}) with nothing there",
                    at.x, at.y
                ));
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
