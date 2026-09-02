//! The premium routing leaf's contract: determinism, deadline behaviour, and the
//! output invariants every router must hold.
//!
//! The collaborators are injected — swapping the grid sub-router for a stub is
//! all it takes to exercise this leaf in isolation.

use std::path::Path;
use std::time::{Duration, Instant};

use pcb_drc::StandardDrc;
use pcb_model::{Budget, Drc, PcbRouter, RouteResult, RoutingView};
use pcb_route_grid::router::{GridRouter, GridSinglePassRouter};
use pcb_route_mesh::deps::MeshDeps;
use pcb_route_mesh::pipeline::MeshRouter;

/// A sub-router that routes nothing: proves the premium leaf never depends on
/// its fallback succeeding, and that an injected stub is enough to drive it.
struct NoopRouter;

impl PcbRouter for NoopRouter {
    fn name(&self) -> &'static str {
        "noop"
    }
    fn route(&self, view: &RoutingView, _budget: &Budget) -> RouteResult {
        RouteResult::abandoned(view, "noop", "stub sub-router routes nothing")
    }
}

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
    let drc = StandardDrc;
    let grid = GridRouter::new(&drc);
    let grid_seed = GridSinglePassRouter::new(&drc);
    MeshRouter::new(MeshDeps {
        drc: &drc,
        grid: &grid,
        grid_seed: &grid_seed,
        budget: *budget,
    })
    .route(view, budget)
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
        assert_eq!(result.failed.len(), view.connections.len());
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
fn a_stub_sub_router_still_yields_a_valid_result() {
    let drc = StandardDrc;
    let view = load("led-r.json");
    let result = MeshRouter::new(MeshDeps {
        drc: &drc,
        grid: &NoopRouter,
        grid_seed: &NoopRouter,
        budget: Budget::unlimited(),
    })
    .route(&view, &Budget::unlimited());

    assert_eq!(
        drc.geometry_violations(&view, &result.solution),
        0,
        "a stubbed collaborator must not make the leaf emit dirty copper"
    );
    for trace in &result.solution.traces {
        assert!(trace.layer.index(view.layer_count).is_some());
    }
}
