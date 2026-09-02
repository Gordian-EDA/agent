//! Realiser-level clearance oracle: on a finished sheet no point may be shared by two
//! nets. This is the geometry behind the `place_parts` refusal "the placed result does
//! not match the requested connectivity (shorted A+B)" — checked here directly on the
//! realised writer, with no kicad netlist round-trip, so a regression is a failing unit
//! test rather than a refused tool call a whole engine-run later.
//!
//! Runs over every `place-parts` fixture in the validation corpus, under both placement
//! engines. SKIPs without KiCAD.

use std::path::Path;

use kicad::KicadInstallation;
use kicad_symbol::SymbolTable;
use sch_floorplan::floorplan;
use sch_model::engine::PlacementEngine;

fn corpus() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/validation")
}

/// Every `<name>.place-parts.json` in the corpus, in name order.
fn fixtures() -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(corpus())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            e.file_name()
                .to_str()?
                .strip_suffix(".place-parts.json")
                .map(str::to_string)
        })
        .collect();
    out.sort();
    out
}

fn engines() -> Vec<(&'static str, Box<dyn PlacementEngine>)> {
    vec![
        ("anneal", Box::new(anneal_place::Anneal)),
        ("spine", Box::new(spine_place::SpinePlace)),
    ]
}

/// Realise `name` under `engine` and report every point two nets share.
fn shorts_of(
    env: &KicadInstallation,
    provider: &SymbolTable,
    name: &str,
    engine: Box<dyn PlacementEngine>,
) -> Vec<String> {
    let src = std::fs::read_to_string(corpus().join(format!("{name}.place-parts.json"))).unwrap();
    let input: sch_check::PlacePartsInput = serde_json::from_str(&src).unwrap();
    let (design, diags, _) = sch_check::into_design(&input, provider, &Default::default());
    assert!(!diags.has_errors(), "{name}: {diags:#?}");
    let ir = input
        .intent
        .clone()
        .map(sch_check::Intent::into_layout_ir)
        .unwrap_or_else(|| floorplan::baseline_ir(&design));
    floorplan::emit_strategy(env, &design, engine, Some(ir))
        .unwrap_or_else(|e| panic!("{name}: {e}"))
        .net_shorts
}

#[test]
fn realised_corpus_sheets_never_share_a_point_between_two_nets() {
    if !corpus().is_dir() {
        eprintln!("SKIP: validation corpus not present");
        return;
    }
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let provider = SymbolTable::from_symbol_dir(env.symbol_dir().to_path_buf());
    // `CLEARANCE_ONLY=bms-10s,esp32-multifunction` restricts the run for fast iteration
    // on one fixture; empty runs the whole corpus.
    let only = std::env::var("CLEARANCE_ONLY").unwrap_or_default();
    let only: Vec<&str> = only.split(',').filter(|s| !s.is_empty()).collect();
    let mut offenders: Vec<String> = Vec::new();
    for name in fixtures() {
        if !only.is_empty() && !only.contains(&name.as_str()) {
            continue;
        }
        for (engine, engine_name) in engines().into_iter().map(|(n, e)| (e, n)) {
            let found = shorts_of(&env, &provider, &name, engine);
            if !found.is_empty() {
                offenders.push(format!("{name}/{engine_name}: {found:#?}"));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "realised sheets share points between nets:\n{}",
        offenders.join("\n")
    );
}
