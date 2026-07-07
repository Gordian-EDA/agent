//! Direct line-of-sight router for trivial boards.
//!
//! This is a narrow portfolio candidate, not a replacement for the detailed router:
//! it emits straight same-layer traces for obvious two-terminal nets and small
//! tree-shaped multi-terminal nets, then runs the DRC oracle and reports anything
//! it could not keep clean. On simple boards it is the fastest and shortest
//! possible route; on anything congested or layer-changing it loses to the
//! grid/detailed routers by normal quality ranking.

use crate::heuristics::{
    connection_crossing_pressures, connection_obstacle_pressure_um,
    connection_segment_obstacle_pressure_um, connection_span_um,
};
use crate::problem::{
    Capabilities, FailedNet, LayerRef, Point2, RouteProblem, RouteQuality, RouteResult,
    RouteSolution, Router, Trace,
};
use crate::quality::{keep_route_candidate as keep_candidate, route_quality, trace_route_cost_um};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};

/// This engine's [`RouteResult::engine`] provenance tag.
pub const ENGINE: &str = "direct";
const PROXIMITY_LENGTH_TIE_BUCKET_UM: u64 = 5_000;
const EXHAUSTIVE_DIRECT_ORDER_MAX_CONNECTIONS: usize = 4;
const DIRECT_MAX_MULTILAYER_CONNECTIONS: usize = 4;
type DirectSolutionKey = (u64, u32, usize);
type DirectLegKey = (u64, u32, usize, usize);
type DirectTraceKey = (u64, u64, u64, u32, usize);
type VisibilitySegmentKey = ((i64, i64), (i64, i64));
type VisibilityCache = BTreeMap<VisibilitySegmentKey, bool>;

/// A straight-line, no-via router for simple same-layer nets.
#[derive(Debug, Clone, Copy, Default)]
pub struct DirectLineRouter;

impl Router for DirectLineRouter {
    fn name(&self) -> &'static str {
        ENGINE
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            max_layers: u32::MAX,
            honors_escape_layers: false,
            honors_net_widths: true,
            // The DRC oracle drops copper that violates a custom outline, so the
            // result remains honest even though the router itself only draws lines.
            honors_outline: true,
        }
    }

    fn can_route(&self, problem: &RouteProblem) -> bool {
        self.capabilities().can_route(problem)
            && (problem.layer_count <= 2
                || problem.connections.len() <= DIRECT_MAX_MULTILAYER_CONNECTIONS)
    }

    fn route(&self, problem: &RouteProblem) -> RouteResult {
        route_direct(problem)
    }
}

/// Route every eligible net as straight same-layer segments and reconcile the
/// result through the same geometry/connectivity oracle used by the detailed router.
pub fn route_direct(problem: &RouteProblem) -> RouteResult {
    let mut best: Option<(RouteResult, RouteQuality)> = None;
    for order in net_order_portfolio(problem) {
        let result = route_direct_order(problem, &order);
        let q = route_quality(problem, &result);
        best = match best.take() {
            None => Some((result, q)),
            Some((bi, bq)) if keep_candidate(&bq, &q) => Some((bi, bq)),
            Some(_) => Some((result, q)),
        };
    }
    best.map(|(r, _)| r)
        .unwrap_or_else(|| route_direct_order_once(problem, &[]))
}

fn route_direct_order(problem: &RouteProblem, order: &[usize]) -> RouteResult {
    let mut best = route_direct_order_once(problem, order);
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

        let candidate = route_direct_order_once(problem, &retry_order);
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

fn route_direct_order_once(problem: &RouteProblem, order: &[usize]) -> RouteResult {
    let mut solution = RouteSolution {
        traces: Vec::new(),
        vias: Vec::new(),
    };
    let mut failed: Vec<FailedNet> = Vec::new();

    for &idx in order {
        let Some(conn) = problem.connections.get(idx) else {
            continue;
        };
        match conn.points_to_connect.as_slice() {
            [] | [_] => {}
            [a, b] if a.layer == b.layer => {
                if let Some(trace) =
                    route_leg(problem, &solution, &conn.name, a.layer.clone(), a, b)
                {
                    solution.traces.push(trace);
                } else {
                    failed.push(FailedNet {
                        connection: conn.name.clone(),
                        reason: "direct router found no clean same-layer path".to_string(),
                    });
                }
            }
            [_, _] => failed.push(FailedNet {
                connection: conn.name.clone(),
                reason: "direct router only handles same-layer two-point nets".to_string(),
            }),
            points if same_layer(points) => {
                if let Some(next) = route_same_layer_tree(problem, &solution, conn) {
                    solution = next;
                } else {
                    failed.push(FailedNet {
                        connection: conn.name.clone(),
                        reason: "direct router found no clean same-layer tree".to_string(),
                    });
                }
            }
            _ => failed.push(FailedNet {
                connection: conn.name.clone(),
                reason: "direct router only handles same-layer nets".to_string(),
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

fn net_order_portfolio(problem: &RouteProblem) -> Vec<Vec<usize>> {
    let base: Vec<usize> = (0..problem.connections.len()).collect();
    let mut orders = Vec::new();
    push_order(&mut orders, base.clone());
    if base.len() > EXHAUSTIVE_DIRECT_ORDER_MAX_CONNECTIONS {
        return orders;
    }

    let metrics = net_order_metrics(problem);

    let mut short_first = base.clone();
    short_first.sort_by(|&a, &b| {
        metrics[a].span_um.cmp(&metrics[b].span_um).then_with(|| {
            problem.connections[a]
                .name
                .cmp(&problem.connections[b].name)
        })
    });
    push_order(&mut orders, short_first);

    let mut crowded_first = base.clone();
    crowded_first.sort_by(|&a, &b| {
        metrics[b]
            .obstacle_pressure_um
            .cmp(&metrics[a].obstacle_pressure_um)
            .then_with(|| metrics[b].span_um.cmp(&metrics[a].span_um))
            .then_with(|| {
                problem.connections[a]
                    .name
                    .cmp(&problem.connections[b].name)
            })
    });
    push_order(&mut orders, crowded_first);

    let mut segment_crowded_first = base.clone();
    segment_crowded_first.sort_by(|&a, &b| {
        metrics[b]
            .segment_obstacle_pressure_um
            .cmp(&metrics[a].segment_obstacle_pressure_um)
            .then_with(|| {
                metrics[b]
                    .obstacle_pressure_um
                    .cmp(&metrics[a].obstacle_pressure_um)
            })
            .then_with(|| metrics[b].span_um.cmp(&metrics[a].span_um))
            .then_with(|| {
                problem.connections[a]
                    .name
                    .cmp(&problem.connections[b].name)
            })
    });
    push_order(&mut orders, segment_crowded_first);

    let mut many_pins_first = base.clone();
    many_pins_first.sort_by(|&a, &b| {
        metrics[b]
            .pin_count
            .cmp(&metrics[a].pin_count)
            .then_with(|| {
                metrics[b]
                    .segment_obstacle_pressure_um
                    .cmp(&metrics[a].segment_obstacle_pressure_um)
            })
            .then_with(|| metrics[b].span_um.cmp(&metrics[a].span_um))
            .then_with(|| {
                problem.connections[a]
                    .name
                    .cmp(&problem.connections[b].name)
            })
    });
    push_order(&mut orders, many_pins_first);

    let mut crossing_first = base;
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
                    .obstacle_pressure_um
                    .cmp(&metrics[a].obstacle_pressure_um)
            })
            .then_with(|| metrics[b].span_um.cmp(&metrics[a].span_um))
            .then_with(|| {
                problem.connections[a]
                    .name
                    .cmp(&problem.connections[b].name)
            })
    });
    push_order(&mut orders, crossing_first);

    orders
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DirectOrderMetric {
    pin_count: usize,
    span_um: u64,
    segment_obstacle_pressure_um: u64,
    obstacle_pressure_um: u64,
    crossing_pressure: usize,
}

fn net_order_metrics(problem: &RouteProblem) -> Vec<DirectOrderMetric> {
    let crossing_pressures = connection_crossing_pressures(problem);
    problem
        .connections
        .iter()
        .enumerate()
        .map(|(idx, conn)| DirectOrderMetric {
            pin_count: conn.points_to_connect.len(),
            span_um: connection_span_um(conn),
            segment_obstacle_pressure_um: connection_segment_obstacle_pressure_um(problem, conn),
            obstacle_pressure_um: connection_obstacle_pressure_um(problem, conn),
            crossing_pressure: crossing_pressures[idx],
        })
        .collect()
}

fn push_order(orders: &mut Vec<Vec<usize>>, order: Vec<usize>) {
    if !orders.iter().any(|existing| existing == &order) {
        orders.push(order);
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
    metrics: &[DirectOrderMetric],
    a: usize,
    b: usize,
) -> std::cmp::Ordering {
    metrics[b]
        .pin_count
        .cmp(&metrics[a].pin_count)
        .then_with(|| {
            metrics[b]
                .segment_obstacle_pressure_um
                .cmp(&metrics[a].segment_obstacle_pressure_um)
        })
        .then_with(|| {
            metrics[b]
                .obstacle_pressure_um
                .cmp(&metrics[a].obstacle_pressure_um)
        })
        .then_with(|| {
            metrics[b]
                .crossing_pressure
                .cmp(&metrics[a].crossing_pressure)
        })
        .then_with(|| metrics[b].span_um.cmp(&metrics[a].span_um))
        .then_with(|| {
            problem.connections[a]
                .name
                .cmp(&problem.connections[b].name)
        })
}

fn route_same_layer_tree(
    problem: &RouteProblem,
    solution: &RouteSolution,
    conn: &crate::problem::Connection,
) -> Option<RouteSolution> {
    let mut best: Option<(RouteSolution, (u64, u32, usize))> = None;
    for root in 0..conn.points_to_connect.len() {
        let Some(candidate) = route_same_layer_tree_from_root(problem, solution, conn, root) else {
            continue;
        };
        let key = solution_tree_key(solution, &candidate);
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

fn route_same_layer_tree_from_root(
    problem: &RouteProblem,
    solution: &RouteSolution,
    conn: &crate::problem::Connection,
    root: usize,
) -> Option<RouteSolution> {
    let layer = conn.points_to_connect[root].layer.clone();
    let mut candidate = solution.clone();
    let mut connected = vec![root];
    let mut remaining: BTreeSet<usize> = (0..conn.points_to_connect.len())
        .filter(|idx| *idx != root)
        .collect();

    while !remaining.is_empty() {
        let mut best_leg: Option<(usize, Trace, DirectLegKey)> = None;
        for &from in &connected {
            for &to in &remaining {
                let Some(trace) = route_leg(
                    problem,
                    &candidate,
                    &conn.name,
                    layer.clone(),
                    &conn.points_to_connect[from],
                    &conn.points_to_connect[to],
                ) else {
                    continue;
                };
                let key = trace_tree_key(&trace, from, to);
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
        connected.push(to);
        remaining.remove(&to);
    }

    Some(candidate)
}

fn solution_tree_key(before: &RouteSolution, after: &RouteSolution) -> DirectSolutionKey {
    let added = &after.traces[before.traces.len()..];
    let length_um = added.iter().map(trace_length_um).sum();
    let bends = added.iter().map(trace_bends).sum();
    (length_um, bends, added.len())
}

fn trace_tree_key(trace: &Trace, from: usize, to: usize) -> DirectLegKey {
    (trace_length_um(trace), trace_bends(trace), to, from)
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

fn route_leg(
    problem: &RouteProblem,
    solution: &RouteSolution,
    connection: &str,
    layer: crate::problem::LayerRef,
    a: &crate::problem::RoutePoint,
    b: &crate::problem::RoutePoint,
) -> Option<Trace> {
    let width = problem.net_width(connection);
    let paths = candidate_paths(
        problem,
        solution,
        connection,
        &layer,
        route_point(a),
        route_point(b),
    );
    if paths.is_empty() {
        return None;
    }

    if let Some(trace) =
        best_clean_trace_for_paths(problem, solution, connection, layer.clone(), width, paths)
    {
        return Some(trace);
    }

    if let Some(path) = visibility_path(problem, solution, connection, layer.clone(), a, b) {
        let trace = Trace {
            connection: connection.to_owned(),
            layer,
            width,
            path,
        };
        let mut candidate = solution.clone();
        candidate.traces.push(trace.clone());
        if direct_candidate_is_geometry_clean(problem, &candidate, connection) {
            return Some(trace);
        }
    }
    None
}

fn best_clean_trace_for_paths(
    problem: &RouteProblem,
    solution: &RouteSolution,
    connection: &str,
    layer: LayerRef,
    width: f64,
    paths: Vec<Vec<Point2>>,
) -> Option<Trace> {
    let mut best: Option<(Trace, DirectTraceKey)> = None;
    for path in paths {
        let trace = Trace {
            connection: connection.to_owned(),
            layer: layer.clone(),
            width,
            path,
        };
        let mut candidate = solution.clone();
        candidate.traces.push(trace.clone());
        if direct_candidate_is_geometry_clean(problem, &candidate, connection) {
            let key = trace_quality_key(problem, solution, &trace);
            best = match best.take() {
                None => Some((trace, key)),
                Some((incumbent, incumbent_key)) if incumbent_key <= key => {
                    Some((incumbent, incumbent_key))
                }
                Some(_) => Some((trace, key)),
            };
        }
    }
    best.map(|(trace, _)| trace)
}

fn trace_quality_key(
    problem: &RouteProblem,
    solution: &RouteSolution,
    trace: &Trace,
) -> (u64, u64, u64, u32, usize) {
    let length_um = trace_length_um(trace);
    (
        length_um / PROXIMITY_LENGTH_TIE_BUCKET_UM,
        trace_route_cost_um(problem, solution, trace, length_um),
        length_um,
        trace_bends(trace),
        trace.path.len(),
    )
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

    let (mut xs, mut ys) = candidate_axes(problem, solution, connection, layer, a, b);

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

fn candidate_axes(
    problem: &RouteProblem,
    solution: &RouteSolution,
    connection: &str,
    layer: &LayerRef,
    a: Point2,
    b: Point2,
) -> (Vec<f64>, Vec<f64>) {
    let route_width = problem.net_width(connection);
    let route_radius = route_width.max(problem.min_trace_width) / 2.0;
    let obstacle_axis = route_width.max(problem.min_trace_width);
    let inset = (problem.clearance + obstacle_axis + 0.5).max(1.0);
    let mut xs = vec![a.x, b.x, (a.x + b.x) / 2.0];
    let mut ys = vec![a.y, b.y, (a.y + b.y) / 2.0];
    xs.extend([problem.bounds.min_x + inset, problem.bounds.max_x - inset]);
    ys.extend([problem.bounds.min_y + inset, problem.bounds.max_y - inset]);
    for obstacle in &problem.obstacles {
        if !obstacle.layers.iter().any(|l| l == layer) {
            continue;
        }
        let dx = obstacle.width / 2.0 + problem.clearance + obstacle_axis;
        let dy = obstacle.height / 2.0 + problem.clearance + obstacle_axis;
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
    xs.retain(|x| {
        (*x >= problem.bounds.min_x + inset && *x <= problem.bounds.max_x - inset)
            || (*x - a.x).abs() < geom::EPS
            || (*x - b.x).abs() < geom::EPS
    });
    ys.retain(|y| {
        (*y >= problem.bounds.min_y + inset && *y <= problem.bounds.max_y - inset)
            || (*y - a.y).abs() < geom::EPS
            || (*y - b.y).abs() < geom::EPS
    });
    xs.sort_by(|a, b| a.total_cmp(b));
    ys.sort_by(|a, b| a.total_cmp(b));
    xs.dedup_by(|a, b| (*a - *b).abs() < 0.05);
    ys.dedup_by(|a, b| (*a - *b).abs() < 0.05);
    (xs, ys)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VisibilityCost {
    length_um: u64,
    bends: u32,
}

impl VisibilityCost {
    fn zero() -> Self {
        Self {
            length_um: 0,
            bends: 0,
        }
    }

    fn add(self, from_dir: VisibilityDir, edge_dir: VisibilityDir, edge_um: u64) -> Self {
        Self {
            length_um: self.length_um + edge_um,
            bends: self.bends + u32::from(from_dir.bends_into(edge_dir)),
        }
    }
}

impl Ord for VisibilityCost {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.length_um
            .cmp(&other.length_um)
            .then_with(|| self.bends.cmp(&other.bends))
    }
}

impl PartialOrd for VisibilityCost {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VisibilityDir {
    Start,
    Horizontal,
    Vertical,
}

impl VisibilityDir {
    const ALL: [Self; 3] = [Self::Start, Self::Horizontal, Self::Vertical];

    fn idx(self) -> usize {
        match self {
            Self::Start => 0,
            Self::Horizontal => 1,
            Self::Vertical => 2,
        }
    }

    fn from_idx(idx: usize) -> Self {
        Self::ALL[idx]
    }

    fn bends_into(self, next: Self) -> bool {
        self != Self::Start && self != next
    }
}

fn visibility_path(
    problem: &RouteProblem,
    solution: &RouteSolution,
    connection: &str,
    layer: LayerRef,
    a: &crate::problem::RoutePoint,
    b: &crate::problem::RoutePoint,
) -> Option<Vec<Point2>> {
    let start = route_point(a);
    let target = route_point(b);
    let (xs, ys) = candidate_axes(problem, solution, connection, &layer, start, target);
    let width = xs.len();
    let height = ys.len();
    if width == 0 || height == 0 {
        return None;
    }
    let node_count = width * height;
    let state_count = node_count * VisibilityDir::ALL.len();
    let node = |xi: usize, yi: usize| yi * width + xi;
    let state = |node: usize, dir: VisibilityDir| node * VisibilityDir::ALL.len() + dir.idx();
    let point = |node: usize| Point2 {
        x: xs[node % width],
        y: ys[node / width],
    };
    let find_axis = |values: &[f64], v: f64| values.iter().position(|x| (*x - v).abs() < 0.05);
    let start_node = node(find_axis(&xs, start.x)?, find_axis(&ys, start.y)?);
    let target_node = node(find_axis(&xs, target.x)?, find_axis(&ys, target.y)?);

    let mut best: Vec<Option<VisibilityCost>> = vec![None; state_count];
    let mut prev: Vec<Option<usize>> = vec![None; state_count];
    let mut visited = vec![false; state_count];
    let mut segment_cache = BTreeMap::new();
    let start_state = state(start_node, VisibilityDir::Start);
    let mut frontier = BinaryHeap::new();
    best[start_state] = Some(VisibilityCost::zero());
    frontier.push((Reverse(VisibilityCost::zero()), Reverse(start_state)));

    while let Some((Reverse(current_cost), Reverse(current_state))) = frontier.pop() {
        if visited[current_state] || best[current_state] != Some(current_cost) {
            continue;
        }
        visited[current_state] = true;
        let current_node = current_state / VisibilityDir::ALL.len();
        let current_dir = VisibilityDir::from_idx(current_state % VisibilityDir::ALL.len());
        let current_point = point(current_node);
        let xi = current_node % width;
        let yi = current_node / width;
        for (next_node, edge_dir) in visibility_neighbors(width, height, xi, yi, &node) {
            let next_point = point(next_node);
            if !visibility_segment_is_clean(
                problem,
                solution,
                connection,
                &layer,
                current_point,
                next_point,
                &mut segment_cache,
            ) {
                continue;
            }
            let edge_um = (current_point.dist(next_point) * 1000.0).round() as u64;
            let next_cost = current_cost.add(current_dir, edge_dir, edge_um);
            let next_state = state(next_node, edge_dir);
            if best[next_state].is_none_or(|cost| next_cost < cost) {
                best[next_state] = Some(next_cost);
                prev[next_state] = Some(current_state);
                frontier.push((Reverse(next_cost), Reverse(next_state)));
            }
        }
    }

    let target_state = VisibilityDir::ALL
        .iter()
        .map(|&dir| state(target_node, dir))
        .filter_map(|idx| best[idx].map(|cost| (idx, cost)))
        .min_by(|(a_idx, a_cost), (b_idx, b_cost)| {
            a_cost.cmp(b_cost).then_with(|| a_idx.cmp(b_idx))
        })
        .map(|(idx, _)| idx)?;

    let mut states = Vec::new();
    let mut cur = target_state;
    while cur != state(start_node, VisibilityDir::Start) {
        states.push(cur);
        let p = prev[cur]?;
        cur = p;
    }
    states.push(state(start_node, VisibilityDir::Start));
    states.reverse();

    let mut path: Vec<Point2> = states
        .into_iter()
        .map(|s| point(s / VisibilityDir::ALL.len()))
        .collect();
    path.dedup_by(|a, b| a.dist(*b) < geom::EPS);
    simplify_rectilinear_path(&mut path);
    (path.len() >= 2).then_some(path)
}

fn visibility_neighbors(
    width: usize,
    height: usize,
    xi: usize,
    yi: usize,
    node: &dyn Fn(usize, usize) -> usize,
) -> Vec<(usize, VisibilityDir)> {
    let mut out = Vec::with_capacity(4);
    if xi > 0 {
        out.push((node(xi - 1, yi), VisibilityDir::Horizontal));
    }
    if xi + 1 < width {
        out.push((node(xi + 1, yi), VisibilityDir::Horizontal));
    }
    if yi > 0 {
        out.push((node(xi, yi - 1), VisibilityDir::Vertical));
    }
    if yi + 1 < height {
        out.push((node(xi, yi + 1), VisibilityDir::Vertical));
    }
    out
}

fn visibility_segment_is_clean(
    problem: &RouteProblem,
    solution: &RouteSolution,
    connection: &str,
    layer: &LayerRef,
    a: Point2,
    b: Point2,
    cache: &mut VisibilityCache,
) -> bool {
    if a.dist(b) < geom::EPS {
        return true;
    }
    let key = visibility_segment_key(a, b);
    if let Some(&clean) = cache.get(&key) {
        return clean;
    }
    let mut candidate = solution.clone();
    candidate.traces.push(Trace {
        connection: connection.to_owned(),
        layer: layer.clone(),
        width: problem.net_width(connection),
        path: vec![a, b],
    });
    let clean = direct_candidate_is_geometry_clean(problem, &candidate, connection);
    cache.insert(key, clean);
    clean
}

fn visibility_segment_key(a: Point2, b: Point2) -> ((i64, i64), (i64, i64)) {
    let ka = (quantize_mm(a.x), quantize_mm(a.y));
    let kb = (quantize_mm(b.x), quantize_mm(b.y));
    if ka <= kb { (ka, kb) } else { (kb, ka) }
}

fn simplify_rectilinear_path(path: &mut Vec<Point2>) {
    let mut i = 1;
    while i + 1 < path.len() {
        let a = path[i - 1];
        let b = path[i];
        let c = path[i + 1];
        if ((a.x - b.x).abs() < geom::EPS && (b.x - c.x).abs() < geom::EPS)
            || ((a.y - b.y).abs() < geom::EPS && (b.y - c.y).abs() < geom::EPS)
        {
            path.remove(i);
        } else {
            i += 1;
        }
    }
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
        .map(|p| (quantize_mm(p.x), quantize_mm(p.y)))
        .collect();
    if seen.insert(key) {
        paths.push(path);
    }
}

fn quantize_mm(v: f64) -> i64 {
    (v * 1000.0).round() as i64
}

fn direct_candidate_is_geometry_clean(
    problem: &RouteProblem,
    solution: &RouteSolution,
    connection: &str,
) -> bool {
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

fn same_layer(points: &[crate::problem::RoutePoint]) -> bool {
    points
        .first()
        .is_some_and(|first| points.iter().all(|pt| pt.layer == first.layer))
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
                reason: "DRC oracle: direct trace dropped (clearance/connectivity)".to_string(),
            }),
    );
}

fn route_point(pt: &crate::problem::RoutePoint) -> crate::problem::Point2 {
    crate::problem::Point2 { x: pt.x, y: pt.y }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::problem::{Connection, LayerRef, Obstacle, Point2, Rect, RoutePoint};

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

    fn keepout(center: (f64, f64), width: f64, height: f64) -> Obstacle {
        Obstacle {
            kind: "rect".to_owned(),
            layers: vec![LayerRef::top()],
            center: Point2 {
                x: center.0,
                y: center.1,
            },
            width,
            height,
            connected_to: vec![],
        }
    }

    fn pt(x: f64, y: f64) -> Point2 {
        Point2 { x, y }
    }

    fn base(connections: Vec<Connection>, obstacles: Vec<Obstacle>) -> RouteProblem {
        RouteProblem {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles,
            connections,
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
    fn clean_candidate_ranking_prefers_roomier_detour_over_tight_straight() {
        let p = base(
            vec![conn("N", &[(2.0, 10.0, "top"), (18.0, 10.0, "top")])],
            vec![
                pad(&["N"], (2.0, 10.0)),
                pad(&["N"], (18.0, 10.0)),
                keepout((10.0, 9.2), 0.5, 0.5),
            ],
        );
        let solution = RouteSolution {
            traces: Vec::new(),
            vias: Vec::new(),
        };
        let trace = best_clean_trace_for_paths(
            &p,
            &solution,
            "N",
            LayerRef::top(),
            p.min_trace_width,
            vec![
                vec![pt(2.0, 10.0), pt(18.0, 10.0)],
                vec![pt(2.0, 10.0), pt(2.0, 11.0), pt(18.0, 11.0), pt(18.0, 10.0)],
            ],
        )
        .expect("both candidate traces should be legal");

        assert_eq!(
            trace.path,
            vec![pt(2.0, 10.0), pt(2.0, 11.0), pt(18.0, 11.0), pt(18.0, 10.0)],
            "direct router candidate ranking should avoid legal but tight obstacle margins"
        );
    }

    #[test]
    fn failed_priority_order_promotes_high_impact_failed_direct_nets() {
        let mut p = base(
            vec![
                conn("BLOCKER", &[(1.0, 1.0, "top"), (4.0, 1.0, "top")]),
                conn("SIG", &[(1.0, 3.0, "top"), (15.0, 3.0, "top")]),
                conn(
                    "BUS",
                    &[(1.0, 5.0, "top"), (15.0, 5.0, "top"), (15.0, 7.0, "top")],
                ),
                conn("TAIL", &[(1.0, 9.0, "top"), (4.0, 9.0, "top")]),
            ],
            vec![keepout((8.0, 6.0), 1.0, 4.0)],
        );
        p.bounds.max_y = 12.0;
        let failed = vec![
            FailedNet {
                connection: "SIG".to_owned(),
                reason: "blocked".to_owned(),
            },
            FailedNet {
                connection: "BUS".to_owned(),
                reason: "blocked".to_owned(),
            },
        ];

        let retry = failed_priority_order(&p, &[0, 1, 2, 3], &failed);

        assert_eq!(
            &retry[..2],
            &[2, 1],
            "direct retry should let the failed bus/crowded net claim space before a smaller signal"
        );
        assert_eq!(
            &retry[2..],
            &[0, 3],
            "non-failed nets should keep their original relative order"
        );
    }

    #[test]
    fn failed_priority_order_chains_from_current_retry_order() {
        let p = base(
            vec![
                conn("A", &[(1.0, 1.0, "top"), (4.0, 1.0, "top")]),
                conn("B", &[(1.0, 3.0, "top"), (15.0, 3.0, "top")]),
                conn("C", &[(1.0, 5.0, "top"), (15.0, 5.0, "top")]),
                conn("D", &[(1.0, 7.0, "top"), (4.0, 7.0, "top")]),
            ],
            vec![],
        );
        let current_retry_order = vec![2, 1, 0, 3];
        let failed = vec![FailedNet {
            connection: "D".to_owned(),
            reason: "still blocked".to_owned(),
        }];

        let retry = failed_priority_order(&p, &current_retry_order, &failed);

        assert_eq!(
            retry,
            vec![3, 2, 1, 0],
            "subsequent retries must promote the new failure from the accepted retry order"
        );
    }

    #[test]
    fn failed_priority_order_uses_segment_pressure_before_bbox_pressure() {
        let p = base(
            vec![
                conn("BBOX_ONLY", &[(1.0, 1.0, "top"), (9.0, 9.0, "top")]),
                conn("SEGMENT_BLOCKED", &[(1.0, 1.0, "top"), (9.0, 1.0, "top")]),
                conn("TAIL", &[(1.0, 11.0, "top"), (4.0, 11.0, "top")]),
            ],
            vec![keepout((8.0, 2.0), 0.5, 0.5), keepout((5.0, 1.0), 0.5, 0.5)],
        );
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
            "direct retry should prioritize actual segment-corridor blockage before bbox-only pressure"
        );
        assert_eq!(&retry[2..], &[2]);
    }

    #[test]
    fn net_order_portfolio_includes_crossing_pressure_order() {
        let p = base(
            vec![
                conn("TAIL", &[(1.0, 1.0, "top"), (3.0, 1.0, "top")]),
                conn("V1", &[(5.0, 1.0, "top"), (5.0, 19.0, "top")]),
                conn("V2", &[(8.0, 1.0, "top"), (8.0, 19.0, "top")]),
                conn("SPINE", &[(1.0, 10.0, "top"), (19.0, 10.0, "top")]),
            ],
            vec![],
        );

        let orders = net_order_portfolio(&p);

        assert!(
            orders.iter().any(|order| order.as_slice() == [3, 1, 2, 0]),
            "direct router should try the high-crossing spine before isolated tails: {orders:?}"
        );
    }

    #[test]
    fn net_order_portfolio_includes_segment_obstacle_pressure_order() {
        let p = base(
            vec![
                conn("BBOX_ONLY", &[(1.0, 1.0, "top"), (9.0, 9.0, "top")]),
                conn("SEGMENT_BLOCKED", &[(1.0, 1.0, "top"), (9.0, 1.0, "top")]),
                conn("TAIL", &[(1.0, 11.0, "top"), (4.0, 11.0, "top")]),
            ],
            vec![keepout((8.0, 2.0), 0.5, 0.5), keepout((5.0, 1.0), 0.5, 0.5)],
        );
        let metrics = net_order_metrics(&p);
        let orders = net_order_portfolio(&p);

        assert_eq!(metrics[0].segment_obstacle_pressure_um, 0);
        assert!(metrics[0].obstacle_pressure_um > 0);
        assert!(metrics[1].segment_obstacle_pressure_um > 0);
        assert!(
            orders.iter().any(|order| order.as_slice() == [1, 0, 2]),
            "direct router should include the segment-obstacle-pressure-first order: {orders:?}"
        );
    }

    #[test]
    fn net_order_portfolio_bounds_medium_boards_to_base_order() {
        let p = base(
            (0..5)
                .map(|idx| {
                    conn(
                        &format!("N{idx}"),
                        &[
                            (1.0, idx as f64 + 1.0, "top"),
                            (9.0, idx as f64 + 1.0, "top"),
                        ],
                    )
                })
                .collect(),
            vec![keepout((5.0, 3.0), 0.5, 0.5)],
        );

        let orders = net_order_portfolio(&p);

        assert_eq!(
            orders.len(),
            1,
            "direct router is a cheap first-pass candidate; medium boards should not try the full ordering portfolio: {orders:?}"
        );
    }

    #[test]
    fn direct_router_declines_nontrivial_multilayer_boards() {
        let mut p = base(
            (0..5)
                .map(|idx| {
                    conn(
                        &format!("N{idx}"),
                        &[
                            (1.0, idx as f64 + 1.0, "top"),
                            (9.0, idx as f64 + 1.0, "top"),
                        ],
                    )
                })
                .collect(),
            vec![],
        );
        p.layer_count = 4;

        assert!(
            !DirectLineRouter.can_route(&p),
            "direct is a cheap no-via candidate and should not monopolize nontrivial multilayer boards"
        );
    }

    #[test]
    fn routes_clean_same_layer_two_point_net() {
        let p = base(
            vec![conn("N", &[(2.0, 10.0, "top"), (18.0, 10.0, "top")])],
            vec![pad(&["N"], (2.0, 10.0)), pad(&["N"], (18.0, 10.0))],
        );
        let r = route_direct(&p);
        assert!(r.failed.is_empty(), "{:?}", r.failed);
        assert_eq!(r.solution.traces.len(), 1);
        assert_eq!(r.solution.vias.len(), 0);
        assert!(crate::lint::lint(&p, &r.solution).is_empty());
    }

    #[test]
    fn doglegs_around_simple_same_layer_keepout() {
        let p = base(
            vec![conn("N", &[(2.0, 10.0, "top"), (18.0, 10.0, "top")])],
            vec![
                pad(&["N"], (2.0, 10.0)),
                pad(&["N"], (18.0, 10.0)),
                keepout((10.0, 10.0), 4.0, 4.0),
            ],
        );
        let r = route_direct(&p);
        assert!(r.failed.is_empty(), "{:?}", r.failed);
        assert_eq!(r.solution.traces.len(), 1);
        assert!(
            r.solution.traces[0].path.len() > 2,
            "obstacle route should use a dogleg: {:?}",
            r.solution.traces[0].path
        );
        assert!(crate::lint::lint(&p, &r.solution).is_empty());
    }

    #[test]
    fn best_clean_trace_candidate_prefers_shorter_later_path() {
        let p = base(
            vec![conn("N", &[(2.0, 2.0, "top"), (18.0, 2.0, "top")])],
            vec![pad(&["N"], (2.0, 2.0)), pad(&["N"], (18.0, 2.0))],
        );
        let solution = RouteSolution {
            traces: vec![],
            vias: vec![],
        };

        let trace = best_clean_trace_for_paths(
            &p,
            &solution,
            "N",
            LayerRef::top(),
            p.min_trace_width,
            vec![
                vec![pt(2.0, 2.0), pt(2.0, 18.0), pt(18.0, 18.0), pt(18.0, 2.0)],
                vec![pt(2.0, 2.0), pt(18.0, 2.0)],
            ],
        )
        .expect("both candidates are clean");

        assert_eq!(trace.path, vec![pt(2.0, 2.0), pt(18.0, 2.0)]);
    }

    #[test]
    fn route_direct_portfolio_checks_later_clean_wirelength() {
        let p = base(
            vec![
                conn("LONG", &[(2.0, 10.0, "top"), (18.0, 10.0, "top")]),
                conn("SHORT", &[(10.0, 9.0, "top"), (10.0, 11.0, "top")]),
            ],
            vec![
                pad(&["LONG"], (2.0, 10.0)),
                pad(&["LONG"], (18.0, 10.0)),
                pad(&["SHORT"], (10.0, 9.0)),
                pad(&["SHORT"], (10.0, 11.0)),
            ],
        );
        let base_first = route_direct_order(&p, &[0, 1]);
        let short_first = route_direct_order(&p, &[1, 0]);

        assert!(base_first.failed.is_empty(), "{:?}", base_first.failed);
        assert!(short_first.failed.is_empty(), "{:?}", short_first.failed);
        assert!(
            short_first.solution.metrics().wirelength < base_first.solution.metrics().wirelength,
            "the later short-first order should be clean and shorter: base={:?} short={:?}",
            base_first.solution,
            short_first.solution
        );

        let r = route_direct(&p);

        assert!(r.failed.is_empty(), "{:?}", r.failed);
        assert_eq!(
            r.solution.metrics().wirelength,
            short_first.solution.metrics().wirelength,
            "the portfolio should not stop at the first clean direct route"
        );
        assert!(crate::lint::lint(&p, &r.solution).is_empty());
    }

    #[test]
    fn dynamic_trace_axes_make_local_detour_around_routed_copper() {
        let mut p = base(
            vec![
                conn("A", &[(4.0, 10.0, "top"), (16.0, 10.0, "top")]),
                conn("B", &[(10.0, 8.0, "top"), (10.0, 12.0, "top")]),
            ],
            vec![],
        );
        p.net_widths.insert("FAT_POWER".to_owned(), 2.0);
        let mut solution = RouteSolution {
            traces: vec![Trace {
                connection: "A".to_owned(),
                layer: LayerRef::top(),
                width: p.min_trace_width,
                path: vec![pt(4.0, 10.0), pt(16.0, 10.0)],
            }],
            vias: vec![],
        };

        let trace = route_leg(
            &p,
            &solution,
            "B",
            LayerRef::top(),
            &p.connections[1].points_to_connect[0],
            &p.connections[1].points_to_connect[1],
        )
        .expect("dynamic trace axes should expose a local clean detour");

        assert!(
            trace_length_um(&trace) < 19_000,
            "B should detour around A locally even when an unrelated fat net exists: {:?}",
            trace.path
        );
        assert!(
            trace
                .path
                .iter()
                .any(|p| (p.x - 3.6).abs() < 0.05 || (p.x - 16.4).abs() < 0.05),
            "detour should use signal-width local axes generated from the existing trace endpoints: {:?}",
            trace.path
        );
        solution.traces.push(trace);
        assert!(crate::lint::lint(&p, &solution).is_empty());
    }

    #[test]
    fn visibility_fallback_routes_alternating_same_layer_channel() {
        let p = RouteProblem {
            bounds: Rect {
                min_x: 0.0,
                max_x: 30.0,
                min_y: 0.0,
                max_y: 20.0,
            },
            ..base(
                vec![conn("N", &[(2.0, 10.0, "top"), (28.0, 10.0, "top")])],
                vec![
                    pad(&["N"], (2.0, 10.0)),
                    pad(&["N"], (28.0, 10.0)),
                    keepout((8.0, 6.5), 1.0, 13.0),
                    keepout((16.0, 13.5), 1.0, 13.0),
                    keepout((24.0, 6.5), 1.0, 13.0),
                ],
            )
        };

        let r = route_direct(&p);

        assert!(r.failed.is_empty(), "{:?}", r.failed);
        assert_eq!(r.solution.traces.len(), 1);
        assert!(
            r.solution.traces[0].path.len() > 4,
            "alternating channel should require a multi-bend route: {:?}",
            r.solution.traces[0].path
        );
        assert!(crate::lint::lint(&p, &r.solution).is_empty());
    }

    #[test]
    fn visibility_segment_cache_key_is_order_independent() {
        let a = Point2 { x: 1.25, y: 2.5 };
        let b = Point2 { x: 6.75, y: 2.5 };

        assert_eq!(visibility_segment_key(a, b), visibility_segment_key(b, a));
    }

    #[test]
    fn reports_true_same_layer_wall_without_copper() {
        let p = base(
            vec![conn("N", &[(2.0, 10.0, "top"), (18.0, 10.0, "top")])],
            vec![
                pad(&["N"], (2.0, 10.0)),
                pad(&["N"], (18.0, 10.0)),
                keepout((10.0, 10.0), 4.0, 20.0),
            ],
        );
        let r = route_direct(&p);
        assert_eq!(r.failed.len(), 1);
        assert!(r.solution.traces.is_empty());
        assert!(r.solution.vias.is_empty());
    }

    #[test]
    fn routes_clean_same_layer_multi_point_star() {
        let p = base(
            vec![conn(
                "N",
                &[
                    (10.0, 10.0, "top"),
                    (4.0, 10.0, "top"),
                    (16.0, 10.0, "top"),
                    (10.0, 16.0, "top"),
                ],
            )],
            vec![
                pad(&["N"], (10.0, 10.0)),
                pad(&["N"], (4.0, 10.0)),
                pad(&["N"], (16.0, 10.0)),
                pad(&["N"], (10.0, 16.0)),
            ],
        );
        let r = route_direct(&p);
        assert!(r.failed.is_empty(), "{:?}", r.failed);
        assert_eq!(r.solution.traces.len(), 3);
        assert_eq!(r.solution.vias.len(), 0);
        assert!(crate::lint::lint(&p, &r.solution).is_empty());
    }

    #[test]
    fn multi_point_tree_uses_short_chain_instead_of_fixed_root_star() {
        let p = base(
            vec![conn(
                "N",
                &[(2.0, 10.0, "top"), (10.0, 10.0, "top"), (18.0, 10.0, "top")],
            )],
            vec![
                pad(&["N"], (2.0, 10.0)),
                pad(&["N"], (10.0, 10.0)),
                pad(&["N"], (18.0, 10.0)),
            ],
        );

        let r = route_direct(&p);

        assert!(r.failed.is_empty(), "{:?}", r.failed);
        assert_eq!(r.solution.traces.len(), 2);
        assert_eq!(r.solution.vias.len(), 0);
        assert!(
            (r.solution.metrics().wirelength - 16.0).abs() < 1e-9,
            "nearest-tree route should avoid the 24mm fixed-root star: {:?}",
            r.solution
        );
        assert!(crate::lint::lint(&p, &r.solution).is_empty());
    }

    #[test]
    fn reports_layer_changing_net_without_copper() {
        let p = base(
            vec![conn("N", &[(2.0, 10.0, "top"), (18.0, 10.0, "bottom")])],
            vec![pad(&["N"], (2.0, 10.0)), pad(&["N"], (18.0, 10.0))],
        );
        let r = route_direct(&p);
        assert_eq!(r.failed.len(), 1);
        assert!(r.solution.traces.is_empty());
        assert!(r.solution.vias.is_empty());
    }

    #[test]
    fn reports_layer_changing_multi_point_net_without_copper() {
        let p = base(
            vec![conn(
                "N",
                &[
                    (2.0, 10.0, "top"),
                    (10.0, 10.0, "top"),
                    (18.0, 10.0, "bottom"),
                ],
            )],
            vec![
                pad(&["N"], (2.0, 10.0)),
                pad(&["N"], (10.0, 10.0)),
                pad(&["N"], (18.0, 10.0)),
            ],
        );
        let r = route_direct(&p);
        assert_eq!(r.failed.len(), 1);
        assert!(r.solution.traces.is_empty());
        assert!(r.solution.vias.is_empty());
    }
}
