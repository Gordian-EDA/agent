//! Routing-only scoreboard over the required corpus plus large-board stress cases.
//!
//! `fixtures/corpus/*.json` are the prepared [`RoutingView`]s the workflow hands
//! the router for those boards, captured once. The placer is budget-driven and
//! so re-places differently run to run; routing these fixed problems isolates
//! the router's own numbers, which is what a tidiness or speed change has to
//! move.
//!
//! `cargo run --release -p pcb-route-mesh --example corpus` — no agent, no
//! placer, no KiCAD, no network.

use std::path::Path;
use std::time::Instant;

use pcb_drc::StandardDrc;
use pcb_model::{Budget, Drc, PcbRouter, RoutingView};
use pcb_route_grid::router::{GridRouter, GridSinglePassRouter};
use pcb_route_mesh::deps::MeshDeps;
use pcb_route_mesh::pipeline::MeshRouter;

const BOARDS: [&str; 9] = [
    "rc-divider",
    "transistor-led-driver",
    "keepout-route",
    "rc-lowpass-chain",
    "power-buck",
    "led-array",
    "led-array-60",
    "bga25-route",
    "mcu-board",
];

fn load(name: &str) -> RoutingView {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/corpus")
        .join(format!("{name}.json"));
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

    println!("board,vias,wirelength_mm,bends,bends_per_mm,off_angle,failed,drc,route_ms");
    let mut totals = (0usize, 0.0, 0usize, 0usize, 0usize, 0.0);
    for name in BOARDS {
        let view = load(name);
        let started = Instant::now();
        let result = router.route(&view, &Budget::unlimited());
        let ms = started.elapsed().as_secs_f64() * 1e3;
        let m = result.solution.metrics();
        let faults = drc.geometry_violations(&view, &result.solution);
        println!(
            "{name},{},{:.2},{},{:.3},{},{},{faults},{ms:.0}",
            m.via_count,
            m.wirelength,
            m.bend_count,
            m.bend_count as f64 / m.wirelength.max(f64::EPSILON),
            m.off_angle_segments,
            result.failed.len(),
        );
        totals.0 += m.via_count;
        totals.1 += m.wirelength;
        totals.2 += m.bend_count;
        totals.3 += m.off_angle_segments;
        totals.4 += result.failed.len() + faults;
        totals.5 += ms;
    }
    println!(
        "TOTAL,{},{:.2},{},{:.3},{},{},,{:.0}",
        totals.0,
        totals.1,
        totals.2,
        totals.2 as f64 / totals.1,
        totals.3,
        totals.4,
        totals.5
    );
}
