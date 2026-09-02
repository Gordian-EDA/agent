//! Stand-alone premium-router bench: route the golden boards in `fixtures/` and
//! print wall time plus the leaf's own metrics.
//!
//! `cargo run -p pcb-route-mesh --example bench` — no agent, no KiCAD, no
//! network. The design-rule oracle and the grid sub-routers are injected here,
//! exactly as the composition root injects them in production.
//!
//! `congested` takes minutes in a debug build, so it runs only when asked:
//! `cargo run -p pcb-route-mesh --example bench -- congested`.

use std::path::Path;
use std::time::Instant;

use pcb_drc::StandardDrc;
use pcb_model::{Budget, Drc, PcbRouter, RoutingView};
use pcb_route_grid::router::{GridRouter, GridSinglePassRouter};
use pcb_route_mesh::deps::MeshDeps;
use pcb_route_mesh::pipeline::MeshRouter;

fn load(name: &str) -> RoutingView {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name);
    let json = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {name}: {e}"));
    serde_json::from_str(&json).unwrap_or_else(|e| panic!("parse {name}: {e}"))
}

fn main() {
    let drc = StandardDrc;
    let grid = GridRouter::new(&drc);
    let grid_seed = GridSinglePassRouter::new(&drc);
    let router = MeshRouter::new(MeshDeps {
        drc: &drc,
        grid: &grid,
        grid_seed: &grid_seed,
        budget: Budget::unlimited(),
    });

    println!(
        "{:<12} {:>5} {:>9} {:>7} {:>6} {:>7} {:>11} {:>5}",
        "board", "nets", "ms", "traces", "vias", "failed", "wirelength", "drc"
    );
    let mut boards = vec!["quad.json", "led-r.json"];
    if std::env::args().any(|a| a == "congested") {
        boards.push("congested.json");
    }
    for name in boards {
        let view = load(name);
        let started = Instant::now();
        let result = router.route(&view, &Budget::unlimited());
        let ms = started.elapsed().as_secs_f64() * 1e3;
        let metrics = result.solution.metrics();
        println!(
            "{:<12} {:>5} {:>9.1} {:>7} {:>6} {:>7} {:>11.2} {:>5}",
            name.trim_end_matches(".json"),
            view.connections.len(),
            ms,
            metrics.trace_count,
            metrics.via_count,
            result.failed.len(),
            metrics.wirelength,
            drc.geometry_violations(&view, &result.solution)
        );
    }
}
