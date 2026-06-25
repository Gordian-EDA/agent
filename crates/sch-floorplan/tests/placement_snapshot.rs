//! Deterministic placement snapshots — the regression gate that replaces
//! "byte-identical to the historical hand-tuned references" once the placement
//! search produces re-baselined geometry (the emit-flow refactor). Renders the
//! aesthetic targets through the PRODUCTION path (sidecar IR if hand-tuned, else
//! `infer_ir`) and diffs the emitted `.kicad_sch` against a committed snapshot.
//!
//! Bless an intentional, visually-reviewed change with `UPDATE_SNAPSHOTS=1`.
//! Correctness of the hard challenge fixtures is covered by the geometry-invariant
//! truthfulness oracle (`floorplan_netlist.rs`); this gate guards the *aesthetic*
//! placement the oracle can't see.

use kicad_env::KicadEnv;
use kicad_symbol::SymbolTable;
use sch_floorplan::floorplan::{self, LayoutIr};
use std::path::{Path, PathBuf};

/// The aesthetic targets: the 4 tuned references (sidecar IR) + the grid demo
/// (authored `layout:` grid via `infer_ir`).
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
    Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../docs/validation/{name}.{ext}"))
}
fn snap_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/snapshots/{name}.kicad_sch"))
}

/// Render a fixture exactly as production would: sidecar IR if present, else the
/// connectivity-inferred frame.
fn render(env: &KicadEnv, provider: &SymbolTable, name: &str) -> String {
    let src = std::fs::read_to_string(doc(name, "circuit.yaml")).unwrap();
    let result = circuit_lang::compile(&src, provider);
    assert!(!result.diagnostics.has_errors(), "{name}: {:#?}", result.diagnostics);
    let design = result.design.unwrap();
    let ir = match std::fs::read_to_string(doc(name, "layout.json")) {
        Ok(s) => LayoutIr::from_json(&s).unwrap(),
        Err(_) => floorplan::infer_ir(env, &design),
    };
    floorplan::emit_strategy(env, &design, &ir, Box::new(greedy_place::Greedy)).unwrap_or_else(|e| panic!("{name}: {e}")).sch
}

#[test]
fn placement_snapshots_match() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("no KiCAD environment; skipping placement snapshots");
        return;
    };
    let provider = SymbolTable::from_env(&env);
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
    let Some(env) = KicadEnv::detect() else { return };
    let provider = SymbolTable::from_env(&env);
    // Emit twice with the same (default) strategy + fixed seed; must be identical.
    // Trivial for the deterministic greedy default today, but this is the guard
    // that catches accidental nondeterminism the moment randomized SA moves land.
    for name in ["555-blinker", "grid-demo"] {
        let a = render(&env, &provider, name);
        let b = render(&env, &provider, name);
        assert_eq!(a, b, "{name}: emit is nondeterministic");
    }
}
