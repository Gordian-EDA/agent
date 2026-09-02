//! The grid routing leaf's contract: determinism, deadline behaviour, and the
//! output invariants every router must hold.

use std::path::Path;
use std::time::{Duration, Instant};

use pcb_drc::StandardDrc;
use pcb_model::{Budget, Drc, PcbRouter, RouteResult, RoutingView};
use pcb_route_grid::probe::GridRouteProbe;
use pcb_route_grid::router::{GridRouter, GridSinglePassRouter};

fn load(name: &str) -> RoutingView {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name);
    let json = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {name}: {e}"));
    serde_json::from_str(&json).unwrap_or_else(|e| panic!("parse {name}: {e}"))
}

fn boards() -> Vec<(&'static str, RoutingView)> {
    ["quad.json", "led-r.json"]
        .into_iter()
        .map(|n| (n, load(n)))
        .collect()
}

fn route(view: &RoutingView, budget: &Budget) -> RouteResult {
    GridRouter::new(&StandardDrc).route(view, budget)
}

#[test]
fn same_input_routes_byte_identically() {
    for (name, view) in boards() {
        let a = serde_json::to_string(&route(&view, &Budget::unlimited())).unwrap();
        let b = serde_json::to_string(&route(&view, &Budget::unlimited())).unwrap();
        assert_eq!(a, b, "{name} route must be reproducible");
    }
}

#[test]
fn expired_budget_returns_promptly_with_an_honest_result() {
    for (name, view) in boards() {
        let budget = Budget {
            deadline: Some(Instant::now() - Duration::from_secs(1)),
            ..Budget::unlimited()
        };
        let started = Instant::now();
        let result = route(&view, &budget);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{name} must abandon a spent budget promptly"
        );
        assert!(result.solution.traces.is_empty());
        assert_eq!(
            result.failed.len(),
            view.connections.len(),
            "{name}: an abandoned run reports every net failed, never silently routed"
        );
    }
}

#[test]
fn every_trace_is_on_a_declared_layer_of_a_requested_connection() {
    for (name, view) in boards() {
        let result = route(&view, &Budget::unlimited());
        let requested: Vec<&str> = view.connections.iter().map(|c| c.name.as_str()).collect();
        for trace in &result.solution.traces {
            assert!(
                requested.contains(&trace.connection.as_str()),
                "{name}: trace for unrequested connection {}",
                trace.connection
            );
            assert!(
                trace.layer.index(view.layer_count).is_some(),
                "{name}: trace on undeclared layer {:?}",
                trace.layer
            );
            assert!(trace.path.len() >= 2, "{name}: degenerate trace polyline");
        }
        for via in &result.solution.vias {
            assert!(
                requested.contains(&via.connection.as_str()),
                "{name}: via for unrequested connection {}",
                via.connection
            );
        }
    }
}

#[test]
fn no_emitted_copper_belongs_to_a_failed_net() {
    for (name, view) in boards() {
        let result = route(&view, &Budget::unlimited());
        for failed in &result.failed {
            assert!(
                !result
                    .solution
                    .traces
                    .iter()
                    .any(|t| t.connection == failed.connection),
                "{name}: {} is reported failed but has copper",
                failed.connection
            );
        }
    }
}

#[test]
fn emitted_copper_is_geometry_clean() {
    for (name, view) in boards() {
        let result = route(&view, &Budget::unlimited());
        assert_eq!(
            StandardDrc.geometry_violations(&view, &result.solution),
            0,
            "{name}: a router must never ship copper that fails DRC"
        );
    }
}

#[test]
fn the_probe_estimate_matches_the_single_pass_router_it_wraps() {
    for (name, view) in boards() {
        let probe = GridRouteProbe::new(&StandardDrc);
        let estimate = pcb_model::RouteProbe::estimate(&probe, &view);
        let direct = GridSinglePassRouter::new(&StandardDrc).route(&view, &Budget::unlimited());
        assert_eq!(
            estimate.failed_nets,
            direct.failed.len(),
            "{name}: the probe must report what its router did"
        );
        assert_eq!(estimate.via_count, direct.solution.metrics().via_count);
    }
}
