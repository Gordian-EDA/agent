use gordian_core::AgentRuntime;
use grid_astar::astar::{AStarCosts, DIAG_COST};
use kicad_env::KicadEnv;
use negotiated_mesh::pipeline::{NegotiatedMeshRouter, route_auto};
use pcb_model::place::to_route_problem;
use pcb_model::{
    FailedNet, LayerRef, Point2, RouteProblem, RouteResult, RouteSolution, Router, Trace, ViaSpan,
};
use pcb_place::placement::Placement;
use std::collections::{BTreeMap, BTreeSet};

fn main() -> anyhow::Result<()> {
    let project = std::env::args()
        .nth(1)
        .expect("usage: debug_file_route <project-dir>");
    let env = KicadEnv::detect().expect("no KiCAD environment");
    let ctx = AgentRuntime::for_project(env, std::path::PathBuf::from(&project))?;
    let seed_text = std::fs::read_to_string(ctx.project_dir().join(".gordian/board.seed.json"))?;
    let place_text =
        std::fs::read_to_string(ctx.project_dir().join(".gordian/board.placement.json"))?;
    let seed: gordian_core::tools_pcb::BoardSeed = serde_json::from_str(&seed_text)?;
    let placements: Vec<Placement> = serde_json::from_str(&place_text)?;
    let problem = gordian_core::tools_pcb::debug_place_problem_from_seed(&seed, &ctx)
        .map_err(anyhow::Error::msg)?;
    let mut rp = to_route_problem(&problem, &placements);
    rp.via_diameter = seed.rules.via_diameter;
    rp.via_drill = seed.rules.via_drill;
    rp.net_widths = seed.rules.net_widths.clone();
    let poured: std::collections::BTreeSet<String> =
        seed.rules.pours.iter().map(|p| p.net.clone()).collect();
    if !poured.is_empty() {
        rp.connections.retain(|conn| !poured.contains(&conn.name));
    }
    println!(
        "place: {} parts; route: {} connections, {} obstacles, bounds {:?}, poured {:?}",
        problem.parts.len(),
        rp.connections.len(),
        rp.obstacles.len(),
        rp.bounds,
        poured
    );
    for conn in rp.connections.iter().take(12) {
        println!(
            "conn {}: {} points hpwl {:.1}",
            conn.name,
            conn.points_to_connect.len(),
            conn.half_perimeter()
        );
        for p in conn.points_to_connect.iter().take(4) {
            println!("  {:.3},{:.3} {}", p.x, p.y, p.layer.0);
        }
    }
    let strict = grid_astar::router::route(&rp);
    print_result("strict", &rp, &strict);
    let lenient = grid_astar::router::route_lenient(&rp);
    print_result("lenient", &rp, &lenient);
    let ortho = grid_astar::router::route_orthogonal(&rp);
    print_result("ortho", &rp, &ortho);
    let ortho_lenient = grid_astar::router::route_orthogonal_lenient(&rp);
    print_result("ortho_lenient", &rp, &ortho_lenient);
    let router = grid_astar::router::GridAStarRouter.route(&rp);
    print_result("router", &rp, &router);
    audit_postprocess(&rp, router);
    if std::env::var_os("DEBUG_MESH").is_some() {
        let mesh = NegotiatedMeshRouter.route(&rp);
        print_result("mesh", &rp, &mesh);
        let auto = route_auto(&rp);
        print_result("auto", &rp, &auto);
    }
    let hard: std::collections::BTreeSet<String> = [
        "GPIO49", "ADC0", "SWCLK", "GPIO0", "XTAL_OUT", "USB_DM", "USB_DP",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    let strict_costs = AStarCosts {
        via_clear_radius_cells: grid_astar::router::via_clear_radius_cells(&rp),
        diag: DIAG_COST,
        ..AStarCosts::default()
    };
    print_result(
        "hard_first",
        &rp,
        &grid_astar::router::route_with(&rp, strict_costs, &hard),
    );
    for net in ["GPIO49", "ADC0", "SWCLK", "GPIO0", "XTAL_OUT"] {
        let one = [net.to_string()].into_iter().collect();
        let result = grid_astar::router::route_with(&rp, strict_costs, &one);
        print_result(&format!("first_{net}"), &rp, &result);
        let mut honest = result;
        let failed = failed_connections(&honest);
        add_terminal_stubs(&rp, &mut honest.solution, &failed);
        make_route_honest(&rp, &mut honest);
        print_result(&format!("first_{net}_honest"), &rp, &honest);
    }
    Ok(())
}

fn print_result(name: &str, rp: &RouteProblem, result: &RouteResult) {
    let counts = count_lint(rp, &result.solution);
    let m = result.solution.metrics();
    println!(
        "{name}: {} traces {} vias {:.1}mm {} failed engine={} lint={:?}",
        result.solution.traces.len(),
        result.solution.vias.len(),
        m.wirelength,
        result.failed.len(),
        result.engine,
        counts
    );
    for f in result.failed.iter().take(20) {
        println!("  failed {}: {}", f.connection, f.reason);
    }
}

fn audit_postprocess(rp: &RouteProblem, mut result: RouteResult) {
    println!("postprocess:");
    print_stage("raw", rp, &result);
    let dropped_failed = drop_failed_net_copper(&mut result);
    println!("  dropped_failed_net_copper: {:?}", dropped_failed);
    print_stage("after_drop_failed", rp, &result);
    let failed = failed_connections(&result);
    add_terminal_stubs(rp, &mut result.solution, &failed);
    print_stage("after_terminal_stubs", rp, &result);
    let original_solution = result.solution.clone();
    let pruned = prune_dangling_spurs(rp, &mut result.solution);
    println!("  pruned_dangling_spurs: {pruned}");
    if pruned > 0 && lint_real_count(rp, &result.solution, &result.failed) > 0 {
        result.solution = original_solution;
        println!("  pruning rolled back: introduced real lint/connectivity issues");
    }
    print_stage("after_prune", rp, &result);
    let dropped = make_route_honest(rp, &mut result);
    println!("  dropped_violating_or_unconnected: {:?}", dropped);
    print_stage("after_honest", rp, &result);
}

fn print_stage(name: &str, rp: &RouteProblem, result: &RouteResult) {
    let failed: BTreeSet<_> = result
        .failed
        .iter()
        .map(|f| f.connection.as_str())
        .collect();
    println!(
        "  {name}: {} traces {} vias failed={:?} lint={:?}",
        result.solution.traces.len(),
        result.solution.vias.len(),
        failed,
        count_lint(rp, &result.solution)
    );
    for violation in drc_lint::lint::lint(rp, &result.solution)
        .into_iter()
        .take(12)
    {
        println!("    lint {:?}", violation);
    }
}

fn lint_real_count(rp: &RouteProblem, solution: &RouteSolution, failed: &[FailedNet]) -> usize {
    let failed: BTreeSet<_> = failed
        .iter()
        .map(|f| f.connection.as_str())
        .filter(|name| !name.is_empty())
        .collect();
    drc_lint::lint::lint(rp, solution)
        .into_iter()
        .filter(|violation| match violation {
            drc_lint::lint::DrcViolation::Connectivity {
                violation: drc_lint::connectivity::Violation::Unconnected { connection, .. },
            } => !failed.contains(connection.as_str()),
            drc_lint::lint::DrcViolation::Connectivity {
                violation: drc_lint::connectivity::Violation::CrossNetMerge { .. },
            } => true,
            _ => true,
        })
        .count()
}

fn failed_connections(result: &RouteResult) -> BTreeSet<String> {
    result
        .failed
        .iter()
        .map(|f| f.connection.clone())
        .filter(|name| !name.is_empty())
        .collect()
}

fn drop_failed_net_copper(result: &mut RouteResult) -> Vec<String> {
    let failed = failed_connections(result);
    if failed.is_empty() {
        return Vec::new();
    }
    result
        .solution
        .traces
        .retain(|trace| !failed.contains(&trace.connection));
    result
        .solution
        .vias
        .retain(|via| !failed.contains(&via.connection));
    failed.into_iter().collect()
}

fn make_route_honest(rp: &RouteProblem, result: &mut RouteResult) -> Vec<String> {
    let mut dropped = Vec::new();
    for net in drc_lint::lint::drop_unconnected_copper(rp, &mut result.solution) {
        append_failed(
            result,
            &net,
            "dropped copper: connectivity oracle reported it unconnected",
        );
        dropped.push(net);
    }
    let violating: BTreeSet<String> = drc_lint::lint::lint(rp, &result.solution)
        .iter()
        .flat_map(geometry_violation_nets)
        .collect();
    if !violating.is_empty() {
        result
            .solution
            .traces
            .retain(|trace| !violating.contains(&trace.connection));
        result
            .solution
            .vias
            .retain(|via| !violating.contains(&via.connection));
        for net in violating {
            append_failed(
                result,
                &net,
                "dropped copper: route had geometry DRC violations",
            );
            dropped.push(net);
        }
    }
    for net in drc_lint::lint::drop_unconnected_copper(rp, &mut result.solution) {
        append_failed(
            result,
            &net,
            "dropped copper: connectivity oracle reported it unconnected after geometry cleanup",
        );
        dropped.push(net);
    }
    dropped.sort();
    dropped.dedup();
    dropped
}

fn geometry_violation_nets(v: &drc_lint::lint::DrcViolation) -> Vec<String> {
    match v {
        drc_lint::lint::DrcViolation::ClearanceTraceTrace { a, b, .. } => {
            vec![a.clone(), b.clone()]
        }
        drc_lint::lint::DrcViolation::ClearanceTraceObstacle { connection, .. }
        | drc_lint::lint::DrcViolation::ClearanceViaAny { connection, .. }
        | drc_lint::lint::DrcViolation::TraceWidthBelowMin { connection, .. }
        | drc_lint::lint::DrcViolation::OutOfBounds { connection, .. }
        | drc_lint::lint::DrcViolation::ViaDiameterBelowMin { connection, .. }
        | drc_lint::lint::DrcViolation::InvalidLayer { connection, .. } => vec![connection.clone()],
        drc_lint::lint::DrcViolation::Connectivity { .. } => Vec::new(),
    }
}

fn append_failed(result: &mut RouteResult, connection: &str, reason: &str) {
    if connection.is_empty()
        || result
            .failed
            .iter()
            .any(|f| f.connection == connection && f.reason == reason)
    {
        return;
    }
    result.failed.push(FailedNet {
        connection: connection.to_string(),
        reason: reason.to_string(),
    });
}

fn add_terminal_stubs(
    rp: &RouteProblem,
    solution: &mut RouteSolution,
    skip_connections: &BTreeSet<String>,
) {
    let pitch = grid_astar::grid::grid_pitch(rp);
    for conn in &rp.connections {
        if skip_connections.contains(&conn.name) {
            continue;
        }
        let width = rp.net_width(&conn.name);
        for point in &conn.points_to_connect {
            let cell_x = cell_center(rp.bounds.min_x, point.x, pitch);
            let cell_y = cell_center(rp.bounds.min_y, point.y, pitch);
            let exact = Point2 {
                x: point.x,
                y: point.y,
            };
            let center = Point2 {
                x: cell_x,
                y: cell_y,
            };
            if (exact.x - center.x).abs() < geom::EPS && (exact.y - center.y).abs() < geom::EPS {
                continue;
            }
            solution.traces.push(Trace {
                connection: conn.name.clone(),
                layer: point.layer.clone(),
                width,
                path: vec![exact, center],
            });
        }
    }
}

fn cell_center(min: f64, value: f64, pitch: f64) -> f64 {
    let idx = ((value - min) / pitch).floor().max(0.0);
    min + (idx + 0.5) * pitch
}

#[derive(Clone, Debug)]
struct RouteSegment {
    connection: String,
    layer: LayerRef,
    width: f64,
    start: Point2,
    end: Point2,
}

fn prune_dangling_spurs(rp: &RouteProblem, solution: &mut RouteSolution) -> usize {
    let mut segments = flatten_segments(solution);
    if segments.is_empty() {
        return 0;
    }
    let protected = protected_route_nodes(rp, solution);
    let mut alive = vec![true; segments.len()];
    let mut removed = 0usize;

    loop {
        let mut degree: BTreeMap<(String, u32, i64, i64), usize> = BTreeMap::new();
        for (idx, segment) in segments.iter().enumerate() {
            if !alive[idx] {
                continue;
            }
            for key in [
                node_key(
                    &segment.connection,
                    &segment.layer,
                    segment.start,
                    rp.layer_count,
                ),
                node_key(
                    &segment.connection,
                    &segment.layer,
                    segment.end,
                    rp.layer_count,
                ),
            ] {
                *degree.entry(key).or_default() += 1;
            }
        }

        let mut changed = false;
        for (idx, segment) in segments.iter().enumerate() {
            if !alive[idx] {
                continue;
            }
            let a = node_key(
                &segment.connection,
                &segment.layer,
                segment.start,
                rp.layer_count,
            );
            let b = node_key(
                &segment.connection,
                &segment.layer,
                segment.end,
                rp.layer_count,
            );
            let a_dangles = degree.get(&a).copied().unwrap_or(0) <= 1 && !protected.contains(&a);
            let b_dangles = degree.get(&b).copied().unwrap_or(0) <= 1 && !protected.contains(&b);
            if a_dangles || b_dangles {
                alive[idx] = false;
                removed += 1;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    if removed > 0 {
        solution.traces = segments
            .drain(..)
            .zip(alive)
            .filter_map(|(segment, alive)| {
                alive.then_some(Trace {
                    connection: segment.connection,
                    layer: segment.layer,
                    width: segment.width,
                    path: vec![segment.start, segment.end],
                })
            })
            .collect();
    }
    removed
}

fn flatten_segments(solution: &RouteSolution) -> Vec<RouteSegment> {
    let mut segments = Vec::new();
    for trace in &solution.traces {
        for pair in trace.path.windows(2) {
            segments.push(RouteSegment {
                connection: trace.connection.clone(),
                layer: trace.layer.clone(),
                width: trace.width,
                start: pair[0],
                end: pair[1],
            });
        }
    }
    segments
}

fn protected_route_nodes(
    rp: &RouteProblem,
    solution: &RouteSolution,
) -> BTreeSet<(String, u32, i64, i64)> {
    let mut protected = BTreeSet::new();
    for conn in &rp.connections {
        for point in &conn.points_to_connect {
            protected.insert(node_key(
                &conn.name,
                &point.layer,
                point.point(),
                rp.layer_count,
            ));
        }
    }
    for via in &solution.vias {
        for layer in via_span_indices(&via.span, rp.layer_count) {
            protected.insert((
                via.connection.clone(),
                layer,
                quantize_mm(via.at.x),
                quantize_mm(via.at.y),
            ));
        }
    }
    protected
}

fn via_span_indices(span: &ViaSpan, layer_count: u32) -> Vec<u32> {
    match *span {
        ViaSpan::Through => (0..layer_count.max(1)).collect(),
        ViaSpan::Partial { from, to, .. } => {
            let (lo, hi) = if from <= to { (from, to) } else { (to, from) };
            (lo..=hi).filter(|idx| *idx < layer_count).collect()
        }
    }
}

fn node_key(
    connection: &str,
    layer: &LayerRef,
    point: Point2,
    layer_count: u32,
) -> (String, u32, i64, i64) {
    (
        connection.to_string(),
        layer.index(layer_count).unwrap_or(0),
        quantize_mm(point.x),
        quantize_mm(point.y),
    )
}

fn quantize_mm(v: f64) -> i64 {
    (v * 1_000_000.0).round() as i64
}

fn count_lint(problem: &RouteProblem, solution: &RouteSolution) -> BTreeMap<&'static str, usize> {
    let mut out = std::collections::BTreeMap::new();
    for v in drc_lint::lint::lint(problem, solution) {
        let key = match v {
            drc_lint::lint::DrcViolation::ClearanceTraceTrace { .. } => "clearanceTraceTrace",
            drc_lint::lint::DrcViolation::ClearanceTraceObstacle { .. } => "clearanceTraceObstacle",
            drc_lint::lint::DrcViolation::ClearanceViaAny { .. } => "clearanceViaAny",
            drc_lint::lint::DrcViolation::TraceWidthBelowMin { .. } => "traceWidth",
            drc_lint::lint::DrcViolation::OutOfBounds { .. } => "outOfBounds",
            drc_lint::lint::DrcViolation::ViaDiameterBelowMin { .. } => "viaDiameter",
            drc_lint::lint::DrcViolation::InvalidLayer { .. } => "invalidLayer",
            drc_lint::lint::DrcViolation::Connectivity { .. } => "connectivity",
        };
        *out.entry(key).or_default() += 1;
    }
    out
}
