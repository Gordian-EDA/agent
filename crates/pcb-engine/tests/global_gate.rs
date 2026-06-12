//! Acceptance gate for slice 2's global router: `fixtures/congested.json`.
//!
//! This fixture exists to document, forever, *why* the global router (slice 2)
//! is needed: it is a board the slice-1 greedy router ([`pcb_engine::router::route`])
//! provably cannot solve, yet the congestion-negotiating global router
//! ([`pcb_engine::pathing::global_route`]) plans feasibly — and only by actually
//! engaging its rip-up & reroute machinery (more than one negotiation
//! iteration), not by getting lucky on the first pass.
//!
//! ## The fixture geometry (debuggable by eye)
//!
//! A 60×60 mm, 2-layer board. A 3 mm-thick vertical wall at x = 30 splits it
//! left/right. The **bottom** layer's wall is solid (no gap) — so there is no
//! cheap via-relief; nets must cross on the **top** layer. The top wall has two
//! openings: a CENTRAL gap (≈1.8 mm, at y = 30) that is every net's natural
//! crossing, and a far CORNER relief gap (≈2.6 mm, near y = 4). Eight nets run
//! left→right with vertically *reversed* endpoints (N0 enters low-left / exits
//! high-right, …), so their straight routes all funnel through — and cross
//! within — the central gap.
//!
//! - Slice 1 routes greedily, shortest-half-perimeter first, marking clearance
//!   halos. Early nets wall off the single-layer central crossing and later nets
//!   have nowhere to go: ≥ 1 net fails.
//! - The global router models the wall's per-gap capacity, overflows the central
//!   gap on its first pass, then negotiates (history cost) some nets out to the
//!   far corner relief gap over several rip-up iterations, reaching a
//!   zero-overflow plan.
//!
//! ## If this test starts FAILING because slice 1 now solves the fixture
//!
//! Do **not** delete or weaken the assertion. Slice 1 solving this board means
//! the fixture no longer defeats it — TIGHTEN the fixture (narrow the central
//! gap, add nets, shrink the relief) until slice 1 fails again. The whole point
//! is that this board defeats the greedy router by construction.

use pcb_engine::pathing::global_route;
use pcb_engine::problem::RouteProblem;
use pcb_engine::router;
use std::path::Path;

fn load_congested() -> RouteProblem {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join("congested.json");
    let json = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&json).unwrap_or_else(|e| panic!("parse congested.json: {e}"))
}

/// (a) The fixture provably defeats the slice-1 greedy router.
///
/// If this ever stops holding, slice 1 has improved past the fixture — tighten
/// the fixture (see module docs), never the assertion.
#[test]
fn slice1_greedy_is_defeated() {
    let p = load_congested();
    let result = router::route(&p);
    assert!(
        !result.failed.is_empty(),
        "congested.json must DEFEAT the slice-1 greedy router (>= 1 failed net); \
         got 0 failures. If slice 1 now solves this board, TIGHTEN the fixture \
         (narrower central gap / more nets), do not delete this assertion."
    );
}

/// (b) The global router plans the same board *feasibly* (no edge over capacity,
/// every net planned) AND (c) it does so only by engaging rip-up & reroute:
/// more than one negotiation iteration. A first-pass-feasible result (iterations
/// <= 1) would mean the fixture no longer exercises negotiation — tighten it.
#[test]
fn global_router_is_feasible_and_engages_ripup() {
    let p = load_congested();
    let g = global_route(&p);
    let r = &g.report;

    // (b) feasible: no overflow, nothing unrouted.
    assert!(
        g.is_feasible(),
        "congested.json must get a FEASIBLE global plan: \
         final_overflow={} unrouted={:?}",
        r.final_overflow,
        r.unrouted
    );
    assert_eq!(r.final_overflow, 0, "feasible ⇒ zero residual overflow");
    assert!(r.unrouted.is_empty(), "feasible ⇒ no unrouted nets");

    // (c) rip-up provably engaged: negotiation ran more than once. The first
    // pass MUST overflow (so overflow_history[0] > 0) and later clear, proving
    // the negotiated rip-up — not first-pass luck — is what makes it feasible.
    assert!(
        r.iterations > 1,
        "congested.json must ENGAGE rip-up (> 1 iteration), got {}; \
         overflow_history={:?}. A first-pass-feasible fixture does not exercise \
         negotiation — tighten the central gap, do not weaken this assertion.",
        r.iterations,
        r.overflow_history
    );
    assert!(
        r.overflow_history.first().copied().unwrap_or(0) > 0,
        "the first routing pass must overflow (overflow_history[0] > 0) for \
         rip-up to have anything to negotiate; got {:?}",
        r.overflow_history
    );

    // Congestion is reported, never silently absorbed: a board that overflowed
    // and negotiated must surface its hotspots.
    assert!(
        !r.edge_hotspots.is_empty(),
        "a congested board must report edge hotspots"
    );
}

/// Determinism: the engines are deterministic, so this fixture must produce a
/// byte-identical global result across runs.
#[test]
fn global_result_is_deterministic_on_congested() {
    let p = load_congested();
    let a = serde_json::to_string(&global_route(&p)).unwrap();
    let b = serde_json::to_string(&global_route(&p)).unwrap();
    assert_eq!(a, b, "two global routes of congested.json must serialize byte-equal");
}
