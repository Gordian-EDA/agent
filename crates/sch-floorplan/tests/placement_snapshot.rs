//! Deterministic placement snapshots — the regression gate that replaces
//! "byte-identical to the historical hand-tuned references" once the placement
//! search produces re-baselined geometry (the emit-flow refactor). Renders the
//! aesthetic targets through the PRODUCTION path (sidecar IR if hand-tuned, else
//! connectivity-inferred IR inside [`SchematicPlaceProblem`]).
//! Correctness of the hard challenge fixtures is covered by the geometry-invariant
//! truthfulness oracle (`floorplan_netlist.rs`); this gate guards the *aesthetic*
//! placement the oracle can't see.

use kicad::KicadInstallation;
use kicad_symbol::SymbolTable;
use sch_floorplan::floorplan;
use std::path::{Path, PathBuf};

/// The aesthetic targets: the 4 tuned references (sidecar IR) + the grid demo
const TARGETS: &[&str] = &[
    "divider-filter",
    "mcp1703-power-entry",
    "555-blinker",
    "uart-level-translator",
    "grid-demo",
    // Regression guard for single-pin ports whose pin direction DISAGREES with the
    // name heuristic (MOSFET gates HA/LA/HB/LB face left but aren't input-named):
    // the port pennant must follow the pin, not land on the transistor body.
    "hbridge-nmos",
];

fn doc(name: &str, ext: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/validation/{name}.{ext}"))
}
fn snap_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/snapshots/{name}.kicad_sch"))
}

fn validation_corpus_available() -> bool {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/validation")
        .is_dir()
}

/// Render a fixture exactly as production would: sidecar IR if present, else the
/// connectivity-inferred frame.
fn render(env: &KicadInstallation, provider: &SymbolTable, name: &str) -> String {
    let src = std::fs::read_to_string(doc(name, "place-parts.json")).unwrap();
    let input: sch_check::PlacePartsInput = serde_json::from_str(&src).unwrap();
    let (design, diagnostics) = sch_check::into_design(&input, provider);
    assert!(!diagnostics.has_errors(), "{name}: {:#?}", diagnostics);
    let ir = input
        .intent
        .map(sch_check::Intent::into_layout_ir)
        .unwrap_or_else(|| floorplan::infer_ir(env, &design));
    floorplan::emit_strategy(env, &design, Box::new(anneal_place::Anneal), Some(ir))
        .unwrap_or_else(|e| panic!("{name}: {e}"))
        .sch
}

#[test]
fn placement_snapshots_match() {
    if !validation_corpus_available() {
        eprintln!("docs/validation corpus not present; skipping placement snapshots");
        return;
    }
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("no KiCAD environment; skipping placement snapshots");
        return;
    };
    let provider = SymbolTable::from_symbol_dir(env.symbol_dir().to_path_buf());
    let bless = std::env::var("UPDATE_SNAPSHOTS").is_ok();
    let mut problems = Vec::new();
    for name in TARGETS {
        let got = render(&env, &provider, name);
        let path = snap_path(name);
        if bless {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &got).unwrap();
            eprintln!("blessed {name}");
            continue;
        }
        match std::fs::read_to_string(&path) {
            Ok(want) if want == got => {}
            Ok(_) => problems.push(format!(
                "{name}: placement changed — re-render + unbiased-subagent visual \
                 review, then UPDATE_SNAPSHOTS=1 to re-baseline"
            )),
            Err(_) => problems.push(format!(
                "{name}: no snapshot at {} (run with UPDATE_SNAPSHOTS=1 to create)",
                path.display()
            )),
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn emit_is_deterministic() {
    if !validation_corpus_available() {
        eprintln!("docs/validation corpus not present; skipping deterministic emit test");
        return;
    }
    let Some(env) = KicadInstallation::detect() else {
        return;
    };
    let provider = SymbolTable::from_symbol_dir(env.symbol_dir().to_path_buf());
    // Emit twice with the same (default) strategy + fixed seed; must be identical.
    // Trivial for the deterministic greedy default today, but this is the guard
    // that catches accidental nondeterminism the moment randomized SA moves land.
    for name in ["555-blinker", "grid-demo"] {
        let a = render(&env, &provider, name);
        let b = render(&env, &provider, name);
        assert_eq!(a, b, "{name}: emit is nondeterministic");
    }
}
