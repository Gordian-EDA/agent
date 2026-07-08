//! Simple via-escape router for small boards blocked on the pad layer.
//!
//! [`DirectLineRouter`](crate::direct::DirectLineRouter) deliberately refuses vias,
//! while the negotiated mesh and grid routers are heavier machinery. This candidate
//! fills the narrow gap between them: same-layer two-terminal or small star nets may
//! be routed as straight/dogleg traces on any non-plane signal layer, adding
//! through-vias at the terminals when the chosen layer differs from the pads' layer.

use crate::heuristics::{
    connection_crossing_pressures, connection_segment_obstacle_pressure_um, connection_span_um,
    connection_tree_segments,
};
use crate::problem::{
    Capabilities, Connection, FailedNet, LayerRef, Point2, RouteProblem, RouteQuality, RouteResult,
    RouteSolution, Router, Trace, Via, ViaSpan,
};
use crate::quality::{
    keep_route_candidate as keep_candidate, route_quality, trace_proximity_penalty_um,
    trace_route_cost_um,
};
use std::collections::BTreeSet;

/// This engine's [`RouteResult::engine`] provenance tag.
pub const ENGINE: &str = "via-escape";
const VIA_ESCAPE_MAX_MULTILAYER_CONNECTIONS: usize = 4;
type ViaEscapeSolutionKey = (usize, u64, u64, u32, usize);
type ViaEscapeLegKey = (u64, u64, u32, usize, usize);
type ViaEscapeBestLeg = (usize, Trace, ViaEscapeLegKey);

/// A narrow, deterministic router for same-layer point/star nets needing one
/// alternate signal layer.
#[derive(Debug, Clone, Copy, Default)]
pub struct ViaEscapeRouter;

impl Router for ViaEscapeRouter {
    fn name(&self) -> &'static str {
        ENGINE
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            max_layers: u32::MAX,
            honors_escape_layers: false,
            honors_net_widths: true,
            honors_outline: true,
        }
    }

    fn can_route(&self, problem: &RouteProblem) -> bool {
        self.capabilities().can_route(problem)
            && (problem.layer_count <= 2
                || problem.connections.len() <= VIA_ESCAPE_MAX_MULTILAYER_CONNECTIONS)
    }

    fn route(&self, problem: &RouteProblem) -> RouteResult {
        route_via_escape(problem)
    }
}

/// Route eligible same-layer nets, using endpoint through-vias when an
/// alternate signal layer is cleaner than the pad layer.
thread_local! {
    /// Remaining full-solution geometry checks for the CURRENT route_via_escape
    /// call — the same deterministic count-based runaway guard as direct's
    /// (each check clones the solution and runs the whole lint).
    static GEOMETRY_CHECKS_LEFT: std::cell::Cell<usize> =
        const { std::cell::Cell::new(VIA_ESCAPE_GEOMETRY_CHECK_BUDGET) };
}

const VIA_ESCAPE_GEOMETRY_CHECK_BUDGET: usize = 30_000;

pub fn route_via_escape(problem: &RouteProblem) -> RouteResult {
    GEOMETRY_CHECKS_LEFT.with(|b| b.set(VIA_ESCAPE_GEOMETRY_CHECK_BUDGET));
    let mut best: Option<(RouteResult, RouteQuality)> = None;
    for order in net_order_portfolio(problem) {
        let result = route_via_escape_order(problem, &order);
        let q = route_quality(problem, &result);
        best = match best.take() {
            None => Some((result, q)),
            Some((bi, bq)) if keep_candidate(&bq, &q) => Some((bi, bq)),
            Some(_) => Some((result, q)),
        };
    }
    best.map(|(r, _)| r)
        .unwrap_or_else(|| route_via_escape_order(problem, &[]))
}

fn route_via_escape_order(problem: &RouteProblem, order: &[usize]) -> RouteResult {
    let mut best = route_via_escape_order_once(problem, order);
    let mut current_order = order.to_vec();
    let mut tried = vec![current_order.clone()];

    for _ in 0..2 {
        if best.failed.is_empty() {
            break;
        }

        let retry_order = failed_priority_order(problem, &current_order, &best.failed);
        if tried.iter().any(|existing| existing == &retry_order) {
            break;
        }
        tried.push(retry_order.clone());

        let candidate = route_via_escape_order_once(problem, &retry_order);
        let incumbent_quality = route_quality(problem, &best);
        let candidate_quality = route_quality(problem, &candidate);
        if keep_candidate(&incumbent_quality, &candidate_quality) {
            break;
        }
        best = candidate;
        current_order = retry_order;
    }

    best
}

fn route_via_escape_order_once(problem: &RouteProblem, order: &[usize]) -> RouteResult {
    let mut solution = RouteSolution {
        traces: Vec::new(),
        vias: Vec::new(),
    };
    let mut failed = Vec::new();

    for &idx in order {
        let Some(conn) = problem.connections.get(idx) else {
            continue;
        };
        match conn.points_to_connect.as_slice() {
            [] | [_] => {}
            points if same_layer(points) => {
                if let Some(next) = route_same_layer_tree(problem, &solution, conn) {
                    solution = next;
                } else {
                    failed.push(FailedNet {
                        connection: conn.name.clone(),
                        reason: "via-escape found no clean same-layer/two-via tree".to_string(),
                    });
                }
            }
            _ => failed.push(FailedNet {
                connection: conn.name.clone(),
                reason: "via-escape only handles same-layer nets".to_string(),
            }),
        }
    }

    reconcile(problem, &mut solution, &mut failed);
    RouteResult {
        solution,
        failed,
        engine: ENGINE.to_owned(),
    }
}

fn failed_priority_order(
    problem: &RouteProblem,
    order: &[usize],
    failed: &[FailedNet],
) -> Vec<usize> {
    let failed_names: BTreeSet<&str> = failed.iter().map(|f| f.connection.as_str()).collect();
    if failed_names.is_empty() {
        return order.to_vec();
    }

    let mut promoted = Vec::new();
    let mut rest = Vec::new();
    for &idx in order {
        let Some(conn) = problem.connections.get(idx) else {
            continue;
        };
        if failed_names.contains(conn.name.as_str()) {
            promoted.push(idx);
        } else {
            rest.push(idx);
        }
    }

    let metrics = net_order_metrics(problem);
    promoted.sort_by(|&a, &b| failed_priority_cmp_with_metrics(problem, &metrics, a, b));
    promoted.extend(rest);
    promoted
}

fn failed_priority_cmp_with_metrics(
    problem: &RouteProblem,
    metrics: &[ViaEscapeOrderMetric],
    a: usize,
    b: usize,
) -> std::cmp::Ordering {
    metrics[b]
        .constraint_score
        .cmp(&metrics[a].constraint_score)
        .then_with(|| {
            metrics[b]
                .segment_obstacle_pressure_um
                .cmp(&metrics[a].segment_obstacle_pressure_um)
        })
        .then_with(|| {
            metrics[b]
                .crossing_pressure
                .cmp(&metrics[a].crossing_pressure)
        })
        .then_with(|| metrics[b].pin_count.cmp(&metrics[a].pin_count))
        .then_with(|| metrics[b].span_um.cmp(&metrics[a].span_um))
        .then_with(|| {
            problem.connections[a]
                .name
                .cmp(&problem.connections[b].name)
        })
}

fn net_order_portfolio(problem: &RouteProblem) -> Vec<Vec<usize>> {
    let base: Vec<usize> = (0..problem.connections.len()).collect();
    let metrics = net_order_metrics(problem);
    let mut orders = Vec::new();
    push_order(&mut orders, base.clone());

    let mut constrained = base.clone();
    constrained.sort_by(|&a, &b| {
        metrics[b]
            .constraint_score
            .cmp(&metrics[a].constraint_score)
            .then_with(|| metrics[b].pin_count.cmp(&metrics[a].pin_count))
            .then_with(|| {
                problem.connections[a]
                    .name
                    .cmp(&problem.connections[b].name)
            })
    });
    push_order(&mut orders, constrained);

    let mut segment_crowded_first = base.clone();
    segment_crowded_first.sort_by(|&a, &b| {
        metrics[b]
            .segment_obstacle_pressure_um
            .cmp(&metrics[a].segment_obstacle_pressure_um)
            .then_with(|| {
                metrics[b]
                    .constraint_score
                    .cmp(&metrics[a].constraint_score)
            })
            .then_with(|| metrics[b].pin_count.cmp(&metrics[a].pin_count))
            .then_with(|| metrics[b].span_um.cmp(&metrics[a].span_um))
            .then_with(|| {
                problem.connections[a]
                    .name
                    .cmp(&problem.connections[b].name)
            })
    });
    push_order(&mut orders, segment_crowded_first);

    let mut crossing_first = base.clone();
    crossing_first.sort_by(|&a, &b| {
        metrics[b]
            .crossing_pressure
            .cmp(&metrics[a].crossing_pressure)
            .then_with(|| {
                metrics[b]
                    .segment_obstacle_pressure_um
                    .cmp(&metrics[a].segment_obstacle_pressure_um)
            })
            .then_with(|| {
                metrics[b]
                    .constraint_score
                    .cmp(&metrics[a].constraint_score)
            })
            .then_with(|| metrics[b].pin_count.cmp(&metrics[a].pin_count))
            .then_with(|| metrics[b].span_um.cmp(&metrics[a].span_um))
            .then_with(|| {
                problem.connections[a]
                    .name
                    .cmp(&problem.connections[b].name)
            })
    });
    push_order(&mut orders, crossing_first);

    let mut short_first = base.clone();
    short_first.sort_by(|&a, &b| {
        metrics[a].span_um.cmp(&metrics[b].span_um).then_with(|| {
            problem.connections[a]
                .name
                .cmp(&problem.connections[b].name)
        })
    });
    push_order(&mut orders, short_first);

    let mut long_first = base;
    long_first.sort_by(|&a, &b| {
        metrics[b].span_um.cmp(&metrics[a].span_um).then_with(|| {
            problem.connections[a]
                .name
                .cmp(&problem.connections[b].name)
        })
    });
    push_order(&mut orders, long_first);
    orders
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ViaEscapeOrderMetric {
    constraint_score: usize,
    pin_count: usize,
    span_um: u64,
    segment_obstacle_pressure_um: u64,
    crossing_pressure: usize,
}

fn net_order_metrics(problem: &RouteProblem) -> Vec<ViaEscapeOrderMetric> {
    let crossing_pressures = connection_crossing_pressures(problem);
    problem
        .connections
        .iter()
        .enumerate()
        .map(|(idx, conn)| ViaEscapeOrderMetric {
            constraint_score: connection_constraint_score(problem, conn),
            pin_count: conn.points_to_connect.len(),
            span_um: connection_span_um(conn),
            segment_obstacle_pressure_um: connection_segment_obstacle_pressure_um(problem, conn),
            crossing_pressure: crossing_pressures[idx],
        })
        .collect()
}

fn push_order(orders: &mut Vec<Vec<usize>>, order: Vec<usize>) {
    if !orders.iter().any(|existing| existing == &order) {
        orders.push(order);
    }
}

fn connection_constraint_score(problem: &RouteProblem, conn: &Connection) -> usize {
    if !same_layer(&conn.points_to_connect) || conn.points_to_connect.len() < 2 {
        return 0;
    }
    connection_tree_segments(conn)
        .into_iter()
        .map(|(a, b)| segment_pressure(problem, a, b))
        .sum()
}

fn segment_pressure(problem: &RouteProblem, a: Point2, b: Point2) -> usize {
    let min_x = a.x.min(b.x);
    let max_x = a.x.max(b.x);
    let min_y = a.y.min(b.y);
    let max_y = a.y.max(b.y);
    let clearance = problem.clearance + problem.min_trace_width;
    problem
        .obstacles
        .iter()
        .filter(|ob| {
            ob.connected_to.is_empty()
                && ob.center.x + ob.width / 2.0 + clearance >= min_x
                && ob.center.x - ob.width / 2.0 - clearance <= max_x
                && ob.center.y + ob.height / 2.0 + clearance >= min_y
                && ob.center.y - ob.height / 2.0 - clearance <= max_y
        })
        .count()
}

fn route_two_point(
    problem: &RouteProblem,
    solution: &RouteSolution,
    conn: &crate::problem::Connection,
) -> Option<RouteSolution> {
    let a = conn.points_to_connect[0].point();
    let b = conn.points_to_connect[1].point();
    let terminal_layer = conn.points_to_connect[0].layer.index(problem.layer_count)?;
    let mut candidates = Vec::new();
    for layer in candidate_layers(problem, terminal_layer) {
        for path in candidate_paths(problem, solution, &conn.name, &layer, a, b) {
            let candidate =
                two_point_candidate(problem, solution, conn, &layer, terminal_layer, a, b, path);
            if !candidate_is_geometry_clean(problem, &candidate, &conn.name) {
                continue;
            }
            candidates.push(candidate);
        }
    }
    best_solution_candidate(problem, solution, candidates)
}

#[allow(clippy::too_many_arguments)]
fn two_point_candidate(
    problem: &RouteProblem,
    solution: &RouteSolution,
    conn: &crate::problem::Connection,
    layer: &LayerRef,
    terminal_layer: u32,
    a: Point2,
    b: Point2,
    path: Vec<Point2>,
) -> RouteSolution {
    let mut candidate = solution.clone();
    if layer.index(problem.layer_count) != Some(terminal_layer) {
        candidate.vias.push(Via {
            connection: conn.name.clone(),
            at: a,
            diameter: problem.via_diameter,
            drill: problem.via_drill,
            span: ViaSpan::Through,
        });
        candidate.vias.push(Via {
            connection: conn.name.clone(),
            at: b,
            diameter: problem.via_diameter,
            drill: problem.via_drill,
            span: ViaSpan::Through,
        });
    }
    candidate.traces.push(Trace {
        connection: conn.name.clone(),
        layer: layer.clone(),
        width: problem.net_width(&conn.name),
        path,
    });
    simplify_candidate_paths(&mut candidate);
    candidate
}

fn best_solution_candidate(
    problem: &RouteProblem,
    before: &RouteSolution,
    candidates: Vec<RouteSolution>,
) -> Option<RouteSolution> {
    candidates
        .into_iter()
        .map(|candidate| {
            let key = solution_tree_key(problem, before, &candidate);
            (candidate, key)
        })
        .min_by(|(_, a), (_, b)| a.cmp(b))
        .map(|(candidate, _)| candidate)
}

fn route_same_layer_tree(
    problem: &RouteProblem,
    solution: &RouteSolution,
    conn: &crate::problem::Connection,
) -> Option<RouteSolution> {
    match conn.points_to_connect.as_slice() {
        [_, _] => route_two_point(problem, solution, conn),
        [root, ..] => {
            let terminal_layer = root.layer.index(problem.layer_count)?;
            let mut best: Option<(RouteSolution, ViaEscapeSolutionKey)> = None;
            for layer in candidate_layers(problem, terminal_layer) {
                let mut seed = solution.clone();
                if layer.index(problem.layer_count) != Some(terminal_layer) {
                    for pt in &conn.points_to_connect {
                        push_via_once(problem, &mut seed, &conn.name, pt.point());
                    }
                }
                let Some(candidate) = route_tree_on_layer(problem, &seed, conn, &layer) else {
                    continue;
                };
                let key = solution_tree_key(problem, solution, &candidate);
                best = match best.take() {
                    None => Some((candidate, key)),
                    Some((incumbent, incumbent_key)) if incumbent_key <= key => {
                        Some((incumbent, incumbent_key))
                    }
                    Some(_) => Some((candidate, key)),
                };
            }
            best.map(|(solution, _)| solution)
        }
        _ => None,
    }
}

fn route_tree_on_layer(
    problem: &RouteProblem,
    solution: &RouteSolution,
    conn: &crate::problem::Connection,
    layer: &LayerRef,
) -> Option<RouteSolution> {
    let mut best: Option<(RouteSolution, ViaEscapeSolutionKey)> = None;
    for root in 0..conn.points_to_connect.len() {
        let Some(candidate) = route_tree_on_layer_from_root(problem, solution, conn, layer, root)
        else {
            continue;
        };
        let key = solution_tree_key(problem, solution, &candidate);
        best = match best.take() {
            None => Some((candidate, key)),
            Some((incumbent, incumbent_key)) if incumbent_key <= key => {
                Some((incumbent, incumbent_key))
            }
            Some(_) => Some((candidate, key)),
        };
    }
    best.map(|(solution, _)| solution)
}

fn route_tree_on_layer_from_root(
    problem: &RouteProblem,
    solution: &RouteSolution,
    conn: &crate::problem::Connection,
    layer: &LayerRef,
    root: usize,
) -> Option<RouteSolution> {
    let mut candidate = solution.clone();
    let mut connected = vec![root];
    let mut remaining: BTreeSet<usize> = (0..conn.points_to_connect.len())
        .filter(|idx| *idx != root)
        .collect();

    while !remaining.is_empty() {
        let mut best_leg: Option<ViaEscapeBestLeg> = None;
        for &from in &connected {
            for &to in &remaining {
                let Some(trace) = route_leg_on_layer(
                    problem,
                    &candidate,
                    conn,
                    layer,
                    conn.points_to_connect[from].point(),
                    conn.points_to_connect[to].point(),
                    from,
                    to,
                ) else {
                    continue;
                };
                let key = trace_tree_key(problem, &candidate, &trace, from, to);
                best_leg = match best_leg.take() {
                    None => Some((to, trace, key)),
                    Some((best_to, best_trace, best_key)) if best_key <= key => {
                        Some((best_to, best_trace, best_key))
                    }
                    Some(_) => Some((to, trace, key)),
                };
            }
        }
        let (to, trace, _) = best_leg?;
        candidate.traces.push(trace);
        remaining.remove(&to);
        connected.push(to);
    }

    Some(candidate)
}

#[allow(clippy::too_many_arguments)]
fn route_leg_on_layer(
    problem: &RouteProblem,
    solution: &RouteSolution,
    conn: &crate::problem::Connection,
    layer: &LayerRef,
    a: Point2,
    b: Point2,
    from: usize,
    to: usize,
) -> Option<Trace> {
    if a.dist(b) < geom::EPS {
        return None;
    }
    let mut best: Option<(Trace, ViaEscapeLegKey)> = None;
    for path in candidate_paths(problem, solution, &conn.name, layer, a, b) {
        let trace = Trace {
            connection: conn.name.clone(),
            layer: layer.clone(),
            width: problem.net_width(&conn.name),
            path,
        };
        let mut candidate = solution.clone();
        candidate.traces.push(trace.clone());
        simplify_candidate_paths(&mut candidate);
        if !candidate_is_geometry_clean(problem, &candidate, &conn.name) {
            continue;
        }
        let key = trace_tree_key(problem, solution, &trace, from, to);
        best = match best.take() {
            None => Some((trace, key)),
            Some((incumbent, incumbent_key)) if incumbent_key <= key => {
                Some((incumbent, incumbent_key))
            }
            Some(_) => Some((trace, key)),
        };
    }
    best.map(|(trace, _)| trace)
}

fn solution_tree_key(
    problem: &RouteProblem,
    before: &RouteSolution,
    after: &RouteSolution,
) -> (usize, u64, u64, u32, usize) {
    let added = &after.traces[before.traces.len()..];
    let added_vias = after.vias.len().saturating_sub(before.vias.len());
    let length_um: u64 = added.iter().map(trace_length_um).sum();
    let proximity_um = added
        .iter()
        .map(|trace| trace_proximity_penalty_um(problem, before, trace))
        .sum::<u64>();
    let route_cost_um = length_um.saturating_add(proximity_um);
    let bends = added.iter().map(trace_bends).sum();
    (added_vias, route_cost_um, length_um, bends, added.len())
}

fn trace_tree_key(
    problem: &RouteProblem,
    solution: &RouteSolution,
    trace: &Trace,
    from: usize,
    to: usize,
) -> (u64, u64, u32, usize, usize) {
    let length_um = trace_length_um(trace);
    (
        trace_route_cost_um(problem, solution, trace, length_um),
        length_um,
        trace_bends(trace),
        to,
        from,
    )
}

fn trace_length_um(trace: &Trace) -> u64 {
    (trace.path.windows(2).map(|w| w[1].dist(w[0])).sum::<f64>() * 1000.0).round() as u64
}

fn trace_bends(trace: &Trace) -> u32 {
    trace
        .path
        .windows(3)
        .filter(|w| {
            let ab_horizontal = (w[0].y - w[1].y).abs() < geom::EPS;
            let bc_horizontal = (w[1].y - w[2].y).abs() < geom::EPS;
            let ab_vertical = (w[0].x - w[1].x).abs() < geom::EPS;
            let bc_vertical = (w[1].x - w[2].x).abs() < geom::EPS;
            (ab_horizontal && bc_vertical) || (ab_vertical && bc_horizontal)
        })
        .count() as u32
}

fn push_via_once(
    problem: &RouteProblem,
    solution: &mut RouteSolution,
    connection: &str,
    at: Point2,
) {
    if solution
        .vias
        .iter()
        .any(|v| v.connection == connection && v.at.dist(at) < geom::EPS)
    {
        return;
    }
    solution.vias.push(Via {
        connection: connection.to_owned(),
        at,
        diameter: problem.via_diameter,
        drill: problem.via_drill,
        span: ViaSpan::Through,
    });
}

fn candidate_layers(problem: &RouteProblem, terminal_layer: u32) -> Vec<LayerRef> {
    let layer_count = problem.layer_count.max(1);
    let plane_layers: BTreeSet<u32> = crate::router::plane_layers(layer_count as usize)
        .into_iter()
        .collect();
    let mut layers = Vec::new();
    if !plane_layers.contains(&terminal_layer) {
        layers.push(layer_ref(terminal_layer, layer_count));
    }
    for idx in 0..layer_count {
        if idx == terminal_layer || plane_layers.contains(&idx) {
            continue;
        }
        layers.push(layer_ref(idx, layer_count));
    }
    layers
}

fn candidate_paths(
    problem: &RouteProblem,
    solution: &RouteSolution,
    connection: &str,
    layer: &LayerRef,
    a: Point2,
    b: Point2,
) -> Vec<Vec<Point2>> {
    let mut paths = Vec::new();
    let mut seen = BTreeSet::new();
    push_candidate_path(&mut paths, &mut seen, vec![a, b]);
    push_candidate_path(&mut paths, &mut seen, vec![a, Point2 { x: a.x, y: b.y }, b]);
    push_candidate_path(&mut paths, &mut seen, vec![a, Point2 { x: b.x, y: a.y }, b]);

    let route_radius = problem.net_width(connection).max(problem.min_trace_width) / 2.0;
    let inset = (problem.clearance + problem.min_trace_width + 0.5).max(1.0);
    let mut xs = vec![a.x, b.x, (a.x + b.x) / 2.0];
    let mut ys = vec![a.y, b.y, (a.y + b.y) / 2.0];
    xs.extend([problem.bounds.min_x + inset, problem.bounds.max_x - inset]);
    ys.extend([problem.bounds.min_y + inset, problem.bounds.max_y - inset]);
    for obstacle in &problem.obstacles {
        if !obstacle.layers.iter().any(|l| l == layer) {
            continue;
        }
        let dx = obstacle.width / 2.0 + problem.clearance + problem.min_trace_width;
        let dy = obstacle.height / 2.0 + problem.clearance + problem.min_trace_width;
        xs.extend([obstacle.center.x - dx, obstacle.center.x + dx]);
        ys.extend([obstacle.center.y - dy, obstacle.center.y + dy]);
    }
    for trace in &solution.traces {
        if trace.connection == connection || &trace.layer != layer {
            continue;
        }
        let Some(first) = trace.path.first() else {
            continue;
        };
        let (mut min_x, mut max_x, mut min_y, mut max_y) = (first.x, first.x, first.y, first.y);
        for point in &trace.path {
            min_x = min_x.min(point.x);
            max_x = max_x.max(point.x);
            min_y = min_y.min(point.y);
            max_y = max_y.max(point.y);
        }
        let d = trace.width / 2.0 + problem.clearance + route_radius;
        xs.extend([min_x - d, max_x + d]);
        ys.extend([min_y - d, max_y + d]);
    }
    for via in &solution.vias {
        if via.connection == connection {
            continue;
        }
        let d = problem.via_diameter / 2.0 + problem.clearance + route_radius;
        xs.extend([via.at.x - d, via.at.x + d]);
        ys.extend([via.at.y - d, via.at.y + d]);
    }
    xs.retain(|x| *x >= problem.bounds.min_x + inset && *x <= problem.bounds.max_x - inset);
    ys.retain(|y| *y >= problem.bounds.min_y + inset && *y <= problem.bounds.max_y - inset);
    xs.sort_by(|a, b| a.total_cmp(b));
    ys.sort_by(|a, b| a.total_cmp(b));
    xs.dedup_by(|a, b| (*a - *b).abs() < 0.05);
    ys.dedup_by(|a, b| (*a - *b).abs() < 0.05);

    let mid_key = |v: f64, lo: f64, hi: f64| {
        let mid = (lo + hi) / 2.0;
        ((v - mid).abs() * 1000.0).round() as i64
    };
    xs.sort_by_key(|x| mid_key(*x, a.x.min(b.x), a.x.max(b.x)));
    ys.sort_by_key(|y| mid_key(*y, a.y.min(b.y), a.y.max(b.y)));

    for x in xs.into_iter().take(16) {
        push_candidate_path(
            &mut paths,
            &mut seen,
            vec![a, Point2 { x, y: a.y }, Point2 { x, y: b.y }, b],
        );
    }
    for y in ys.into_iter().take(16) {
        push_candidate_path(
            &mut paths,
            &mut seen,
            vec![a, Point2 { x: a.x, y }, Point2 { x: b.x, y }, b],
        );
    }
    paths
}

fn push_candidate_path(
    paths: &mut Vec<Vec<Point2>>,
    seen: &mut BTreeSet<Vec<(i64, i64)>>,
    path: Vec<Point2>,
) {
    let path = geom::Polyline::new(path).simplify().into_points();
    if path.len() < 2 {
        return;
    }
    let key: Vec<(i64, i64)> = path
        .iter()
        .map(|p| ((p.x * 1000.0).round() as i64, (p.y * 1000.0).round() as i64))
        .collect();
    if seen.insert(key) {
        paths.push(path);
    }
}

fn simplify_candidate_paths(solution: &mut RouteSolution) {
    for trace in &mut solution.traces {
        trace.path.dedup_by(|a, b| a.dist(*b) < geom::EPS);
    }
}

fn candidate_is_geometry_clean(
    problem: &RouteProblem,
    solution: &RouteSolution,
    connection: &str,
) -> bool {
    let exhausted = GEOMETRY_CHECKS_LEFT.with(|b| {
        let left = b.get();
        b.set(left.saturating_sub(1));
        left == 0
    });
    if exhausted {
        return false;
    }
    for violation in crate::lint::lint(problem, solution) {
        match violation {
            crate::lint::DrcViolation::Connectivity {
                violation: crate::connectivity::Violation::CrossNetMerge { ref a, ref b },
            } if a == connection || b == connection => return false,
            crate::lint::DrcViolation::Connectivity { .. } => {}
            _ => return false,
        }
    }
    true
}

fn reconcile(problem: &RouteProblem, solution: &mut RouteSolution, failed: &mut Vec<FailedNet>) {
    crate::via_cleanup::normalize_redundant_vias(problem, solution);
    let mut dropped = crate::lint::drop_violating_copper(problem, solution);
    dropped.extend(crate::lint::drop_unconnected_copper(problem, solution));

    let known: BTreeSet<String> = failed.iter().map(|f| f.connection.clone()).collect();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    failed.extend(
        dropped
            .into_iter()
            .filter(|name| !known.contains(name.as_str()) && seen.insert(name.clone()))
            .map(|connection| FailedNet {
                connection,
                reason: "DRC oracle: via-escape copper dropped (clearance/connectivity)"
                    .to_string(),
            }),
    );
}

fn layer_ref(layer: u32, layer_count: u32) -> LayerRef {
    if layer == 0 {
        LayerRef::top()
    } else if layer + 1 == layer_count {
        LayerRef::bottom()
    } else {
        LayerRef(format!("inner{layer}"))
    }
}

fn same_layer(points: &[crate::problem::RoutePoint]) -> bool {
    points
        .first()
        .is_some_and(|first| points.iter().all(|pt| pt.layer == first.layer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::problem::{Connection, Obstacle, Rect, RoutePoint, Trace};

    fn conn(name: &str, pts: &[(f64, f64, &str)]) -> Connection {
        Connection {
            name: name.to_owned(),
            points_to_connect: pts
                .iter()
                .map(|&(x, y, l)| RoutePoint {
                    x,
                    y,
                    layer: LayerRef(l.to_owned()),
                })
                .collect(),
        }
    }

    fn pad(connected_to: &[&str], center: (f64, f64)) -> Obstacle {
        Obstacle {
            kind: "rect".to_owned(),
            layers: vec![LayerRef::top()],
            center: Point2 {
                x: center.0,
                y: center.1,
            },
            width: 0.6,
            height: 0.6,
            connected_to: connected_to.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    fn wall(layers: Vec<LayerRef>) -> Obstacle {
        Obstacle {
            kind: "rect".to_owned(),
            layers,
            center: Point2 { x: 10.0, y: 10.0 },
            width: 1.0,
            height: 20.0,
            connected_to: vec![],
        }
    }

    fn keepout(at: (f64, f64), layer: LayerRef) -> Obstacle {
        Obstacle {
            kind: "rect".to_owned(),
            layers: vec![layer],
            center: Point2 { x: at.0, y: at.1 },
            width: 0.5,
            height: 0.5,
            connected_to: vec![],
        }
    }

    fn base(obstacles: Vec<Obstacle>) -> RouteProblem {
        RouteProblem {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles,
            connections: vec![conn("N", &[(2.0, 10.0, "top"), (18.0, 10.0, "top")])],
            bounds: Rect {
                min_x: 0.0,
                max_x: 20.0,
                min_y: 0.0,
                max_y: 20.0,
            },
            clearance: 0.2,
            via_diameter: 0.6,
            via_drill: 0.3,
            net_widths: Default::default(),
            outline: None,
            escape_layers: Default::default(),
        }
    }

    fn trace(path: &[(f64, f64)]) -> Trace {
        Trace {
            connection: "N".to_owned(),
            layer: LayerRef::top(),
            width: 0.2,
            path: path.iter().map(|&(x, y)| Point2 { x, y }).collect(),
        }
    }

    fn pt(x: f64, y: f64) -> Point2 {
        Point2 { x, y }
    }

    #[test]
    fn push_candidate_path_simplifies_before_dedupe() {
        let mut paths = Vec::new();
        let mut seen = BTreeSet::new();

        push_candidate_path(
            &mut paths,
            &mut seen,
            vec![pt(1.0, 1.0), pt(1.0, 1.0), pt(3.0, 1.0), pt(5.0, 1.0)],
        );
        push_candidate_path(&mut paths, &mut seen, vec![pt(1.0, 1.0), pt(5.0, 1.0)]);

        assert_eq!(paths, vec![vec![pt(1.0, 1.0), pt(5.0, 1.0)]]);
    }

    #[test]
    fn trace_key_prefers_roomier_detour_over_tight_straight() {
        let p = base(vec![keepout((10.0, 9.2), LayerRef::top())]);
        let solution = RouteSolution {
            traces: Vec::new(),
            vias: Vec::new(),
        };
        let tight = trace(&[(2.0, 10.0), (18.0, 10.0)]);
        let roomy = trace(&[(2.0, 10.0), (2.0, 11.0), (18.0, 11.0), (18.0, 10.0)]);

        let tight_key = trace_tree_key(&p, &solution, &tight, 0, 1);
        let roomy_key = trace_tree_key(&p, &solution, &roomy, 0, 1);

        assert!(
            roomy_key < tight_key,
            "via-escape should prefer a modest detour with real clearance over a legal but tight straight segment: tight={tight_key:?} roomy={roomy_key:?}"
        );
    }

    #[test]
    fn escapes_top_wall_on_bottom_with_two_vias() {
        let p = base(vec![
            pad(&["N"], (2.0, 10.0)),
            pad(&["N"], (18.0, 10.0)),
            wall(vec![LayerRef::top()]),
        ]);
        let direct = crate::direct::route_direct(&p);
        assert!(!direct.failed.is_empty(), "direct router must not add vias");

        let r = route_via_escape(&p);

        assert!(r.failed.is_empty(), "{:?}", r.failed);
        assert_eq!(r.solution.traces.len(), 1);
        assert_eq!(r.solution.traces[0].layer, LayerRef::bottom());
        assert_eq!(r.solution.vias.len(), 2);
        assert!(crate::lint::lint(&p, &r.solution).is_empty());
    }

    #[test]
    fn reports_two_layer_wall_without_copper() {
        let p = base(vec![
            pad(&["N"], (2.0, 10.0)),
            pad(&["N"], (18.0, 10.0)),
            wall(vec![LayerRef::top(), LayerRef::bottom()]),
        ]);
        let r = route_via_escape(&p);

        assert_eq!(r.failed.len(), 1);
        assert!(r.solution.traces.is_empty());
        assert!(r.solution.vias.is_empty());
    }

    #[test]
    fn escapes_same_layer_multi_point_star_on_bottom() {
        let mut p = base(vec![
            pad(&["N"], (2.0, 10.0)),
            pad(&["N"], (18.0, 10.0)),
            pad(&["N"], (18.0, 14.0)),
            wall(vec![LayerRef::top()]),
        ]);
        p.connections = vec![conn(
            "N",
            &[(2.0, 10.0, "top"), (18.0, 10.0, "top"), (18.0, 14.0, "top")],
        )];
        let direct = crate::direct::route_direct(&p);
        assert!(
            !direct.failed.is_empty(),
            "direct router must not solve a same-layer wall by adding vias"
        );

        let r = route_via_escape(&p);

        assert!(r.failed.is_empty(), "{:?}", r.failed);
        assert_eq!(r.solution.traces.len(), 2);
        assert!(
            r.solution
                .traces
                .iter()
                .all(|t| t.layer == LayerRef::bottom()),
            "multi-point escape should route the star on bottom: {:?}",
            r.solution.traces
        );
        assert_eq!(r.solution.vias.len(), 3);
        assert!(crate::lint::lint(&p, &r.solution).is_empty());
    }

    #[test]
    fn multi_point_tree_uses_short_chain_instead_of_fixed_root_star() {
        let mut p = base(vec![
            pad(&["N"], (2.0, 10.0)),
            pad(&["N"], (10.0, 10.0)),
            pad(&["N"], (18.0, 10.0)),
        ]);
        p.connections = vec![conn(
            "N",
            &[(2.0, 10.0, "top"), (10.0, 10.0, "top"), (18.0, 10.0, "top")],
        )];

        let r = route_via_escape(&p);

        assert!(r.failed.is_empty(), "{:?}", r.failed);
        assert_eq!(r.solution.traces.len(), 2);
        assert_eq!(r.solution.vias.len(), 0);
        assert!(
            (r.solution.metrics().wirelength - 16.0).abs() < 1e-9,
            "via-escape tree should avoid the 24mm fixed-root star: {:?}",
            r.solution
        );
        assert!(crate::lint::lint(&p, &r.solution).is_empty());
    }

    #[test]
    fn multi_point_tree_prefers_same_layer_detour_over_shorter_via_escape() {
        let mut p = base(vec![
            pad(&["N"], (2.0, 10.0)),
            pad(&["N"], (18.0, 10.0)),
            pad(&["N"], (18.0, 14.0)),
            Obstacle {
                kind: "rect".to_owned(),
                layers: vec![LayerRef::top()],
                center: Point2 { x: 10.0, y: 10.0 },
                width: 4.0,
                height: 4.0,
                connected_to: vec![],
            },
        ]);
        p.connections = vec![conn(
            "N",
            &[(2.0, 10.0, "top"), (18.0, 10.0, "top"), (18.0, 14.0, "top")],
        )];

        let r = route_via_escape(&p);

        assert!(r.failed.is_empty(), "{:?}", r.failed);
        assert_eq!(
            r.solution.vias.len(),
            0,
            "same-layer clean detour should beat a shorter alternate-layer route with terminal vias"
        );
        assert!(
            r.solution.traces.iter().all(|t| t.layer == LayerRef::top()),
            "expected top-layer detour, got {:?}",
            r.solution
        );
        assert!(crate::lint::lint(&p, &r.solution).is_empty());
    }

    #[test]
    fn net_order_portfolio_prioritizes_constrained_nets() {
        let mut p = base(vec![
            pad(&["EASY"], (2.0, 2.0)),
            pad(&["EASY"], (6.0, 2.0)),
            pad(&["HARD"], (2.0, 10.0)),
            pad(&["HARD"], (18.0, 10.0)),
            wall(vec![LayerRef::top()]),
        ]);
        p.connections = vec![
            conn("EASY", &[(2.0, 2.0, "top"), (6.0, 2.0, "top")]),
            conn("HARD", &[(2.0, 10.0, "top"), (18.0, 10.0, "top")]),
        ];

        let orders = net_order_portfolio(&p);

        assert_eq!(orders[0], vec![0, 1], "input order remains first");
        assert_eq!(
            orders[1],
            vec![1, 0],
            "obstacle-pressure order tries the constrained net before the easy net"
        );
    }

    #[test]
    fn net_order_portfolio_includes_crossing_pressure_order() {
        let mut p = base(vec![]);
        p.connections = vec![
            conn("TAIL", &[(1.0, 1.0, "top"), (3.0, 1.0, "top")]),
            conn("V1", &[(5.0, 1.0, "top"), (5.0, 19.0, "top")]),
            conn("V2", &[(8.0, 1.0, "top"), (8.0, 19.0, "top")]),
            conn("SPINE", &[(1.0, 10.0, "top"), (19.0, 10.0, "top")]),
        ];

        let orders = net_order_portfolio(&p);

        assert!(
            orders.iter().any(|order| order.as_slice() == [3, 1, 2, 0]),
            "via-escape should try a high-crossing order when same-layer escapes compete: {orders:?}"
        );
    }

    #[test]
    fn net_order_portfolio_includes_segment_obstacle_pressure_order() {
        let mut p = base(vec![
            keepout((8.0, 4.0), LayerRef::top()),
            keepout((5.0, 1.0), LayerRef::top()),
        ]);
        p.connections = vec![
            conn("BBOX_ONLY", &[(1.0, 3.0, "top"), (9.0, 9.0, "top")]),
            conn("SEGMENT_BLOCKED", &[(1.0, 1.0, "top"), (9.0, 1.0, "top")]),
            conn("TAIL", &[(1.0, 14.0, "top"), (4.0, 14.0, "top")]),
        ];
        let metrics = net_order_metrics(&p);
        let orders = net_order_portfolio(&p);

        assert_eq!(metrics[0].constraint_score, metrics[1].constraint_score);
        assert_eq!(metrics[0].segment_obstacle_pressure_um, 0);
        assert!(metrics[1].segment_obstacle_pressure_um > 0);
        assert!(
            orders.iter().any(|order| order.as_slice() == [1, 0, 2]),
            "via-escape portfolio should include segment-obstacle-pressure-first order: {orders:?}"
        );
    }

    #[test]
    fn constraint_score_ignores_false_root_star_corridor() {
        let mut p = base(vec![keepout((8.0, 2.0), LayerRef::top())]);
        p.connections = vec![conn(
            "BUS",
            &[(1.0, 1.0, "top"), (1.0, 9.0, "top"), (9.0, 9.0, "top")],
        )];

        assert_eq!(
            connection_constraint_score(&p, &p.connections[0]),
            0,
            "via-escape constraint pressure should follow the nearest tree, not a fixed root-star corridor"
        );
    }

    #[test]
    fn failed_priority_order_promotes_constrained_failed_via_escape_nets() {
        let mut p = base(vec![
            pad(&["BLOCKER"], (2.0, 2.0)),
            pad(&["BLOCKER"], (4.0, 2.0)),
            pad(&["OPEN"], (2.0, 6.0)),
            pad(&["OPEN"], (8.0, 6.0)),
            pad(&["PINCHED"], (2.0, 10.0)),
            pad(&["PINCHED"], (18.0, 10.0)),
            pad(&["TAIL"], (2.0, 14.0)),
            pad(&["TAIL"], (4.0, 14.0)),
            wall(vec![LayerRef::top()]),
        ]);
        p.connections = vec![
            conn("BLOCKER", &[(2.0, 2.0, "top"), (4.0, 2.0, "top")]),
            conn("OPEN", &[(2.0, 6.0, "top"), (8.0, 6.0, "top")]),
            conn("PINCHED", &[(2.0, 10.0, "top"), (18.0, 10.0, "top")]),
            conn("TAIL", &[(2.0, 14.0, "top"), (4.0, 14.0, "top")]),
        ];
        let failed = vec![
            FailedNet {
                connection: "OPEN".to_owned(),
                reason: "blocked".to_owned(),
            },
            FailedNet {
                connection: "PINCHED".to_owned(),
                reason: "blocked".to_owned(),
            },
        ];

        let retry = failed_priority_order(&p, &[0, 1, 2, 3], &failed);

        assert_eq!(
            &retry[..2],
            &[2, 1],
            "failed net crossing the top-layer wall should claim the alternate-layer corridor first"
        );
        assert_eq!(&retry[2..], &[0, 3]);
    }

    #[test]
    fn failed_priority_order_uses_segment_pressure_after_constraint_score() {
        let mut p = base(vec![
            keepout((8.0, 4.0), LayerRef::top()),
            keepout((5.0, 1.0), LayerRef::top()),
        ]);
        p.connections = vec![
            conn("BBOX_ONLY", &[(1.0, 3.0, "top"), (9.0, 9.0, "top")]),
            conn("SEGMENT_BLOCKED", &[(1.0, 1.0, "top"), (9.0, 1.0, "top")]),
            conn("TAIL", &[(1.0, 14.0, "top"), (4.0, 14.0, "top")]),
        ];
        let failed = vec![
            FailedNet {
                connection: "BBOX_ONLY".to_owned(),
                reason: "blocked".to_owned(),
            },
            FailedNet {
                connection: "SEGMENT_BLOCKED".to_owned(),
                reason: "blocked".to_owned(),
            },
        ];

        let retry = failed_priority_order(&p, &[0, 1, 2], &failed);

        assert_eq!(
            &retry[..2],
            &[1, 0],
            "via-escape retry should prefer true segment-corridor blockage once coarse constraint score ties"
        );
        assert_eq!(&retry[2..], &[2]);
    }

    #[test]
    fn via_escape_candidate_ranking_prefers_fewer_vias_then_wirelength() {
        let incumbent = RouteQuality {
            fault_weight: 0,
            geom: 0,
            failed_nets: 0,
            via_count: 2,
            wirelength: 4.0,
        };
        let fewer_vias = RouteQuality {
            via_count: 0,
            wirelength: 40.0,
            ..incumbent
        };
        assert!(
            !keep_candidate(&incumbent, &fewer_vias),
            "manufacturability should beat a shorter via-heavy candidate"
        );

        let shorter = RouteQuality {
            via_count: 2,
            wirelength: 3.0,
            ..incumbent
        };
        assert!(
            !keep_candidate(&incumbent, &shorter),
            "wirelength decides only after routability, failed nets, and vias tie"
        );
    }

    #[test]
    fn route_leg_uses_existing_bottom_copper_axes_for_local_detour() {
        let mut p = base(vec![]);
        p.connections = vec![
            conn("A", &[(4.0, 10.0, "top"), (16.0, 10.0, "top")]),
            conn("B", &[(10.0, 8.0, "top"), (10.0, 12.0, "top")]),
        ];
        p.net_widths.insert("FAT_POWER".to_owned(), 2.0);
        let mut solution = RouteSolution {
            traces: vec![Trace {
                connection: "A".to_owned(),
                layer: LayerRef::bottom(),
                width: p.min_trace_width,
                path: vec![pt(4.0, 10.0), pt(16.0, 10.0)],
            }],
            vias: vec![],
        };

        let trace = route_leg_on_layer(
            &p,
            &solution,
            &p.connections[1],
            &LayerRef::bottom(),
            pt(10.0, 8.0),
            pt(10.0, 12.0),
            0,
            1,
        )
        .expect("existing bottom copper should produce a local detour candidate");

        assert!(
            trace_length_um(&trace) < 19_000,
            "via-escape leg should detour locally around prior copper even when an unrelated fat net exists: {:?}",
            trace.path
        );
        assert!(
            trace
                .path
                .iter()
                .any(|p| (p.x - 3.6).abs() < 0.05 || (p.x - 16.4).abs() < 0.05),
            "detour should use signal-width local axes generated from the existing bottom trace: {:?}",
            trace.path
        );
        solution.traces.push(trace);
        assert_eq!(crate::router::geometry_violations(&p, &solution), 0);
    }

    #[test]
    fn best_solution_candidate_prefers_shorter_later_two_point_route() {
        let p = base(vec![]);
        let before = RouteSolution {
            traces: vec![],
            vias: vec![],
        };
        let long_first = RouteSolution {
            traces: vec![trace(&[(2.0, 2.0), (2.0, 18.0), (18.0, 18.0), (18.0, 2.0)])],
            vias: vec![],
        };
        let short_second = RouteSolution {
            traces: vec![trace(&[(2.0, 2.0), (18.0, 2.0)])],
            vias: vec![],
        };

        let selected = best_solution_candidate(&p, &before, vec![long_first, short_second])
            .expect("both candidates are rankable");

        assert_eq!(
            selected.traces[0].path,
            vec![Point2 { x: 2.0, y: 2.0 }, Point2 { x: 18.0, y: 2.0 }]
        );
    }
}
