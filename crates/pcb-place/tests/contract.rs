//! The placement leaf's contract: determinism, deadline behaviour, and the
//! output invariants every placer must hold.

use std::path::Path;
use std::time::{Duration, Instant};

use pcb_model::{
    Budget, LockedAt, PcbPlacer, Placement, PlacementHints, PlacementView, Point2, RouteEstimate,
    RouteProbe, RoutingView,
};
use pcb_place::TunedPlacer;

/// A deterministic stub collaborator: the placer must never need a real router.
struct StubProbe;

impl RouteProbe for StubProbe {
    fn name(&self) -> &'static str {
        "stub"
    }
    fn estimate(&self, view: &RoutingView) -> RouteEstimate {
        RouteEstimate {
            fault_weight: view.connections.len(),
            geometry_violations: 0,
            failed_nets: 0,
            via_count: 0,
            wirelength: 0.0,
        }
    }
}

fn load(name: &str) -> PlacementView {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name);
    let json = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {name}: {e}"));
    serde_json::from_str(&json).unwrap_or_else(|e| panic!("parse {name}: {e}"))
}

fn boards() -> Vec<(&'static str, PlacementView)> {
    ["led-r.json", "dense-ic.json"]
        .into_iter()
        .map(|n| (n, load(n)))
        .collect()
}

fn place(view: &PlacementView, budget: &Budget) -> Vec<Placement> {
    TunedPlacer
        .place(view, &PlacementHints::default(), &StubProbe, budget)
        .placements
}

#[test]
fn same_input_and_seed_places_byte_identically() {
    for (name, view) in boards() {
        let budget = Budget {
            seed: 7,
            ..Budget::unlimited()
        };
        let a = serde_json::to_string(&place(&view, &budget)).unwrap();
        let b = serde_json::to_string(&place(&view, &budget)).unwrap();
        assert_eq!(a, b, "{name} placement must be reproducible");
    }
}

#[test]
fn expired_budget_returns_promptly_with_a_valid_placement() {
    for (name, view) in boards() {
        let budget = Budget {
            deadline: Some(Instant::now() - Duration::from_secs(1)),
            ..Budget::unlimited()
        };
        let started = Instant::now();
        let placements = place(&view, &budget);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{name} must abandon a spent budget promptly"
        );
        assert_eq!(
            placements.len(),
            view.parts.len(),
            "{name} must still place every part"
        );
    }
}

#[test]
fn every_part_is_placed_exactly_once() {
    for (name, view) in boards() {
        let placements = place(&view, &Budget::unlimited());
        let mut refs: Vec<&str> = placements.iter().map(|p| p.reference.as_str()).collect();
        let mut want: Vec<&str> = view.parts.iter().map(|p| p.reference.as_str()).collect();
        refs.sort_unstable();
        want.sort_unstable();
        assert_eq!(refs, want, "{name} must place each part exactly once");
        for p in &placements {
            assert!(p.at.x.is_finite() && p.at.y.is_finite(), "{name}: {p:?}");
        }
    }
}

#[test]
fn locked_parts_are_never_moved() {
    let mut view = load("dense-ic.json");
    let anchor = LockedAt {
        at: Point2::new(20.0, 15.0),
        rotation: 90.0,
    };
    view.parts[0].locked = Some(anchor.clone());

    for budget in [
        Budget::unlimited(),
        Budget {
            deadline: Some(Instant::now() - Duration::from_secs(1)),
            ..Budget::unlimited()
        },
    ] {
        let placements = place(&view, &budget);
        let seated = placements
            .iter()
            .find(|p| p.reference == view.parts[0].reference)
            .expect("locked part is placed");
        assert_eq!(seated.at, anchor.at, "a locked part must not move");
        assert_eq!(seated.rotation, anchor.rotation);
    }
}
