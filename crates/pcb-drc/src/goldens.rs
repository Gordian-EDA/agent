//! Real routed boards for the tests, plus [`tiled`] — the same board repeated
//! across the plane so the copper count clears the broad-phase index's
//! small-input threshold.

use pcb_model::{RouteSolution, RoutingView};
use std::path::Path;

fn read<T: serde::de::DeserializeOwned>(name: &str) -> T {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name);
    let json = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {name}: {e}"));
    serde_json::from_str(&json).unwrap_or_else(|e| panic!("parse {name}: {e}"))
}

/// Every golden problem/solution pair, named.
pub fn boards() -> Vec<(&'static str, RoutingView, RouteSolution)> {
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

/// `n × n` copies of a board, each offset by one board pitch and carrying its
/// own connection names, so tiles never interact and the copper count grows
/// quadratically.
pub fn tiled(view: &RoutingView, solution: &RouteSolution, n: i32) -> (RoutingView, RouteSolution) {
    let pitch = (
        view.bounds.max_x - view.bounds.min_x,
        view.bounds.max_y - view.bounds.min_y,
    );
    let mut out_view = view.clone();
    let mut out_sol = solution.clone();
    out_view.obstacles.clear();
    out_view.connections.clear();
    out_sol.traces.clear();
    out_sol.vias.clear();
    out_view.bounds.max_x = view.bounds.min_x + pitch.0 * n as f64;
    out_view.bounds.max_y = view.bounds.min_y + pitch.1 * n as f64;
    out_view.outline = None;

    for i in 0..n {
        for j in 0..n {
            let (dx, dy) = (i as f64 * pitch.0, j as f64 * pitch.1);
            let tag = |name: &str| format!("{name}#{i}_{j}");

            for ob in &view.obstacles {
                let mut ob = ob.clone();
                ob.center.x += dx;
                ob.center.y += dy;
                ob.connected_to = ob.connected_to.iter().map(|n| tag(n)).collect();
                out_view.obstacles.push(ob);
            }
            for conn in &view.connections {
                let mut conn = conn.clone();
                conn.name = tag(&conn.name);
                for p in &mut conn.points_to_connect {
                    p.x += dx;
                    p.y += dy;
                }
                out_view.connections.push(conn);
            }
            for trace in &solution.traces {
                let mut trace = trace.clone();
                trace.connection = tag(&trace.connection);
                for p in &mut trace.path {
                    p.x += dx;
                    p.y += dy;
                }
                out_sol.traces.push(trace);
            }
            for via in &solution.vias {
                let mut via = via.clone();
                via.connection = tag(&via.connection);
                via.at.x += dx;
                via.at.y += dy;
                out_sol.vias.push(via);
            }
        }
    }
    (out_view, out_sol)
}
