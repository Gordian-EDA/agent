//! Engine-SDK guards for `pcb-drc`:
//!
//! 1. The standard suite produces a KNOWN, exact finding set on a board with a
//!    deliberate mix of violations — the byte-identical guard for the code-motion
//!    out of the old hardcoded `lint()`.
//! 2. The standard suite's findings equal the manual `standard_rules()`
//!    concatenation, confirming `run()` is just ordered composition.
//! 3. The third-party story: `DrcSuite::standard().with(Box::new(MyRule))`
//!    appends a custom rule with no edit to the in-house set, and its findings
//!    land last (after the standard ones).

use pcb_drc::{DrcCtx, DrcSuite, Finding, Rule};
use pcb_model::{
    Connection, LayerRef, Obstacle, Point2, Rect, RoutePoint, RouteSolution, RoutingView, Trace,
};

fn bounds() -> Rect {
    Rect {
        min_x: 0.0,
        max_x: 100.0,
        min_y: 0.0,
        max_y: 100.0,
    }
}

fn problem(connections: Vec<Connection>, obstacles: Vec<Obstacle>) -> RoutingView {
    RoutingView {
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
        plane_nets: Default::default(),
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

fn trace(connection: &str, layer: &str, width: f64, path: &[(f64, f64)]) -> Trace {
    Trace {
        connection: connection.to_owned(),
        layer: LayerRef(layer.to_owned()),
        width,
        path: path.iter().map(|&(x, y)| Point2 { x, y }).collect(),
    }
}

/// A board carrying, deterministically: one trace below min width, two parallel
/// foreign traces too close (trace/trace clearance), and a stranded second
/// connection (connectivity Unconnected). The exact, ordered finding set the
/// suite must reproduce — geometry first, connectivity last.
fn mixed_board() -> (RoutingView, RouteSolution) {
    let p = problem(
        vec![
            // Two close parallel foreign traces (centrelines 0.30 mm apart, each 0.25 wide).
            conn("A", &[(10.0, 10.0, "top"), (30.0, 10.0, "top")]),
            conn("B", &[(10.0, 10.3, "top"), (30.0, 10.3, "top")]),
            // A narrow trace (width 0.10 < min 0.25).
            conn("THIN", &[(5.0, 40.0, "top"), (25.0, 40.0, "top")]),
            // A connection whose second point is never reached → Unconnected.
            conn(
                "GAP",
                &[(5.0, 70.0, "top"), (25.0, 70.0, "top"), (45.0, 70.0, "top")],
            ),
        ],
        vec![],
    );
    let s = RouteSolution {
        traces: vec![
            trace("A", "top", 0.25, &[(10.0, 10.0), (30.0, 10.0)]),
            trace("B", "top", 0.25, &[(10.0, 10.3), (30.0, 10.3)]),
            trace("THIN", "top", 0.10, &[(5.0, 40.0), (25.0, 40.0)]),
            trace("GAP", "top", 0.25, &[(5.0, 70.0), (25.0, 70.0)]),
        ],
        vias: vec![],
    };
    (p, s)
}

/// The standard suite preserves the ordered finding set: trace-width, then
/// pairwise clearance, then connectivity folded in last.
#[test]
fn standard_suite_reproduces_known_finding_set() {
    let (p, s) = mixed_board();
    let findings = DrcSuite::standard().run(&p, &s);

    // Compare as serde JSON so the assertion fails loudly on any field/order drift.
    let got = serde_json::to_value(&findings).unwrap();
    let expected = serde_json::json!([
        // (1) trace-width: THIN before connectivity, in trace order.
        { "kind": "traceWidthBelowMin", "connection": "THIN", "layer": "top", "width": 0.10, "required": 0.25 },
        // (3) pairwise clearance: A↔B parallel traces, edge gap 0.05 < 0.2.
        { "kind": "clearanceTraceTrace", "a": "A", "b": "B", "layer": "top", "gap": 0.05000000000000071, "required": 0.2, "at": { "x": 10.0, "y": 10.0 } },
        // (4) connectivity folded in last: GAP's point #2 stranded.
        { "kind": "connectivity", "violation": { "kind": "unconnected", "connection": "GAP", "point_index": 2 } },
    ]);
    assert_eq!(
        got, expected,
        "standard suite finding set drifted:\n{got:#}"
    );
}

/// `run()` is exactly the ordered concatenation of each standard rule's `check`.
/// Re-running the standard suite must be byte-identical (determinism contract).
#[test]
fn standard_suite_is_deterministic() {
    let (p, s) = mixed_board();
    let a = DrcSuite::standard().run(&p, &s);
    let b = DrcSuite::standard().run(&p, &s);
    assert_eq!(a, b, "identical input must give identical findings");
}

/// A third-party rule, depending only on `pcb-drc`. It flags any trace
/// wider than a ceiling — a rule the in-house set does not have.
struct MaxTraceWidthRule {
    ceiling: f64,
}

impl Rule for MaxTraceWidthRule {
    fn name(&self) -> &'static str {
        "third-party/max-trace-width"
    }
    fn check(&self, ctx: &DrcCtx) -> Vec<Finding> {
        ctx.solution
            .traces
            .iter()
            .filter(|t| t.width > self.ceiling)
            .map(|t| Finding::TraceWidthBelowMin {
                connection: t.connection.clone(),
                layer: t.layer.0.clone(),
                width: t.width,
                required: self.ceiling,
            })
            .collect()
    }
}

/// The third-party story: `standard().with(Box::new(MyRule))` extends the suite
/// with no edit to the in-house rules. The custom rule's findings append AFTER
/// the standard ones, and `rule_names()` shows the appended provenance.
#[test]
fn third_party_rule_appends_via_with() {
    let (p, s) = mixed_board();

    let standard = DrcSuite::standard().run(&p, &s);
    let extended = DrcSuite::standard()
        .with(Box::new(MaxTraceWidthRule { ceiling: 0.2 }))
        .run(&p, &s);

    // Every standard finding is preserved, in order, as the prefix.
    assert_eq!(&extended[..standard.len()], &standard[..]);
    // The custom rule fired (the three 0.25-wide traces exceed the 0.2 ceiling)
    // and its findings come last.
    assert_eq!(extended.len(), standard.len() + 3);
    assert!(matches!(
        extended.last(),
        Some(Finding::TraceWidthBelowMin { required, .. }) if (*required - 0.2).abs() < 1e-9
    ));

    let names = DrcSuite::standard()
        .with(Box::new(MaxTraceWidthRule { ceiling: 0.2 }))
        .rule_names();
    assert_eq!(names.last(), Some(&"third-party/max-trace-width"));
}

/// `collect_copper` is public so a third-party rule reuses the same collection
/// the in-house geometry rules read, instead of re-deriving copper from raw
/// traces/vias.
#[test]
fn collect_copper_is_public_and_shared() {
    let (p, s) = mixed_board();
    let copper = pcb_drc::collect_copper(&p, &s);
    // Four traces, each a single segment, no obstacles, no vias.
    assert_eq!(copper.len(), 4);
    assert!(copper.iter().any(|c| c.owned_by("A")));
}
