//! Slice-4 acceptance gate: placement feeds routing, and good placement BEATS a
//! deliberately bad fixed one — the spec's gate ("route success goes UP vs a
//! fixed bad placement"), realised on `place-charger.json`.
//!
//! ## The fixture
//!
//! `place-charger.json` is a small (40×28 mm, 2-layer) charger-ish board: a
//! 2-pin power-input header (J1), two SOT-23 ICs (U1 regulator, U2 status
//! driver), four 0603 decouplers (C1–C4), a feedback divider (R1/R2) and a
//! status-LED pair (R3/R4) — 11 parts, REAL footprint dimensions cribbed from
//! kicad-bridge's vendored R_0603 / SOT-23 / PinHeader_1x02 fixtures
//! (pad-enclosing courtyards, per the Task-1 invariant). Nets: VIN, GND, VOUT,
//! FB, STAT, LED. All parts are UNLOCKED — the engine places them.
//!
//! `place-charger-fixed.json` is the SAME 11 parts, all LOCKED at a deliberately
//! terrible layout: every part that shares a net is flung to the opposite side
//! of the board, so each net's span is near the full board diagonal (decouplers
//! scattered far from their ICs, the FB/LED dividers split corner-to-corner).
//!
//! ## The defeat form: HPWL / wirelength regression (not failed nets)
//!
//! At this small scale on a 2-layer board the pad field has enough room that even
//! the corner-flung fixed placement still ROUTES (0 failed nets) — a structural
//! failed-net defeat like congested.json's saturated wall does not materialise
//! (a wall of 0603s emits only their tiny top-layer PADS as obstacles, not solid
//! keepouts, so the router slips around/under them; ~8 layout iterations
//! confirmed no honest failed-net defeat at 11 parts / 2 layers). So we assert
//! the plan's documented FALLBACK: the bad placement's HPWL and routed
//! wirelength are ≥ 2× the engine's. This is the spec gate in its honest metric
//! form — placement quality drives routing cost, and good placement wins.
//!
//! Actuals (2026-06-12, this engine):
//!   variant            HPWL     routed wirelength   routed failed
//!   engine (no hints)  65.56    97.00               0
//!   fixed (bad lock)   235.71   327.00              0   (HPWL 3.6× / wl 3.4×)
//!   hinted             54.41    81.60               0   (HPWL ≤ no-hints)
//!
//! ## Tighten-don't-delete
//!
//! If the engine ever places `place-charger` so loosely that fixed is NOT ≥ 2×
//! tighter — or if a future router makes the fixed board's wirelength competitive
//! — that is a real regression in placement quality or a fixture that stopped
//! biting: INVESTIGATE and TIGHTEN the fixed layout (fling parts further / shrink
//! the board), never weaken these assertions. Likewise if hints stop helping
//! (hinted HPWL creeping above no-hints), that is a real hint-engine regression.

use pcb_engine::lint::lint;
use pcb_engine::pipeline::{metrics, route_auto};
use pcb_engine::placement::{
    place, to_route_problem, Edge, GroupHint, PlaceProblem, PlaceResult, Placement, PlacementHints,
};
use std::path::Path;

fn load(name: &str) -> PlaceProblem {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name);
    let json = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&json).unwrap_or_else(|e| panic!("parse {name}: {e}"))
}

/// Routed wirelength of a placement (via `to_route_problem` → `route_auto`),
/// together with the failed-net count, computed ONCE per placement (routing can
/// be slow — every variant routes exactly once in this file).
fn route_wirelength(problem: &PlaceProblem, res: &PlaceResult) -> (f64, usize) {
    let rp = to_route_problem(problem, &res.placements);
    let routed = route_auto(&rp);
    (metrics(&routed.solution).wirelength, routed.failed.len())
}

/// The fixed fixture's placements are read straight off its locked positions (the
/// engine returns them unchanged, since every part is locked).
fn locked_placements(problem: &PlaceProblem) -> Vec<Placement> {
    problem
        .parts
        .iter()
        .map(|p| {
            let l = p
                .locked
                .as_ref()
                .expect("place-charger-fixed parts must all be locked");
            Placement {
                reference: p.reference.clone(),
                at: l.at.clone(),
                rotation: l.rotation,
            }
        })
        .collect()
}

// ── (i) engine placement (empty hints) → legal, routes clean, lint empty ────────

#[test]
fn engine_placement_is_legal_routes_clean_and_lints_empty() {
    let problem = load("place-charger.json");
    let res = place(&problem, &PlacementHints::default());
    assert!(
        res.legal,
        "engine placement of place-charger must be legal: {res:?}"
    );

    let rp = to_route_problem(&problem, &res.placements);
    let routed = route_auto(&rp);
    assert!(
        routed.failed.is_empty(),
        "engine-placed place-charger must route with 0 failed nets; got {:?}",
        routed.failed
    );
    let lints = lint(&rp, &routed.solution);
    assert!(
        lints.is_empty(),
        "engine-placed + routed place-charger must lint EMPTY; got {lints:?}"
    );
}

// ── (ii) fixed placement → the defeat: ≥ 2× HPWL AND ≥ 2× routed wirelength ──────

#[test]
fn fixed_bad_placement_is_at_least_2x_worse() {
    let engine_problem = load("place-charger.json");
    let engine = place(&engine_problem, &PlacementHints::default());
    assert!(engine.legal, "engine baseline must be legal: {engine:?}");
    let (engine_wl, engine_failed) = route_wirelength(&engine_problem, &engine);
    assert_eq!(engine_failed, 0, "engine baseline routes clean");

    let fixed_problem = load("place-charger-fixed.json");
    // The fixed board is the SAME parts (verified) but all locked.
    assert_eq!(
        fixed_problem.parts.len(),
        engine_problem.parts.len(),
        "fixed fixture must carry the SAME parts as the engine fixture"
    );
    let fixed = PlaceResult {
        placements: locked_placements(&fixed_problem),
        legal: true,
        // The report is not used here; route off the locked placements directly.
        report: engine.report.clone(),
    };
    let (fixed_wl, _fixed_failed) = route_wirelength(&fixed_problem, &fixed);

    // Placement-geometry defeat: the fixed layout's HPWL is ≥ 2× the engine's.
    // (HPWL is computed by `place` for the all-locked problem — it just reports
    // the locked positions' net-bbox half-perimeters.)
    let fixed_hpwl = place(&fixed_problem, &PlacementHints::default()).report.hpwl;
    let engine_hpwl = engine.report.hpwl;
    assert!(
        fixed_hpwl >= 2.0 * engine_hpwl,
        "fixed HPWL {fixed_hpwl:.2} must be >= 2x the engine's {engine_hpwl:.2} \
         (actuals 235.71 vs 65.56 = 3.6x). If this no longer holds, the fixed \
         fixture stopped biting — TIGHTEN it (fling parts further), do not weaken \
         this assertion."
    );

    // Routing-cost defeat: the bad placement costs ≥ 2× the wirelength to route.
    assert!(
        fixed_wl >= 2.0 * engine_wl,
        "fixed routed wirelength {fixed_wl:.2} must be >= 2x the engine's \
         {engine_wl:.2} (actuals 327.00 vs 97.00 = 3.4x). A drop here means the \
         placement→routing uplift shrank — INVESTIGATE the placement engine or \
         TIGHTEN the fixed fixture, never re-baseline silently."
    );
}

// ── (iii) hinted placement → legal, routes clean, HPWL ≤ unhinted ───────────────

#[test]
fn hints_help_and_never_hurt() {
    let problem = load("place-charger.json");

    let unhinted = place(&problem, &PlacementHints::default());
    assert!(unhinted.legal, "unhinted baseline legal: {unhinted:?}");

    // Author hints in the test (data only): group each IC with its decouplers so
    // they cohere, and pull the power-input connector to the west edge.
    let hints = PlacementHints {
        groups: vec![
            GroupHint {
                name: "u1-decouplers".to_owned(),
                members: vec!["U1".to_owned(), "C1".to_owned(), "C2".to_owned()],
                region: None,
                edge: None,
                grid: false,
                surround: None,
            },
            GroupHint {
                name: "u2-decouplers".to_owned(),
                members: vec!["U2".to_owned(), "C3".to_owned(), "C4".to_owned()],
                region: None,
                edge: None,
                grid: false,
                surround: None,
            },
            GroupHint {
                name: "connector".to_owned(),
                members: vec!["J1".to_owned()],
                region: None,
                edge: Some(Edge::W),
                grid: false,
                surround: None,
            },
        ],
        ..Default::default()
    };
    let hinted = place(&problem, &hints);
    assert!(hinted.legal, "hinted placement legal: {hinted:?}");

    // Hinted placement still routes clean and lints empty.
    let rp = to_route_problem(&problem, &hinted.placements);
    let routed = route_auto(&rp);
    assert!(
        routed.failed.is_empty(),
        "hinted placement must route with 0 failed nets; got {:?}",
        routed.failed
    );
    assert!(
        lint(&rp, &routed.solution).is_empty(),
        "hinted placement must lint EMPTY"
    );

    // The connector hugged the west edge (J1 courtyard 2.0 wide ⇒ half-width 1.0;
    // EDGE_BAND is 2.0 mm, so J1.x sits well within the left band).
    let j1 = hinted
        .placements
        .iter()
        .find(|p| p.reference == "J1")
        .expect("J1 placed");
    assert!(
        j1.at.x - 1.0 <= problem.bounds.min_x + 2.0 + 0.5,
        "the edge-hinted connector J1 (left edge {:.2}) must sit in the west band",
        j1.at.x - 1.0
    );

    // Hints must not HURT: hinted HPWL ≤ unhinted HPWL (+ a small epsilon).
    // Actuals: 54.41 (hinted) ≤ 65.56 (unhinted).
    let eps = 1e-6;
    assert!(
        hinted.report.hpwl <= unhinted.report.hpwl + eps,
        "hinted HPWL {:.2} must be <= unhinted HPWL {:.2} (+eps) — hints may only \
         help, never hurt; a rise is a real hint-engine regression. (actuals \
         54.41 <= 65.56)",
        hinted.report.hpwl,
        unhinted.report.hpwl
    );
}
