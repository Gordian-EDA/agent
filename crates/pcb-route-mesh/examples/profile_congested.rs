//! Stage-by-stage timing of the detailed router on a fixture, to find where the
//! time actually goes. Usage: cargo run --release -p pcb-route-mesh --example
//! profile_congested -- [fixture-name]  (default congested.json).

use std::path::Path;
use std::time::Instant;

use pcb_route_mesh::crossing::assign_crossings;
use pcb_route_mesh::detail::route_cells;
use pcb_route_mesh::mesh::CapacityMesh;
use pcb_route_mesh::pathing::global_route;
use pcb_route_mesh::pipeline::route_detailed;
use pcb_model::RouteProblem;

fn main() {
    let name = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "congested.json".into());
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(&name);
    let json = std::fs::read_to_string(&path).unwrap();
    let problem: RouteProblem = serde_json::from_str(&json).unwrap();
    println!("fixture {name}: {} connections", problem.connections.len());

    let t = Instant::now();
    let global = global_route(&problem);
    println!(
        "global_route       {:>8.1} ms",
        t.elapsed().as_secs_f64() * 1e3
    );

    let t = Instant::now();
    let mesh = CapacityMesh::build(&problem);
    println!(
        "mesh::build        {:>8.1} ms",
        t.elapsed().as_secs_f64() * 1e3
    );

    let t = Instant::now();
    let assignment = assign_crossings(&problem, &mesh, &global.plan);
    println!(
        "assign_crossings   {:>8.1} ms",
        t.elapsed().as_secs_f64() * 1e3
    );

    let t = Instant::now();
    let cells = route_cells(&problem, &mesh, &assignment);
    println!(
        "route_cells        {:>8.1} ms   ({} cell routes, {} failed)",
        t.elapsed().as_secs_f64() * 1e3,
        cells.cell_routes.len(),
        cells.failed.len()
    );

    let t = Instant::now();
    let r = route_detailed(&problem);
    println!(
        "route_detailed ALL {:>8.1} ms   ({} failed)",
        t.elapsed().as_secs_f64() * 1e3,
        r.failed.len()
    );
}
