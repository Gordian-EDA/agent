//! The DRC leaf's contract: determinism, prompt evaluation, and the report
//! invariants every rule set must hold.

use std::path::Path;
use std::time::{Duration, Instant};

use pcb_drc::StandardDrc;
use pcb_model::{Drc, LayerRef, Point2, RouteSolution, RoutingView, Trace};

fn read<T: serde::de::DeserializeOwned>(name: &str) -> T {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name);
    let json = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {name}: {e}"));
    serde_json::from_str(&json).unwrap_or_else(|e| panic!("parse {name}: {e}"))
}

fn boards() -> Vec<(&'static str, RoutingView, RouteSolution)> {
    ["quad", "led-r", "congested"]
        .into_iter()
        .map(|b| {
            (
                b,
                read(&format!("{b}.problem.json")),
                read(&format!("{b}.solution.json")),
            )
        })
        .collect()
}

#[test]
fn the_same_input_reports_byte_identical_findings() {
    for (name, view, solution) in boards() {
        let a = serde_json::to_string(&StandardDrc.check(&view, &solution)).unwrap();
        let b = serde_json::to_string(&StandardDrc.check(&view, &solution)).unwrap();
        assert_eq!(a, b, "{name} findings must be reproducible");
    }
}

#[test]
fn checking_a_golden_board_is_prompt() {
    for (name, view, solution) in boards() {
        let started = Instant::now();
        let _ = StandardDrc.check(&view, &solution);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{name} must lint promptly"
        );
    }
}

#[test]
fn findings_are_reported_in_suite_order_geometry_before_connectivity() {
    for (name, view, solution) in boards() {
        let findings = StandardDrc.check(&view, &solution);
        let first_connectivity = findings.iter().position(|f| !f.is_geometry());
        let last_geometry = findings.iter().rposition(|f| f.is_geometry());
        if let (Some(first), Some(last)) = (first_connectivity, last_geometry) {
            assert!(
                last < first,
                "{name}: connectivity findings must be folded in last"
            );
        }
    }
}

#[test]
fn every_geometry_finding_names_the_nets_it_implicates() {
    for (name, view, solution) in boards() {
        for finding in StandardDrc.check(&view, &solution) {
            if finding.is_geometry() {
                assert!(
                    !finding.nets().is_empty(),
                    "{name}: a geometry finding must implicate a net: {finding:?}"
                );
            }
        }
    }
}

#[test]
fn the_drop_passes_leave_the_surviving_copper_clean() {
    let (mut view, mut solution) = {
        let (_, view, solution) = boards().remove(0);
        (view, solution)
    };
    // Add copper that is guaranteed to violate: a foreign trace laid directly on
    // top of an existing one.
    let victim = solution.traces[0].clone();
    view.connections.push(pcb_model::Connection {
        name: "INTRUDER".to_owned(),
        points_to_connect: Vec::new(),
    });
    solution.traces.push(Trace {
        connection: "INTRUDER".to_owned(),
        ..victim
    });
    assert!(
        StandardDrc
            .check(&view, &solution)
            .iter()
            .any(|f| f.is_geometry()),
        "the overlaid trace must violate geometry"
    );

    let dropped = StandardDrc.drop_violating_copper(&view, &mut solution);
    assert!(!dropped.is_empty(), "the offender must be dropped");
    assert!(
        dropped.windows(2).all(|w| w[0] < w[1]),
        "dropped net names are sorted and unique"
    );
    assert!(
        !StandardDrc
            .check(&view, &solution)
            .iter()
            .any(|f| f.is_geometry()),
        "surviving copper must be geometry-clean"
    );
}

#[test]
fn an_empty_solution_on_an_empty_board_reports_nothing() {
    let view = RoutingView {
        layer_count: 2,
        min_trace_width: 0.2,
        obstacles: Vec::new(),
        connections: Vec::new(),
        bounds: pcb_model::Rect::new(0.0, 0.0, 10.0, 10.0),
        clearance: 0.2,
        via_diameter: 0.6,
        via_drill: 0.3,
        net_widths: Default::default(),
        outline: None,
        plane_nets: Default::default(),
        escape_layers: Default::default(),
        fixed_copper: RouteSolution::default(),
        nets: None,
    };
    assert!(StandardDrc.check(&view, &RouteSolution::default()).is_empty());
    let _ = (LayerRef::top(), Point2::new(0.0, 0.0));
}
