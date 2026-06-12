//! Slice-1 acceptance gate: the full autorouting pipeline cross-checked by
//! KiCAD's own DRC and the in-house strict lint.
//!
//! Read `two_res.kicad_pcb` → `read_problem` → `pcb_engine::route` → assert no
//! failed nets → `write_solution` → `kicad-cli pcb drc` on the written board.
//! The routed board must have **zero copper/track DRC violations** and **zero
//! unconnected items**, and the in-house [`pcb_engine::lint`] run on the same
//! problem+solution must agree (clean). If the two oracles disagreed, that would
//! be a bug to chase at the source — never suppressed here.
//!
//! ## Why `lib_footprint_mismatch` is not a gate failure
//!
//! `two_res.kicad_pcb` is a hand-authored fixture whose inline footprint bodies
//! do not byte-match the installed `Resistor_SMD` library copy, so DRC emits a
//! `warning`-severity `lib_footprint_mismatch` per footprint. This is a footprint
//! *library bookkeeping* finding, present before and after routing and wholly
//! independent of copper geometry or connectivity — it is not something the
//! autorouter can fix and not what this gate measures. We therefore key the gate
//! on **error-severity** violations and the **unconnected-items** count (the two
//! things routing controls) and additionally assert that *no copper-class*
//! violation type ever appears. The carve-out is the same one the
//! `pcb_roundtrip` DRC smoke test already documents. We deliberately do not
//! regenerate the fixture's footprints from a specific KiCAD build to silence the
//! warning: that would pin the checked-in board to one KiCAD version, breaking the
//! slice-0 "loadable by KiCAD 9" convention.

use std::path::{Path, PathBuf};

use kicad_bridge::cli::{DrcReport, KicadCli, Violation};
use kicad_bridge::env::KicadEnv;
use kicad_bridge::pcb::{read_problem, write_solution};

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/two_res.kicad_pcb")
}

/// `kicad-cli pcb drc` (and the rest of the CLI surface) exists from KiCAD 8 on.
/// Gate the e2e on a detected install AND a major version ≥ 8, printing a
/// visible skip otherwise — mirroring `tests/env.rs` and the other CLI tests.
fn detect_gated() -> Option<KicadEnv> {
    let env = KicadEnv::detect()?;
    let major: u32 = env
        .cli_version
        .split('.')
        .next()
        .and_then(|m| m.parse().ok())
        .unwrap_or(0);
    if major < 8 {
        eprintln!("SKIP: KiCAD {} is below the supported major 8", env.cli_version);
        return None;
    }
    Some(env)
}

/// Footprint-library bookkeeping warnings unrelated to copper/routing. Anything
/// NOT in this set is treated as a copper-class violation the gate forbids.
const NON_COPPER_WARNINGS: &[&str] = &["lib_footprint_mismatch", "lib_footprint_issues"];

fn is_non_copper(v: &Violation) -> bool {
    v.severity == "warning" && NON_COPPER_WARNINGS.contains(&v.kind.as_str())
}

/// The slice-1 acceptance gate end-to-end.
#[test]
fn routed_board_passes_kicad_drc_and_inhouse_lint() {
    let Some(env) = detect_gated() else { return };

    // Work on a copy so the checked-in fixture is never mutated.
    let tmp = tempfile::Builder::new()
        .prefix("autopcb-e2e-")
        .suffix(".kicad_pcb")
        .tempfile()
        .expect("tempfile");
    std::fs::copy(fixture(), tmp.path()).expect("copy fixture");
    let path: &Path = tmp.path();

    // 1. Read the board into a routing problem.
    let board = read_problem(path).expect("read_problem");

    // 2. Route it. The whole board must route — a failed net fails the gate.
    let result = pcb_engine::router::route(&board.problem);
    assert!(
        result.failed.is_empty(),
        "router left nets unrouted: {:?}",
        result.failed
    );

    // 3. In-house strict lint on the SAME problem+solution must be clean. This is
    //    one of the two cross-checking oracles; a non-empty result fails the gate
    //    (and would mean the router emitted geometry its own oracle rejects).
    let lints = pcb_engine::lint::lint(&board.problem, &result.solution);
    assert!(
        lints.is_empty(),
        "in-house lint flagged the routed solution: {lints:?}"
    );

    // 4. Write the copper back onto the board, then run KiCAD's DRC on it.
    write_solution(path, &result.solution, &board).expect("write_solution");
    let cli = KicadCli::new(&env);
    let report: DrcReport = cli.drc(path).expect("kicad-cli pcb drc");

    // 5. The acceptance gate: zero unconnected items (routing joined every net)
    //    and zero copper-class DRC violations. The only tolerated entries are the
    //    footprint-library warnings documented above.
    assert_eq!(
        report.unconnected_items.len(),
        0,
        "routed board still has unconnected items: {:?}",
        report.unconnected_items
    );
    let copper: Vec<&Violation> = report
        .violations
        .iter()
        .filter(|v| !is_non_copper(v))
        .collect();
    assert!(
        copper.is_empty(),
        "kicad DRC reported copper-class violations on the routed board: {copper:?}"
    );
    assert_eq!(
        report.error_count(),
        0,
        "routed board has error-severity DRC findings: {report:?}"
    );

    // 6. Cross-oracle agreement: both the in-house lint and KiCAD's copper DRC
    //    found the routed copper clean. (A disagreement here — e.g. lint clean but
    //    kicad flags a short, or vice versa — is a router/lint/writer bug to fix
    //    at the source, not to mask.)
    eprintln!(
        "e2e OK: in-house lint clean; kicad DRC copper-clean, 0 unconnected, \
         {} tolerated footprint-library warning(s)",
        report.violations.len()
    );
}
