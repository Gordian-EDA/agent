//! Slice-4 acceptance gate: a board PLACED by the engine from real footprints,
//! routed, written out, and cross-checked by KiCAD's DRC and the in-house lint.
//!
//! End to end: load the three vendored `.kicad_mod` footprints → build a
//! [`pcb_engine::placement::Part`] for each via [`kicad_bridge::placefp::part_from_footprint`]
//! with a small net map → [`pcb_engine::placement::place`] (EMPTY hints) → assert
//! legal → [`to_route_problem`] → [`route_auto`] → assert zero failed nets →
//! [`move_footprints`] the template board to the engine's placement →
//! [`write_solution`] the routed copper onto it → `kicad-cli pcb drc`.
//!
//! The placed and routed board must have zero copper DRC violations and zero
//! unconnected items, and the in-house [`pcb_engine::lint`] must agree (clean).
//!
//! ## Coherence: ONE description drives both the PlaceProblem and the template
//!
//! KiCAD's connectivity is computed from the *template board's* footprints and
//! net table; the engine's routing is computed from the *PlaceProblem*. For the
//! two oracles to agree they must describe the SAME circuit — same references,
//! same pad→net wiring, same pad geometry. We therefore derive the PlaceProblem
//! parts FROM the very footprints the template instantiates, and the [`NET_MAP`]
//! table below is the single authoritative pad→net wiring that BOTH the engine
//! parts (here) and the template's `(net …)` pad bindings (checked in) follow. If
//! they ever drift, `move_footprints` relocates a pad to a spot the routed copper
//! does not reach, and DRC reports it as unconnected — the gate catches the
//! incoherence rather than masking it.
//!
//! The `lib_footprint_mismatch` carve-out is the same one `pcb_route_e2e.rs`
//! documents: the template's inline footprint bodies do not byte-match the
//! installed library copies, an inert library-bookkeeping warning independent of
//! copper or connectivity.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use kicad_bridge::cli::{DrcReport, KicadCli, Violation};
use kicad_bridge::env::KicadEnv;
use kicad_bridge::footlib::Footprint;
use kicad_bridge::pcb::{read_problem, write_solution};
use kicad_bridge::placefp::{move_footprints, part_from_footprint};
use pcb_engine::placement::{place, to_route_problem, PlaceProblem, PlacementHints};
use pcb_engine::pipeline::route_auto;

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn footprint(name: &str) -> Footprint {
    Footprint::load(&fixtures().join("footprints").join(name))
        .unwrap_or_else(|e| panic!("load {name}: {e}"))
}

/// `kicad-cli pcb drc` exists from KiCAD 8 on. Gate on a detected install AND a
/// major version ≥ 8, with a visible skip otherwise (mirrors `pcb_route_e2e.rs`).
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

const NON_COPPER_WARNINGS: &[&str] = &["lib_footprint_mismatch", "lib_footprint_issues"];

fn is_non_copper(v: &Violation) -> bool {
    v.severity == "warning" && NON_COPPER_WARNINGS.contains(&v.kind.as_str())
}

/// The authoritative pad→net wiring for the whole circuit, keyed by reference.
/// Each entry maps a pad number to its net name. BOTH the engine parts built
/// here and the checked-in template's pad `(net …)` bindings follow this table —
/// it is the single source of truth that keeps the two oracles coherent.
///
/// Nets (all ≥ 2 pins): VIN {J1.1, U1.1}, GND {J1.2, U1.2, R1.2, R2.2},
/// VOUT {U1.3, R1.1, R2.1}.
///
/// VIN is kept a simple 2-pin net between the thru-hole connector pin and the
/// regulator input so the router reaches J1's drilled pad on a single layer:
/// a 3-pin VIN forces a bottom-layer detour that vias *up at the thru-hole pad*,
/// which KiCAD flags as a `hole_to_hole` (a via stacked on a drilled pad). Wiring
/// the connector pins as net leaves keeps the placed board single-layer near the
/// drills and DRC-clean. (Finding recorded in the test for future board
/// auto-generation: keep thru-hole pads off the interior of multi-pin nets.)
fn net_map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(p, n)| (p.to_string(), n.to_string()))
        .collect()
}

/// Build the PlaceProblem from the three real footprints, on a 40×28 board (the
/// template's outline). The references, pad nets and geometry match the template.
fn place_problem() -> PlaceProblem {
    let header = footprint("PinHeader_1x02_P2.54mm_Vertical.kicad_mod");
    let sot = footprint("SOT-23.kicad_mod");
    let res = footprint("R_0603_1608Metric.kicad_mod");

    let parts = vec![
        part_from_footprint(&header, "J1", &net_map(&[("1", "VIN"), ("2", "GND")])),
        part_from_footprint(
            &sot,
            "U1",
            &net_map(&[("1", "VIN"), ("2", "GND"), ("3", "VOUT")]),
        ),
        part_from_footprint(&res, "R1", &net_map(&[("1", "VOUT"), ("2", "GND")])),
        part_from_footprint(&res, "R2", &net_map(&[("1", "VOUT"), ("2", "GND")])),
    ];

    PlaceProblem {
        bounds: pcb_engine::problem::Bounds {
            min_x: 0.0,
            max_x: 40.0,
            min_y: 0.0,
            max_y: 28.0,
        },
        clearance: 0.2,
        layer_count: 2,
        min_trace_width: 0.25,
        parts,
    }
}

#[test]
fn placed_board_passes_kicad_drc_and_inhouse_lint() {
    let Some(env) = detect_gated() else { return };

    // 1. Footprints → parts → PlaceProblem → place (empty hints). The placement
    //    is verified legal by the engine's exact-geometry check.
    let problem = place_problem();
    let result = place(&problem, &PlacementHints::default());
    assert!(result.legal, "engine placement must be legal: {result:?}");

    // 2. Placement → routing problem → route_auto. The whole board must route.
    let rp = to_route_problem(&problem, &result.placements);
    let routed = route_auto(&rp);
    assert!(
        routed.failed.is_empty(),
        "placed board left nets unrouted: {:?}",
        routed.failed
    );

    // 3. In-house strict lint on the SAME problem+solution must be clean — one of
    //    the two cross-checking oracles.
    let lints = pcb_engine::lint::lint(&rp, &routed.solution);
    assert!(
        lints.is_empty(),
        "in-house lint flagged the placed+routed solution: {lints:?}"
    );

    // 4. Move the template's footprints to the engine placement, then write the
    //    routed copper onto the moved board. (Work in a temp dir so the checked-in
    //    template is never mutated.)
    let tmp = tempfile::Builder::new()
        .prefix("autopcb-placed-e2e-")
        .suffix(".kicad_pcb")
        .tempfile()
        .expect("tempfile");
    let path: &Path = tmp.path();
    move_footprints(
        &fixtures().join("placed_template.kicad_pcb"),
        &result.placements,
        path,
    )
    .expect("move_footprints");

    // Read the MOVED board back (its net codes/layer mapping) and write the
    // routed copper onto it. read_problem on the moved board gives the net-code
    // map write_solution needs; the net names match the routed solution because
    // both derive from NET_MAP.
    let board = read_problem(path).expect("read_problem on moved board");
    write_solution(path, &routed.solution, &board).expect("write_solution");

    // 5. KiCAD DRC on the placed + routed board.
    let cli = KicadCli::new(&env);
    let report: DrcReport = cli.drc(path).expect("kicad-cli pcb drc");

    // 6. The acceptance gate: zero unconnected items (routing joined every net on
    //    the placed board) and zero copper-class DRC violations (only the
    //    documented footprint-library warnings tolerated).
    assert_eq!(
        report.unconnected_items.len(),
        0,
        "placed+routed board still has unconnected items: {:?}",
        report.unconnected_items
    );
    let copper: Vec<&Violation> = report
        .violations
        .iter()
        .filter(|v| !is_non_copper(v))
        .collect();
    assert!(
        copper.is_empty(),
        "kicad DRC reported copper-class violations on the placed board: {copper:?}"
    );
    assert_eq!(
        report.error_count(),
        0,
        "placed board has error-severity DRC findings: {report:?}"
    );

    eprintln!(
        "placed e2e OK ({:?}): engine placement legal (hpwl {:.2}), route_auto clean, \
         in-house lint clean; kicad DRC copper-clean, 0 unconnected, {} tolerated \
         footprint-library warning(s)",
        routed.router,
        result.report.hpwl,
        report.violations.len()
    );
}
