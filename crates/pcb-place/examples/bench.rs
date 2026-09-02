//! Stand-alone placement bench: run the tuned placer over the golden boards in
//! `fixtures/` and print wall time plus the leaf's own quality metrics.
//!
//! `cargo run -p pcb-place --example bench` — no agent, no KiCAD, no network.
//! The routability probe is stubbed (a constant estimate), so this measures the
//! placement search alone.

use std::path::Path;
use std::time::Instant;

use pcb_model::{
    Budget, PcbPlacer, PlacementHints, PlacementView, RouteEstimate, RouteProbe, RoutingView,
};
use pcb_place::TunedPlacer;

/// A probe that reports every board perfectly routable — the neutral collaborator
/// that keeps the bench measuring placement and nothing else.
struct NullProbe;

impl RouteProbe for NullProbe {
    fn name(&self) -> &'static str {
        "null"
    }
    fn estimate(&self, _view: &RoutingView) -> RouteEstimate {
        RouteEstimate {
            fault_weight: 0,
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

fn main() {
    println!(
        "{:<14} {:>6} {:>9} {:>10} {:>9} {:>7}",
        "board", "parts", "ms", "hpwl", "cost", "legal"
    );
    for name in ["led-r.json", "dense-ic.json"] {
        let view = load(name);
        let hints = PlacementHints::default();
        let started = Instant::now();
        let result = TunedPlacer.place(&view, &hints, &NullProbe, &Budget::unlimited());
        let ms = started.elapsed().as_secs_f64() * 1e3;
        println!(
            "{:<14} {:>6} {:>9.1} {:>10.2} {:>9.2} {:>7}",
            name.trim_end_matches(".json"),
            view.parts.len(),
            ms,
            result.report.hpwl,
            result.report.layout_cost,
            result.legal
        );
    }
}
