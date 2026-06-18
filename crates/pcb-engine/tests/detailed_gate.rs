//! Slice-3 acceptance gate: the detailed router delivers DRC-clean copper on a
//! *medium* board end-to-end, the zero-slack stress board stays an honest
//! documented failure, and pipeline-vs-naive metrics are reported.
//!
//! ## Why two congested fixtures
//!
//! `congested.json` is a **zero-slack adversarial** fixture (8 nets through
//! exactly 8 top-wall crossing slots) built to stress slice-2's rip-up. Routing
//! it to DRC-clean copper needs a rip-up *detailed* router (deferred machinery —
//! see the slice-3 plan's Task 3.5a/3.5b findings), so it is asserted here AS the
//! documented stress case: geometry-clean, with EXACTLY 3 honest finisher
//! failures. The spec's slice-3 gate is "DRC-clean copper on *medium boards*",
//! which `congested-relief.json` realises: the same defeat-greedy topology with
//! the top wall opened into three gaps (relief-low 2.6 mm @ y≈4, central 1.8 mm
//! @ y≈30, relief-high 2.6 mm @ y≈56) for a total top-layer crossing capacity of
//! **12** against 8 nets — **slack 4**. The 1.8 mm central gap still defeats the
//! greedy slice-1 router (it walls off the single-layer central crossing for
//! later nets), but the slack lets the detailed pipeline route all eight nets
//! clean. (N0's right pad sits at y=55, not 57, to keep the top-right pad fan-out
//! out of a finisher near-miss — the topology is otherwise congested.json's
//! reversed-endpoint funnel.)
//!
//! ## If the congested.json finisher-failure count changes
//!
//! EXACTLY 3 is asserted. A change in EITHER direction is a real engine change
//! (the detailed router got better or worse), not noise: investigate it, update
//! the count and this comment with the new finding, never silently re-baseline.

use pcb_engine::lint::lint;
use pcb_engine::pipeline::{metrics, route_auto, route_detailed, RouteResult, RouterKind};
use pcb_engine::problem::RouteProblem;
use pcb_engine::router;
use std::path::Path;

fn load(name: &str) -> RouteProblem {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name);
    let json = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&json).unwrap_or_else(|e| panic!("parse {name}: {e}"))
}

/// Count `lint` entries that are *geometry* violations (clearance / via / width),
/// excluding the `Connectivity` entries that a dropped failed net produces.
fn geometry_violations(p: &RouteProblem, r: &RouteResult) -> Vec<String> {
    lint(p, &r.solution)
        .into_iter()
        .map(|v| format!("{v:?}"))
        .filter(|s| !s.contains("Connectivity"))
        .collect()
}

// ── the medium-board gate: congested-relief clean end-to-end ────────────────────

/// The slice-3 gate proper. On `congested-relief.json` (medium board, slack 4):
/// the greedy slice-1 router STILL fails ≥ 1 net, yet the detailed pipeline
/// routes every net with an EMPTY lint, and `route_auto` returns the Detailed
/// result (no fallback). This is "DRC-clean copper on a medium board, end-to-end".
///
/// If slice 1 ever stops failing this board, the fixture no longer exercises the
/// gate — TIGHTEN it (narrow the central gap / add nets), never weaken this
/// assertion (the same tighten-don't-delete rule as `congested.json`).
#[test]
fn congested_relief_is_clean_end_to_end() {
    let p = load("congested-relief.json");

    // (i) The greedy slice-1 router is defeated — the board still earns its name.
    let naive = router::route(&p);
    assert!(
        !naive.failed.is_empty(),
        "congested-relief.json must still DEFEAT the slice-1 greedy router \
         (>= 1 failed net); got 0. Slice 1 solving it means the central gap is \
         too wide — TIGHTEN the fixture, do not weaken this assertion. \
         (current slice-1 failures: {:?})",
        naive.failed
    );

    // (ii) The detailed pipeline routes the whole board clean.
    let detailed = route_detailed(&p);
    assert_eq!(detailed.router, RouterKind::Detailed);
    assert!(
        detailed.failed.is_empty(),
        "congested-relief.json must route CLEAN through route_detailed \
         (0 failed nets); got {:?}",
        detailed.failed
    );
    let lints = lint(&p, &detailed.solution);
    assert!(
        lints.is_empty(),
        "congested-relief detailed solution must lint EMPTY (no clearance / via / \
         width / connectivity violation); got {lints:?}"
    );

    // route_auto returns the Detailed result with no fallback (detailed is clean).
    let auto = route_auto(&p);
    assert_eq!(
        auto.router,
        RouterKind::Detailed,
        "route_auto must return Detailed when route_detailed is clean"
    );
    assert!(auto.failed.is_empty(), "route_auto clean: {:?}", auto.failed);
}

// ── the zero-slack stress case: congested.json, exactly 3 finisher failures ─────

/// `congested.json` is the documented zero-slack stress board. The detailed
/// finisher routes most of the wall but cannot pack all eight nets through the
/// exactly-saturated top wall, leaving EXACTLY 3 honest finisher failures — and
/// the emitted copper is GEOMETRY-CLEAN (no clearance / via / width violation;
/// the only lint entries are `Connectivity` from the 3 dropped nets).
///
/// **Do not silently re-baseline the count of 3.** A change in either direction
/// is a real detailed-engine change: a drop toward 0 means the rip-up-detailed
/// work landed (celebrate, then update this to the new floor); a rise means a
/// regression (investigate). Either way, update the count *and* this comment with
/// the new finding — never just bump the number to make the test pass.
#[test]
fn congested_stress_is_geometry_clean_with_exactly_three_finisher_failures() {
    let p = load("congested.json");
    let detailed = route_detailed(&p);

    // Every residual must be an honest finisher failure (not a global/assign/cell
    // fault that would mean a different stage broke).
    assert!(
        detailed.failed.iter().all(|f| f.reason.contains("finisher")),
        "every congested residual must carry finisher provenance: {:?}",
        detailed.failed
    );
    assert_eq!(
        detailed.failed.len(),
        3,
        "congested.json (zero-slack stress) must leave EXACTLY 3 finisher \
         failures; got {}. A change in EITHER direction is a real engine change — \
         investigate and update the count AND the module comment, never silently \
         re-baseline. (failures: {:?})",
        detailed.failed.len(),
        detailed.failed
    );

    // The copper that IS emitted is geometry-clean: only the dropped nets'
    // connectivity gaps remain, no clearance/via/width defect.
    let geom = geometry_violations(&p, &detailed);
    assert!(
        geom.is_empty(),
        "congested detailed copper must be geometry-clean (only connectivity gaps \
         from the 3 dropped nets are allowed); got {geom:?}"
    );
}

// ── the already-clean medium fixtures, asserted here as the gate's record ───────

/// `quad.json` and `led-r.json` route clean through `route_detailed` (closed by
/// the Task 3.5 finisher). Asserted here so the gate file is the single record of
/// every fixture's detailed-pipeline outcome.
#[test]
fn quad_and_led_r_are_clean_through_route_detailed() {
    for name in ["led-r.json", "quad.json"] {
        let p = load(name);
        let d = route_detailed(&p);
        assert_eq!(d.router, RouterKind::Detailed);
        assert!(
            d.failed.is_empty(),
            "{name} must route clean through route_detailed; got {:?}",
            d.failed
        );
        let lints = lint(&p, &d.solution);
        assert!(
            lints.is_empty(),
            "{name} detailed solution must lint EMPTY; got {lints:?}"
        );
    }
}

// ── metrics comparison table: naive vs pipeline on every fixture ────────────────

/// Print a naive-vs-detailed metrics table (wirelength / vias / failed) for every
/// fixture and assert the honesty bound: where BOTH engines route a fixture fully
/// (0 failed each), the detailed pipeline's wirelength is within **3×** the naive
/// router's (it trades a little copper for clearance-honest 45° routing, not an
/// order of magnitude). Run with `--nocapture` to read the table.
///
/// Each fixture's expensive `route_detailed` is computed **once** here (congested
/// costs ~20 s); the other gate tests in this file each route their own fixture
/// once as well — the suite pays for `route_detailed(congested*)` a bounded number
/// of times, never in a loop.
#[test]
fn metrics_table_naive_vs_pipeline() {
    // Actuals (2026-06-12, this engine), for the honesty-bound comment:
    //   fixture            naive  (wl / via)     detailed (wl / via)   ratio
    //   led-r.json         42.30 / 0             43.25 / 0             1.02×
    //   quad.json          222.30 / 8            232.22 / 12           1.04×
    //   congested-relief   674.55 / 4 (1 fail)   758.44 / 12 (0 fail)  — (naive partial)
    //   congested.json     461.25 / 4 (3 fail)   464.11 / 10 (3 fail)  — (both partial)
    let fixtures = [
        "led-r.json",
        "quad.json",
        "congested-relief.json",
        "congested.json",
    ];

    println!(
        "\n{:<22} | {:>6} {:>9} {:>5} | {:>6} {:>9} {:>5} | {:>6}",
        "fixture", "n_fail", "n_wl", "n_via", "d_fail", "d_wl", "d_via", "wl×"
    );
    println!("{}", "-".repeat(78));

    for name in fixtures {
        let p = load(name);
        let naive = router::route(&p);
        let nm = metrics(&naive.solution);
        let detailed = route_detailed(&p); // once per fixture
        let dm = metrics(&detailed.solution);

        let both_full = naive.failed.is_empty() && detailed.failed.is_empty();
        let ratio = if both_full && nm.wirelength > 0.0 {
            dm.wirelength / nm.wirelength
        } else {
            f64::NAN
        };
        let ratio_str = if both_full {
            format!("{ratio:.2}x")
        } else {
            "  -  ".to_owned()
        };

        println!(
            "{:<22} | {:>6} {:>9.2} {:>5} | {:>6} {:>9.2} {:>5} | {:>6}",
            name,
            naive.failed.len(),
            nm.wirelength,
            nm.via_count,
            detailed.failed.len(),
            dm.wirelength,
            dm.via_count,
            ratio_str
        );

        // Honesty bound: where both engines fully route, detailed wirelength is
        // within 3× naive (a sanity bound, not a quality target).
        if both_full {
            assert!(
                dm.wirelength <= 3.0 * nm.wirelength,
                "{name}: detailed wirelength {:.2} exceeds 3x naive {:.2} — \
                 the pipeline is taking wildly longer routes, investigate",
                dm.wirelength,
                nm.wirelength
            );
            assert!(
                dm.wirelength > 0.0 && dm.trace_count > 0,
                "{name}: detailed produced no copper"
            );
        }
    }
    println!();
}
