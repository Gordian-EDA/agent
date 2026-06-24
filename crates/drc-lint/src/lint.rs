//! Strict DRC lint: the precision oracle for an emitted [`RouteSolution`].
//!
//! This module is now a thin shim over [`drc_core::DrcSuite`]: [`lint`] runs
//! the standard suite, and [`DrcViolation`] is [`drc_core::Finding`]. The
//! geometry/connectivity rules themselves live in the engine kernel
//! (`drc-core`); this crate keeps the historical entry points stable for the
//! router, board harness, and route-quality scorers, and adds the two
//! copper-dropping helpers built on top of the report.
//!
//! ## What the standard suite checks
//!
//! - [`DrcViolation::ClearanceTraceTrace`] — two trace segments of *different*
//!   connections on the *same* layer whose copper edges are closer than
//!   `clearance`.
//! - [`DrcViolation::ClearanceTraceObstacle`] — a trace segment too close to a
//!   foreign or unowned (keepout) obstacle on a shared layer.
//! - [`DrcViolation::ClearanceViaAny`] — a via too close to copper not its own
//!   (another connection's trace/via/obstacle, or unowned copper), or a drill
//!   edge too close to another hole / foreign track / foreign pad.
//! - [`DrcViolation::TraceWidthBelowMin`] — a trace narrower than
//!   `min_trace_width`.
//! - [`DrcViolation::OutOfBounds`] — copper leaving the board `bounds` (or a
//!   custom outline edge).
//! - [`DrcViolation::InvalidLayer`] — a layer name that does not resolve.
//! - [`DrcViolation::ViaDiameterBelowMin`] — a via below its type's minimum.
//! - [`DrcViolation::Connectivity`] — the connectivity oracle's defects, folded
//!   in last so `lint` is the single one-stop report.

use crate::problem::{RouteProblem, RouteSolution};
use drc_core::DrcSuite;

/// A design-rule violation. Historical alias for the engine kernel's
/// [`drc_core::Finding`]; the variants and serde shape are identical.
pub use drc_core::Finding as DrcViolation;

// ── public API ───────────────────────────────────────────────────────────────

/// Run the full strict DRC lint over `solution` against `problem`.
///
/// A thin shim over [`DrcSuite::standard`]: returns every violation in
/// deterministic order — geometry violations first (collection order — traces,
/// then vias, then bounds), then the connectivity oracle's violations folded in
/// last.
pub fn lint(problem: &RouteProblem, solution: &RouteSolution) -> Vec<DrcViolation> {
    DrcSuite::standard().run(problem, solution)
}

/// Make `solution` connectivity-honest: drop the copper of every net the
/// connectivity oracle reports as unconnected (a half-route a router miscounted
/// as done) or cross-net-shorted, and return those net names (sorted, unique).
///
/// The connectivity oracle — not a router's own bookkeeping — is the authority on
/// what is actually joined. After this call the surviving copper carries no
/// connectivity defect; callers should mark the returned names as failed nets so
/// the reported result is faithful (an honest unrouted net, never silent copper
/// that lies about connectivity). Dropping a net's copper only removes obstacles,
/// so it can never break another net or introduce a geometry violation.
pub fn drop_unconnected_copper(problem: &RouteProblem, solution: &mut RouteSolution) -> Vec<String> {
    use crate::connectivity::Violation as ConnViolation;
    let mut broken: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for v in lint(problem, solution) {
        if let DrcViolation::Connectivity { violation } = v {
            match violation {
                ConnViolation::Unconnected { connection, .. } => {
                    broken.insert(connection);
                }
                ConnViolation::CrossNetMerge { a, b } => {
                    broken.insert(a);
                    broken.insert(b);
                }
            }
        }
    }
    if broken.is_empty() {
        return Vec::new();
    }
    solution.traces.retain(|t| !broken.contains(&t.connection));
    solution.vias.retain(|v| !broken.contains(&v.connection));
    broken.into_iter().collect()
}

/// Make `solution` GEOMETRY-clean: while the lint reports any geometry violation
/// (clearance / trace-width / via-clearance / out-of-bounds / invalid-layer),
/// drop the copper of the net involved in the most violations and retry. Returns
/// the dropped net names. The engine must never EMIT copper that fails DRC — on a
/// board too dense to route a net cleanly, dropping it (and reporting it failed)
/// is correct; a silent clearance violation that looks routed is not. Bounded by
/// the net count so it always terminates. Connectivity is handled separately by
/// [`drop_unconnected_copper`]; callers typically run both.
pub fn drop_violating_copper(problem: &RouteProblem, solution: &mut RouteSolution) -> Vec<String> {
    let mut dropped: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    // One net can be dropped per pass; at most one pass per net plus a margin.
    let max_passes = problem.connections.len() + 1;
    for _ in 0..max_passes {
        let mut tally: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
        for v in lint(problem, solution) {
            for net in violation_nets(&v) {
                *tally.entry(net).or_default() += 1;
            }
        }
        if tally.is_empty() {
            break;
        }
        // Drop the worst offender (most violations); ties broken by name (BTreeMap
        // iteration order) for determinism.
        let worst = tally
            .iter()
            .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
            .map(|(n, _)| n.clone())
            .unwrap();
        solution.traces.retain(|t| t.connection != worst);
        solution.vias.retain(|v| v.connection != worst);
        dropped.insert(worst);
    }
    dropped.into_iter().collect()
}

/// The net name(s) a GEOMETRY violation implicates (empty for connectivity, which
/// this never returns since callers pre-filter). For a trace/trace clearance both
/// nets are implicated; dropping the one in more violations resolves the most.
fn violation_nets(v: &DrcViolation) -> Vec<String> {
    match v {
        DrcViolation::ClearanceTraceTrace { a, b, .. } => vec![a.clone(), b.clone()],
        DrcViolation::ClearanceTraceObstacle { connection, .. }
        | DrcViolation::ClearanceViaAny { connection, .. }
        | DrcViolation::TraceWidthBelowMin { connection, .. }
        | DrcViolation::OutOfBounds { connection, .. }
        | DrcViolation::ViaDiameterBelowMin { connection, .. }
        | DrcViolation::InvalidLayer { connection, .. } => vec![connection.clone()],
        DrcViolation::Connectivity { .. } => Vec::new(),
    }
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectivity::Violation;
    use crate::problem::{
        Bounds, Connection, LayerRef, Obstacle, Point2, RoutePoint, RouteProblem, RouteSolution, Trace, Via,
        ViaSpan,
    };

    fn bounds() -> Bounds {
        Bounds {
            min_x: 0.0,
            max_x: 100.0,
            min_y: 0.0,
            max_y: 100.0,
        }
    }

    fn problem(connections: Vec<Connection>, obstacles: Vec<Obstacle>) -> RouteProblem {
        RouteProblem {
            layer_count: 2,
            min_trace_width: 0.25,
            obstacles,
            connections,
            bounds: bounds(),
            clearance: 0.2,
            via_diameter: 0.6,
            via_drill: 0.3,
            net_widths: Default::default(),
            outline: None,
            escape_layers: Default::default(),
        }
    }

    fn conn(name: &str, pts: &[(f64, f64, &str)]) -> Connection {
        Connection {
            name: name.to_owned(),
            points_to_connect: pts
                .iter()
                .map(|&(x, y, l)| RoutePoint {
                    x,
                    y,
                    layer: LayerRef(l.to_owned()),
                })
                .collect(),
        }
    }

    fn pad(connected_to: &[&str], center: (f64, f64), w: f64, h: f64, layers: &[&str]) -> Obstacle {
        Obstacle {
            kind: "rect".to_owned(),
            layers: layers.iter().map(|l| LayerRef((*l).to_owned())).collect(),
            center: Point2 {
                x: center.0,
                y: center.1,
            },
            width: w,
            height: h,
            connected_to: connected_to.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    fn trace(connection: &str, layer: &str, width: f64, path: &[(f64, f64)]) -> Trace {
        Trace {
            connection: connection.to_owned(),
            layer: LayerRef(layer.to_owned()),
            width,
            path: path.iter().map(|&(x, y)| Point2 { x, y }).collect(),
        }
    }

    fn via(connection: &str, at: (f64, f64)) -> Via {
        Via {
            connection: connection.to_owned(),
            at: Point2 { x: at.0, y: at.1 },
            diameter: 0.6,
            drill: 0.3,
            span: ViaSpan::Through,
        }
    }

    #[test]
    fn via_diameter_below_min_flags_undersized_through_but_allows_micro() {
        // Board netclass via diameter = 0.6 (problem()); micro floor = 0.3.
        let p = problem(vec![], vec![]);
        let mk = |dia: f64, span: ViaSpan| RouteSolution {
            traces: vec![],
            vias: vec![Via {
                connection: "GND".to_owned(),
                at: Point2 { x: 50.0, y: 50.0 },
                diameter: dia,
                drill: 0.3,
                span,
            }],
        };
        let flagged = |s: &RouteSolution| {
            lint(&p, s)
                .iter()
                .any(|v| matches!(v, DrcViolation::ViaDiameterBelowMin { .. }))
        };
        let micro = ViaSpan::Partial { from: 0, to: 1, micro: true };
        // A 0.5 THROUGH via is below the 0.6 netclass min → flagged (the exact class that shipped
        // 56 via_diameter faults from an undersized HDI blind via before this check existed).
        assert!(flagged(&mk(0.5, ViaSpan::Through)), "undersized through via must flag");
        // A full-size through via is fine.
        assert!(!flagged(&mk(0.6, ViaSpan::Through)), "full through via must pass");
        // A 0.4 MICRO via clears the relaxed 0.3 micro floor → NOT flagged (the HDI escape size).
        assert!(!flagged(&mk(0.4, micro.clone())), "0.4 micro via must pass the micro floor");
        // A 0.2 MICRO via is below even the micro floor → flagged.
        assert!(flagged(&mk(0.2, micro)), "sub-floor micro via must flag");
    }

    #[test]
    fn drop_violating_copper_drops_undersized_via_net() {
        // The oracle must DROP a net whose via is undersized, not ship it (honest unrouted).
        let p = problem(vec![], vec![]);
        let mut sol = RouteSolution {
            traces: vec![],
            vias: vec![Via {
                connection: "VCC".to_owned(),
                at: Point2 { x: 50.0, y: 50.0 },
                diameter: 0.5, // < 0.6 netclass min, span Through
                drill: 0.3,
                span: ViaSpan::Through,
            }],
        };
        let dropped = drop_violating_copper(&p, &mut sol);
        assert_eq!(dropped, vec!["VCC".to_owned()]);
        assert!(sol.vias.is_empty(), "undersized via dropped, not shipped");
    }

    fn count<F: Fn(&DrcViolation) -> bool>(vs: &[DrcViolation], f: F) -> usize {
        vs.iter().filter(|v| f(v)).count()
    }

    // ── per-variant triggers ────────────────────────────────────────────────

    #[test]
    fn clearance_trace_trace_fires_for_close_parallel_traces() {
        // Two parallel traces on top, centrelines 0.30 mm apart, each 0.25 wide
        // → edge gap = 0.30 - 0.125 - 0.125 = 0.05 < 0.2 clearance. Different
        // nets, fully connected so no connectivity noise.
        let p = problem(
            vec![
                conn("A", &[(10.0, 10.0, "top"), (30.0, 10.0, "top")]),
                conn("B", &[(10.0, 10.3, "top"), (30.0, 10.3, "top")]),
            ],
            vec![],
        );
        let s = RouteSolution {
            traces: vec![
                trace("A", "top", 0.25, &[(10.0, 10.0), (30.0, 10.0)]),
                trace("B", "top", 0.25, &[(10.0, 10.3), (30.0, 10.3)]),
            ],
            vias: vec![],
        };
        let vs = lint(&p, &s);
        assert_eq!(
            count(&vs, |v| matches!(v, DrcViolation::ClearanceTraceTrace { .. })),
            1,
            "exactly one trace/trace clearance violation, got {vs:?}"
        );
    }

    #[test]
    fn clearance_trace_trace_clean_for_far_parallel_traces() {
        // Same as above but 1.0 mm apart → edge gap 0.75 ≥ 0.2: clean.
        let p = problem(
            vec![
                conn("A", &[(10.0, 10.0, "top"), (30.0, 10.0, "top")]),
                conn("B", &[(10.0, 11.0, "top"), (30.0, 11.0, "top")]),
            ],
            vec![],
        );
        let s = RouteSolution {
            traces: vec![
                trace("A", "top", 0.25, &[(10.0, 10.0), (30.0, 10.0)]),
                trace("B", "top", 0.25, &[(10.0, 11.0), (30.0, 11.0)]),
            ],
            vias: vec![],
        };
        assert!(
            !lint(&p, &s)
                .iter()
                .any(|v| matches!(v, DrcViolation::ClearanceTraceTrace { .. })),
            "far parallel traces must not raise a trace/trace clearance"
        );
    }

    #[test]
    fn clearance_trace_obstacle_fires_for_foreign_pad() {
        // GND pad at (20,10), 1.0×1.0 → right edge x=20.5. A SIG trace runs at
        // y=10 to x=20.6, edge gap = (20.6-0.125) - 20.5 ... measure: segment
        // reaches x=20.6, rect right edge 20.5, dist 0.1, minus half-width
        // 0.125 → -0.025 (overlap) < 0.2. Foreign owner → violation.
        let p = problem(
            vec![
                conn("SIG", &[(5.0, 10.0, "top"), (20.6, 10.0, "top")]),
                conn("GND", &[(20.0, 10.0, "top")]),
            ],
            vec![pad(&["GND"], (20.0, 10.0), 1.0, 1.0, &["top"])],
        );
        let s = RouteSolution {
            traces: vec![trace("SIG", "top", 0.25, &[(5.0, 10.0), (20.6, 10.0)])],
            vias: vec![],
        };
        let vs = lint(&p, &s);
        assert_eq!(
            count(&vs, |v| matches!(
                v,
                DrcViolation::ClearanceTraceObstacle { .. }
            )),
            1,
            "exactly one trace/obstacle clearance violation, got {vs:?}"
        );
    }

    #[test]
    fn clearance_trace_obstacle_clean_for_own_pad() {
        // A SIG trace that ends inside its OWN pad must not raise a clearance.
        let p = problem(
            vec![conn("SIG", &[(5.0, 10.0, "top"), (20.0, 10.0, "top")])],
            vec![pad(&["SIG"], (20.0, 10.0), 1.0, 1.0, &["top"])],
        );
        let s = RouteSolution {
            traces: vec![trace("SIG", "top", 0.25, &[(5.0, 10.0), (20.0, 10.0)])],
            vias: vec![],
        };
        assert!(
            !lint(&p, &s)
                .iter()
                .any(|v| matches!(v, DrcViolation::ClearanceTraceObstacle { .. })),
            "a trace ending in its own pad must not raise a clearance violation"
        );
    }

    #[test]
    fn clearance_via_any_fires_for_foreign_trace() {
        // A NET_B via at (20,10), radius 0.3. A NET_A trace passes at y=10.45:
        // point-seg dist 0.45, minus trace half 0.125 → 0.325 to via centre,
        // minus via radius 0.3 → 0.025 gap < 0.2. Different nets → violation.
        let p = problem(
            vec![
                conn("NET_A", &[(5.0, 10.45, "top"), (35.0, 10.45, "top")]),
                conn("NET_B", &[(20.0, 10.0, "top"), (20.0, 10.0, "bottom")]),
            ],
            vec![],
        );
        let s = RouteSolution {
            traces: vec![trace("NET_A", "top", 0.25, &[(5.0, 10.45), (35.0, 10.45)])],
            vias: vec![via("NET_B", (20.0, 10.0))],
        };
        let vs = lint(&p, &s);
        // The COPPER via-clearance (required == the board clearance 0.2) fires exactly once.
        // The fixture is close enough that the drill-edge HOLE-clearance check (required 0.25)
        // also fires — a second, legitimate violation — so filter to the copper one here.
        assert_eq!(
            count(&vs, |v| matches!(
                v,
                DrcViolation::ClearanceViaAny { required, .. } if *required < 0.24
            )),
            1,
            "exactly one COPPER via/any clearance violation, got {vs:?}"
        );
    }

    #[test]
    fn hole_clearance_fires_for_close_drills() {
        // Two SAME-NET vias 0.4mm apart (drill 0.3 → edge-to-edge 0.4 − 0.15 − 0.15 = 0.10 <
        // KiCAD's 0.25mm hole-to-hole). Same net, so the COPPER via-clearance check skips them
        // (their copper may legally overlap) — only the new drill-edge hole check should fire.
        // Guards the fidelity hole that let small/fat-via configs ship hole_clearance faults.
        let p = problem(
            vec![conn("NET_A", &[(20.0, 10.0, "top"), (20.0, 10.4, "top")])],
            vec![],
        );
        let s = RouteSolution {
            traces: vec![],
            vias: vec![via("NET_A", (20.0, 10.0)), via("NET_A", (20.0, 10.4))],
        };
        let vs = lint(&p, &s);
        let hole = count(&vs, |v| matches!(
            v,
            DrcViolation::ClearanceViaAny { required, .. } if (*required - 0.25).abs() < 1e-9
        ));
        assert_eq!(hole, 1, "drill-to-drill hole clearance must fire once, got {vs:?}");
    }

    #[test]
    fn hole_clearance_fires_for_via_near_foreign_pad() {
        // A via (NET_A) whose DRILL edge sits < 0.25mm from a FOREIGN pad's copper. KiCAD's
        // hole-to-copper rule fires here even though the via COPPER could clear — this is the
        // via↔PAD case the oracle used to skip (a dense fine-clearance escape shipped it).
        // via at (10,10) drill 0.3 (r 0.15); pad NET_B at (10.5,10) is 0.4×0.4 (hw 0.2) →
        // rect-edge gap 0.3, drill-edge gap 0.15 < 0.25.
        let p = problem(
            vec![conn("NET_A", &[(10.0, 10.0, "top")])],
            vec![pad(&["NET_B"], (10.5, 10.0), 0.4, 0.4, &["top"])],
        );
        let s = RouteSolution { traces: vec![], vias: vec![via("NET_A", (10.0, 10.0))] };
        let hole = count(&lint(&p, &s), |v| matches!(
            v,
            DrcViolation::ClearanceViaAny { required, .. } if (*required - 0.25).abs() < 1e-9
        ));
        assert_eq!(hole, 1, "via↔foreign-pad hole clearance must fire once");

        // SAME-NET pad is via-in-pad (intentional) — no hole violation.
        let p2 = problem(
            vec![conn("NET_A", &[(10.0, 10.0, "top")])],
            vec![pad(&["NET_A"], (10.5, 10.0), 0.4, 0.4, &["top"])],
        );
        let s2 = RouteSolution { traces: vec![], vias: vec![via("NET_A", (10.0, 10.0))] };
        let hole2 = count(&lint(&p2, &s2), |v| matches!(
            v,
            DrcViolation::ClearanceViaAny { required, .. } if (*required - 0.25).abs() < 1e-9
        ));
        assert_eq!(hole2, 0, "same-net pad (via-in-pad) must NOT fire hole clearance");
    }

    #[test]
    fn copper_edge_clearance_fires_for_via_near_custom_outline() {
        // A routed via inside a CUSTOM square outline but <0.5mm from an edge. The bbox check (2)
        // can't see the polygon (bounds are 0..100), so only the new (2b) polygon-edge check
        // catches it — mirroring KiCAD's copper-to-edge rule. via at (5.3,10) is 0.3mm from the
        // x=5 edge; with the 0.3mm via radius the copper touches the edge → violation.
        let mut p = problem(vec![conn("NET", &[(5.3, 10.0, "top")])], vec![]);
        p.outline = Some(vec![
            Point2 { x: 5.0, y: 5.0 },
            Point2 { x: 15.0, y: 5.0 },
            Point2 { x: 15.0, y: 15.0 },
            Point2 { x: 5.0, y: 15.0 },
        ]);
        let near = RouteSolution { traces: vec![], vias: vec![via("NET", (5.3, 10.0))] };
        assert!(
            lint(&p, &near).iter().any(|v| matches!(v, DrcViolation::OutOfBounds { .. })),
            "via <0.5mm from a custom outline edge must fire copper-edge clearance"
        );
        // A via centred in the outline is well clear → no edge violation.
        let mid = RouteSolution { traces: vec![], vias: vec![via("NET", (10.0, 10.0))] };
        assert!(
            !lint(&p, &mid).iter().any(|v| matches!(v, DrcViolation::OutOfBounds { .. })),
            "a centred via must NOT fire copper-edge clearance"
        );
    }

    #[test]
    fn clearance_via_any_clean_for_own_trace() {
        // A via and a trace of the SAME net are not a clearance conflict.
        let p = problem(
            vec![conn(
                "NET_A",
                &[(5.0, 10.0, "top"), (20.0, 10.0, "bottom")],
            )],
            vec![],
        );
        let s = RouteSolution {
            traces: vec![
                trace("NET_A", "top", 0.25, &[(5.0, 10.0), (20.0, 10.0)]),
                trace("NET_A", "bottom", 0.25, &[(20.0, 10.0), (20.0, 20.0)]),
            ],
            vias: vec![via("NET_A", (20.0, 10.0))],
        };
        assert!(
            !lint(&p, &s)
                .iter()
                .any(|v| matches!(v, DrcViolation::ClearanceViaAny { .. })),
            "a via must not conflict with its own net's copper"
        );
    }

    #[test]
    fn trace_width_below_min_fires() {
        let p = problem(
            vec![conn("SIG", &[(5.0, 10.0, "top"), (25.0, 10.0, "top")])],
            vec![],
        );
        let s = RouteSolution {
            // 0.10 < min 0.25.
            traces: vec![trace("SIG", "top", 0.10, &[(5.0, 10.0), (25.0, 10.0)])],
            vias: vec![],
        };
        let vs = lint(&p, &s);
        assert_eq!(
            count(&vs, |v| matches!(v, DrcViolation::TraceWidthBelowMin { .. })),
            1,
            "exactly one width violation, got {vs:?}"
        );
    }

    #[test]
    fn out_of_bounds_fires_for_via_off_board() {
        // Via at the very corner: radius pokes past min_x and min_y.
        let p = problem(
            vec![conn("SIG", &[(0.0, 0.0, "top"), (0.0, 0.0, "bottom")])],
            vec![],
        );
        let s = RouteSolution {
            traces: vec![],
            vias: vec![via("SIG", (0.0, 0.0))],
        };
        let vs = lint(&p, &s);
        assert_eq!(
            count(&vs, |v| matches!(v, DrcViolation::OutOfBounds { .. })),
            1,
            "exactly one out-of-bounds violation, got {vs:?}"
        );
    }

    #[test]
    fn out_of_bounds_fires_for_trace_off_board() {
        // Trace whose centreline + half-width crosses max_x.
        let p = problem(
            vec![conn("SIG", &[(50.0, 50.0, "top"), (100.05, 50.0, "top")])],
            vec![],
        );
        let s = RouteSolution {
            traces: vec![trace("SIG", "top", 0.25, &[(50.0, 50.0), (100.05, 50.0)])],
            vias: vec![],
        };
        let vs = lint(&p, &s);
        assert!(
            count(&vs, |v| matches!(v, DrcViolation::OutOfBounds { .. })) >= 1,
            "expected an out-of-bounds violation, got {vs:?}"
        );
    }

    #[test]
    fn connectivity_violations_are_folded_in() {
        // A trace that stops short → connectivity Unconnected, surfaced via the
        // Connectivity variant.
        let p = problem(
            vec![conn(
                "SIG",
                &[(5.0, 10.0, "top"), (25.0, 10.0, "top"), (45.0, 10.0, "top")],
            )],
            vec![],
        );
        let s = RouteSolution {
            traces: vec![trace("SIG", "top", 0.25, &[(5.0, 10.0), (25.0, 10.0)])],
            vias: vec![],
        };
        let vs = lint(&p, &s);
        assert!(
            vs.iter().any(|v| matches!(
                v,
                DrcViolation::Connectivity {
                    violation: Violation::Unconnected { .. }
                }
            )),
            "connectivity Unconnected must be folded into the lint, got {vs:?}"
        );
    }

    // ── InvalidLayer trigger test ─────────────────────────────────────────────

    #[test]
    fn invalid_layer_fires_for_inner1_on_two_layer_board() {
        // A trace on "inner1" on a 2-layer board (which only has "top" = 0 and
        // "bottom" = 1). "inner1" requires at least 3 layers (inner indices are
        // strictly between top and bottom). LayerRef("inner1").index(2) → None.
        let p = problem(
            vec![conn("SIG", &[(5.0, 10.0, "top"), (25.0, 10.0, "inner1")])],
            vec![],
        );
        let s = RouteSolution {
            traces: vec![trace("SIG", "inner1", 0.25, &[(5.0, 10.0), (25.0, 10.0)])],
            vias: vec![],
        };
        let vs = lint(&p, &s);
        let invalid_count = count(&vs, |v| matches!(v, DrcViolation::InvalidLayer { .. }));
        assert_eq!(
            invalid_count, 2,
            "expected exactly 2 InvalidLayer violations: \
             one for the solution trace on inner1, one for the connection route point on inner1, \
             got {vs:?}"
        );
        // Check that the payload fields are set correctly on the trace violation.
        let trace_viol = vs.iter().find(|v| {
            matches!(
                v,
                DrcViolation::InvalidLayer { layer, layer_count: 2, .. }
                    if layer == "inner1"
            )
        });
        assert!(
            trace_viol.is_some(),
            "must find an InvalidLayer with layer=inner1 and layer_count=2, got {vs:?}"
        );
    }

    #[test]
    fn invalid_layer_does_not_fire_for_valid_layers() {
        // "top" and "bottom" are always valid on a 2-layer board.
        let p = problem(
            vec![
                conn("A", &[(5.0, 10.0, "top"), (25.0, 10.0, "top")]),
                conn("B", &[(5.0, 20.0, "bottom"), (25.0, 20.0, "bottom")]),
            ],
            vec![],
        );
        let s = RouteSolution {
            traces: vec![
                trace("A", "top", 0.25, &[(5.0, 10.0), (25.0, 10.0)]),
                trace("B", "bottom", 0.25, &[(5.0, 20.0), (25.0, 20.0)]),
            ],
            vias: vec![],
        };
        let vs = lint(&p, &s);
        assert!(
            !vs.iter().any(|v| matches!(v, DrcViolation::InvalidLayer { .. })),
            "valid layer names must not raise InvalidLayer, got {vs:?}"
        );
    }
}
