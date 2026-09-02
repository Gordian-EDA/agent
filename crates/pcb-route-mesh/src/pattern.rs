//! Composite pattern router for heterogeneous simple boards.
//!
//! The individual micro-routers are intentionally narrow: `direct` handles
//! same-layer line-of-sight/doglegs, `layer-hop` handles mixed-layer terminal
//! vias, `via-escape` handles same-layer nets that need an alternate signal
//! layer, and `channel` handles preferred-direction channel trees. A board
//! with one net of each kind should not have to pay the negotiated mesh just
//! because no single micro-router owns every net. This router tries the pattern
//! candidates per net and accepts only copper that keeps the accumulated routed
//! subset DRC-clean.

use crate::channel::ChannelRouter;
use crate::direct::DirectLineRouter;
use crate::heuristics::{
    connection_crossing_pressures, connection_obstacle_pressure_um,
    connection_segment_obstacle_pressure_um, connection_span_um,
};
use crate::layer_hop::LayerHopRouter;
use crate::quality::{
    compare_route_quality as compare_quality, keep_route_candidate as keep_candidate, route_quality,
};
use crate::via_escape::ViaEscapeRouter;
use pcb_model::{
    FailedNet, Point2, RouteQuality, RouteResult, RouteSolution, RoutingCapabilities, RoutingView,
    Trace, Via, ViaSpan,
};
use std::collections::BTreeSet;

/// This engine's [`RouteResult::engine`] provenance tag.
pub const ENGINE: &str = "pattern";
const PATTERN_BEAM_WIDTH: usize = 8;
const PATTERN_NET_CANDIDATE_LIMIT: usize = 4;
const PATTERN_SYNTHETIC_CANDIDATE_LIMIT: usize = 12;
const PATTERN_SYNTHETIC_CORNER_AXIS_LIMIT: usize = 6;
const PATTERN_MAX_MULTILAYER_CONNECTIONS: usize = 5;
type TraceCandidateKey = (String, String, i64, Vec<(i64, i64)>);
type ViaCandidateKey = (String, (i64, i64), i64, i64, (u32, u32, bool, bool));
type SolutionCandidateKey = (Vec<TraceCandidateKey>, Vec<ViaCandidateKey>);
type PatternStateKey = (SolutionCandidateKey, Vec<usize>, Vec<String>);
type CandidateDiversityKey = (Vec<String>, usize, usize);

trait PatternStep {
    fn can_route(&self, problem: &RoutingView) -> bool;
    fn route(&self, problem: &RoutingView) -> RouteResult;
}

macro_rules! pattern_step {
    ($ty:ty) => {
        impl PatternStep for $ty {
            fn can_route(&self, problem: &RoutingView) -> bool {
                <$ty>::can_route(self, problem)
            }
            fn route(&self, problem: &RoutingView) -> RouteResult {
                <$ty>::route(self, problem)
            }
        }
    };
}

pattern_step!(DirectLineRouter);
pattern_step!(LayerHopRouter);
pattern_step!(ViaEscapeRouter);
pattern_step!(ChannelRouter);

/// A cheap portfolio router that composes the narrow pattern routers per net.
#[derive(Debug, Clone, Copy, Default)]
pub struct PatternRouter;

impl PatternRouter {
    pub fn name(&self) -> &'static str {
        ENGINE
    }

    pub fn capabilities(&self) -> RoutingCapabilities {
        RoutingCapabilities {
            max_layers: u32::MAX,
            honors_escape_layers: false,
            honors_net_widths: true,
            honors_outline: true,
        }
    }

    pub fn can_route(&self, problem: &RoutingView) -> bool {
        self.capabilities().can_route(problem)
            && (problem.layer_count <= 2
                || problem.connections.len() <= PATTERN_MAX_MULTILAYER_CONNECTIONS)
    }

    pub fn route(&self, problem: &RoutingView) -> RouteResult {
        route_pattern(problem)
    }
}

/// Route each net with the best clean pattern candidate, accumulating copper.
pub fn route_pattern(problem: &RoutingView) -> RouteResult {
    let direct = DirectLineRouter;
    let layer_hop = LayerHopRouter;
    let via_escape = ViaEscapeRouter;
    let channel = ChannelRouter;
    let routers: [&dyn PatternStep; 4] = [&direct, &layer_hop, &via_escape, &channel];
    let candidates = isolated_candidates(problem, &routers);

    let mut best: Option<(RouteResult, RouteQuality)> = None;
    for order in net_order_portfolio(problem, &candidates) {
        let result = route_pattern_order(problem, &candidates, &order);
        let q = RouteQuality::of(
            problem,
            &result,
            pcb_route_grid::router::geometry_violations(problem, &result.solution),
        );
        best = match best.take() {
            None => Some((result, q)),
            Some((bi, bq)) if keep_candidate(&bq, &q) => Some((bi, bq)),
            Some(_) => Some((result, q)),
        };
    }
    best.map(|(result, _)| result)
        .unwrap_or_else(|| route_pattern_order(problem, &candidates, &[]))
}

fn route_pattern_order(
    problem: &RoutingView,
    candidates: &[Vec<RouteSolution>],
    order: &[usize],
) -> RouteResult {
    let mut best = route_pattern_order_once(problem, candidates, order);
    let mut current_order = order.to_vec();
    let mut tried = vec![current_order.clone()];

    for _ in 0..2 {
        if best.failed.is_empty() {
            break;
        }

        let retry_order = failed_priority_order(problem, candidates, &current_order, &best.failed);
        if tried.iter().any(|existing| existing == &retry_order) {
            break;
        }
        tried.push(retry_order.clone());

        let candidate = route_pattern_order_once(problem, candidates, &retry_order);
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

fn route_pattern_order_once(
    problem: &RoutingView,
    candidates: &[Vec<RouteSolution>],
    order: &[usize],
) -> RouteResult {
    let mut states = vec![PatternState {
        solution: RouteSolution {
            traces: Vec::new(),
            vias: Vec::new(),
        },
        routed: Vec::new(),
        failed: Vec::new(),
    }];

    for &idx in order {
        let Some(conn) = problem.connections.get(idx) else {
            continue;
        };
        if conn.points_to_connect.len() < 2 {
            continue;
        }

        let mut next_states = Vec::new();
        for state in states {
            let next_solutions =
                route_one_net_candidates(problem, &state.solution, &state.routed, idx, candidates);
            if next_solutions.is_empty() {
                let mut failed = state.failed.clone();
                failed.push(FailedNet {
                    connection: conn.name.clone(),
                    reason: "pattern router found no clean micro-route".to_string(),
                });
                next_states.push(PatternState { failed, ..state });
                continue;
            }

            for solution in next_solutions {
                let mut routed = state.routed.clone();
                routed.push(idx);
                next_states.push(PatternState {
                    solution,
                    routed,
                    failed: state.failed.clone(),
                });
            }
        }
        prune_pattern_states(problem, &mut next_states);
        states = next_states;
    }

    states
        .into_iter()
        .map(|state| RouteResult {
            solution: state.solution,
            failed: state.failed,
            engine: ENGINE.to_owned(),
        })
        .min_by(|a, b| compare_quality(&route_quality(problem, a), &route_quality(problem, b)))
        .unwrap_or_else(|| RouteResult {
            solution: RouteSolution {
                traces: Vec::new(),
                vias: Vec::new(),
            },
            failed: Vec::new(),
            engine: ENGINE.to_owned(),
        })
}

fn failed_priority_order(
    problem: &RoutingView,
    candidates: &[Vec<RouteSolution>],
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

    let metrics = net_order_metrics(problem, candidates);
    promoted.sort_by(|&a, &b| failed_priority_cmp_with_metrics(problem, &metrics, a, b));
    promoted.extend(rest);
    promoted
}

fn failed_priority_cmp_with_metrics(
    problem: &RoutingView,
    metrics: &[PatternOrderMetric],
    a: usize,
    b: usize,
) -> std::cmp::Ordering {
    metrics[a]
        .isolated_success_count
        .cmp(&metrics[b].isolated_success_count)
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

#[derive(Clone)]
struct PatternState {
    solution: RouteSolution,
    routed: Vec<usize>,
    failed: Vec<FailedNet>,
}

fn net_order_portfolio(
    problem: &RoutingView,
    candidates: &[Vec<RouteSolution>],
) -> Vec<Vec<usize>> {
    let base: Vec<usize> = (0..problem.connections.len()).collect();
    let metrics = net_order_metrics(problem, candidates);
    let mut orders = Vec::new();
    push_order(&mut orders, base.clone());

    let mut specialized = base.clone();
    specialized.sort_by(|&a, &b| {
        metrics[a]
            .isolated_success_count
            .cmp(&metrics[b].isolated_success_count)
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
    push_order(&mut orders, specialized);

    let mut segment_crowded_first = base.clone();
    segment_crowded_first.sort_by(|&a, &b| {
        metrics[b]
            .segment_obstacle_pressure_um
            .cmp(&metrics[a].segment_obstacle_pressure_um)
            .then_with(|| {
                metrics[a]
                    .isolated_success_count
                    .cmp(&metrics[b].isolated_success_count)
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
    push_order(&mut orders, segment_crowded_first);

    let mut crossing_first = base.clone();
    crossing_first.sort_by(|&a, &b| {
        metrics[b]
            .crossing_pressure
            .cmp(&metrics[a].crossing_pressure)
            .then_with(|| {
                metrics[a]
                    .isolated_success_count
                    .cmp(&metrics[b].isolated_success_count)
            })
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
struct PatternOrderMetric {
    isolated_success_count: usize,
    span_um: u64,
    segment_obstacle_pressure_um: u64,
    obstacle_pressure_um: u64,
    crossing_pressure: usize,
}

fn net_order_metrics(
    problem: &RoutingView,
    candidates: &[Vec<RouteSolution>],
) -> Vec<PatternOrderMetric> {
    let crossing_pressures = connection_crossing_pressures(problem);
    problem
        .connections
        .iter()
        .enumerate()
        .map(|(idx, conn)| PatternOrderMetric {
            isolated_success_count: isolated_success_count(candidates, idx),
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

fn isolated_success_count(candidates: &[Vec<RouteSolution>], idx: usize) -> usize {
    candidates.get(idx).map_or(0, Vec::len)
}

fn isolated_candidates(
    problem: &RoutingView,
    routers: &[&dyn PatternStep],
) -> Vec<Vec<RouteSolution>> {
    let mut all = Vec::with_capacity(problem.connections.len());
    for idx in 0..problem.connections.len() {
        let subproblem = problem_with_connections(problem, &[idx]);
        let mut net_candidates = Vec::new();
        let mut seen = BTreeSet::new();
        for router in routers {
            if !router.can_route(&subproblem) {
                continue;
            }
            let mut result = router.route(&subproblem);
            if !result.failed.is_empty() {
                continue;
            }
            if result.solution.traces.is_empty() && result.solution.vias.is_empty() {
                continue;
            }
            crate::via_cleanup::normalize_redundant_vias(&subproblem, &mut result.solution);
            if pcb_drc::lint::lint(&subproblem, &result.solution).is_empty()
                && seen.insert(solution_candidate_key(&result.solution))
            {
                net_candidates.push(result.solution);
            }
        }
        add_synthetic_same_layer_candidates(
            problem,
            &subproblem,
            idx,
            &mut seen,
            &mut net_candidates,
        );
        all.push(net_candidates);
    }
    all
}

fn add_synthetic_same_layer_candidates(
    problem: &RoutingView,
    subproblem: &RoutingView,
    idx: usize,
    seen: &mut BTreeSet<SolutionCandidateKey>,
    out: &mut Vec<RouteSolution>,
) {
    if out.len() >= PATTERN_SYNTHETIC_CANDIDATE_LIMIT {
        return;
    }
    let Some(conn) = problem.connections.get(idx) else {
        return;
    };
    let [a, b] = conn.points_to_connect.as_slice() else {
        return;
    };
    if a.layer != b.layer || !has_nearby_foreign_obstacle(problem, conn) {
        return;
    }

    let start = a.point();
    let end = b.point();
    let (xs, ys) = synthetic_detour_axes(problem, conn, start, end, &a.layer);
    let mut paths = Vec::new();
    let mut path_seen = BTreeSet::new();
    push_synthetic_path(&mut paths, &mut path_seen, vec![start, end]);
    push_synthetic_path(
        &mut paths,
        &mut path_seen,
        vec![
            start,
            Point2 {
                x: start.x,
                y: end.y,
            },
            end,
        ],
    );
    push_synthetic_path(
        &mut paths,
        &mut path_seen,
        vec![
            start,
            Point2 {
                x: end.x,
                y: start.y,
            },
            end,
        ],
    );
    for &x in &xs {
        push_synthetic_path(
            &mut paths,
            &mut path_seen,
            vec![start, Point2 { x, y: start.y }, Point2 { x, y: end.y }, end],
        );
    }
    for &y in &ys {
        push_synthetic_path(
            &mut paths,
            &mut path_seen,
            vec![start, Point2 { x: start.x, y }, Point2 { x: end.x, y }, end],
        );
    }
    add_synthetic_corner_paths(&mut paths, &mut path_seen, start, end, &xs, &ys);

    for path in paths {
        if out.len() >= PATTERN_SYNTHETIC_CANDIDATE_LIMIT {
            break;
        }
        let solution = RouteSolution {
            traces: vec![Trace {
                connection: conn.name.clone(),
                layer: a.layer.clone(),
                width: problem.net_width(&conn.name),
                path,
            }],
            vias: vec![],
        };
        if !pcb_drc::lint::lint(subproblem, &solution).is_empty() {
            continue;
        }
        if seen.insert(solution_candidate_key(&solution)) {
            out.push(solution);
        }
    }
}

fn has_nearby_foreign_obstacle(problem: &RoutingView, conn: &pcb_model::Connection) -> bool {
    let Some((mut min_x, mut max_x, mut min_y, mut max_y)) = connection_bbox(conn) else {
        return false;
    };
    let expand =
        (problem.clearance + problem.net_width(&conn.name) + problem.via_diameter + 1.0).max(2.0);
    min_x -= expand;
    max_x += expand;
    min_y -= expand;
    max_y += expand;
    conn.points_to_connect.iter().any(|pt| {
        problem.obstacles.iter().any(|obstacle| {
            !obstacle.connected_to.iter().any(|net| net == &conn.name)
                && obstacle.layers.iter().any(|layer| layer == &pt.layer)
                && obstacle.center.x + obstacle.width / 2.0 >= min_x
                && obstacle.center.x - obstacle.width / 2.0 <= max_x
                && obstacle.center.y + obstacle.height / 2.0 >= min_y
                && obstacle.center.y - obstacle.height / 2.0 <= max_y
        })
    })
}

fn synthetic_detour_axes(
    problem: &RoutingView,
    conn: &pcb_model::Connection,
    a: Point2,
    b: Point2,
    layer: &pcb_model::LayerRef,
) -> (Vec<f64>, Vec<f64>) {
    let route_width = problem.net_width(&conn.name).max(problem.min_trace_width);
    let clearance = problem.clearance + route_width;
    let inset = (clearance + 0.5).max(1.0);
    let mut xs = vec![a.x, b.x, (a.x + b.x) / 2.0];
    let mut ys = vec![a.y, b.y, (a.y + b.y) / 2.0];
    xs.extend([problem.bounds.min_x + inset, problem.bounds.max_x - inset]);
    ys.extend([problem.bounds.min_y + inset, problem.bounds.max_y - inset]);

    for obstacle in &problem.obstacles {
        if obstacle.connected_to.iter().any(|net| net == &conn.name)
            || !obstacle.layers.iter().any(|l| l == layer)
        {
            continue;
        }
        let dx = obstacle.width / 2.0 + clearance;
        let dy = obstacle.height / 2.0 + clearance;
        xs.extend([obstacle.center.x - dx, obstacle.center.x + dx]);
        ys.extend([obstacle.center.y - dy, obstacle.center.y + dy]);
    }

    xs.retain(|x| {
        (*x >= problem.bounds.min_x + inset && *x <= problem.bounds.max_x - inset)
            || (*x - a.x).abs() < 1e-9
            || (*x - b.x).abs() < 1e-9
    });
    ys.retain(|y| {
        (*y >= problem.bounds.min_y + inset && *y <= problem.bounds.max_y - inset)
            || (*y - a.y).abs() < 1e-9
            || (*y - b.y).abs() < 1e-9
    });

    let mid_key = |v: f64, lo: f64, hi: f64| {
        let mid = (lo + hi) / 2.0;
        ((v - mid).abs() * 1000.0).round() as i64
    };
    xs.sort_by_key(|x| mid_key(*x, a.x.min(b.x), a.x.max(b.x)));
    ys.sort_by_key(|y| mid_key(*y, a.y.min(b.y), a.y.max(b.y)));
    xs.dedup_by(|a, b| (*a - *b).abs() < 0.05);
    ys.dedup_by(|a, b| (*a - *b).abs() < 0.05);
    xs.truncate(16);
    ys.truncate(16);
    (xs, ys)
}

fn add_synthetic_corner_paths(
    paths: &mut Vec<Vec<Point2>>,
    seen: &mut BTreeSet<Vec<(i64, i64)>>,
    start: Point2,
    end: Point2,
    xs: &[f64],
    ys: &[f64],
) {
    for &x in xs.iter().take(PATTERN_SYNTHETIC_CORNER_AXIS_LIMIT) {
        for &y in ys.iter().take(PATTERN_SYNTHETIC_CORNER_AXIS_LIMIT) {
            let corner = Point2 { x, y };
            push_synthetic_path(
                paths,
                seen,
                vec![
                    start,
                    Point2 { x, y: start.y },
                    corner,
                    Point2 { x: end.x, y },
                    end,
                ],
            );
            push_synthetic_path(
                paths,
                seen,
                vec![
                    start,
                    Point2 { x: start.x, y },
                    corner,
                    Point2 { x, y: end.y },
                    end,
                ],
            );
        }
    }
}

fn push_synthetic_path(
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

fn solution_candidate_key(solution: &RouteSolution) -> SolutionCandidateKey {
    let mut traces: Vec<_> = solution.traces.iter().map(trace_candidate_key).collect();
    let mut vias: Vec<_> = solution.vias.iter().map(via_candidate_key).collect();
    traces.sort();
    vias.sort();
    (traces, vias)
}

fn trace_candidate_key(trace: &Trace) -> (String, String, i64, Vec<(i64, i64)>) {
    let forward: Vec<(i64, i64)> = trace
        .path
        .iter()
        .map(|p| ((p.x * 1000.0).round() as i64, (p.y * 1000.0).round() as i64))
        .collect();
    let mut reverse = forward.clone();
    reverse.reverse();
    (
        trace.connection.clone(),
        trace.layer.0.clone(),
        (trace.width * 1000.0).round() as i64,
        forward.min(reverse),
    )
}

fn via_candidate_key(via: &Via) -> ViaCandidateKey {
    (
        via.connection.clone(),
        (
            (via.at.x * 1000.0).round() as i64,
            (via.at.y * 1000.0).round() as i64,
        ),
        (via.diameter * 1000.0).round() as i64,
        (via.drill * 1000.0).round() as i64,
        via_span_key(&via.span),
    )
}

fn via_span_key(span: &ViaSpan) -> (u32, u32, bool, bool) {
    match span {
        ViaSpan::Through => (0, 0, false, false),
        ViaSpan::Partial { from, to, micro } => (*from, *to, *micro, true),
    }
}

fn connection_bbox(conn: &pcb_model::Connection) -> Option<(f64, f64, f64, f64)> {
    let first = conn.points_to_connect.first()?;
    let (mut min_x, mut max_x, mut min_y, mut max_y) = (first.x, first.x, first.y, first.y);
    for pt in &conn.points_to_connect {
        min_x = min_x.min(pt.x);
        max_x = max_x.max(pt.x);
        min_y = min_y.min(pt.y);
        max_y = max_y.max(pt.y);
    }
    Some((min_x, max_x, min_y, max_y))
}

fn route_one_net_candidates(
    problem: &RoutingView,
    solution: &RouteSolution,
    routed: &[usize],
    idx: usize,
    candidates: &[Vec<RouteSolution>],
) -> Vec<RouteSolution> {
    let mut clean: Vec<(RouteSolution, RouteQuality, CandidateDiversityKey)> = Vec::new();
    let Some(routes) = candidates.get(idx) else {
        return Vec::new();
    };
    let validation_problem = problem_with_connections_and_extra(problem, routed, idx);
    for route in routes {
        let mut candidate = solution.clone();
        candidate.traces.extend(route.traces.clone());
        candidate.vias.extend(route.vias.clone());
        crate::via_cleanup::normalize_redundant_vias(&validation_problem, &mut candidate);

        let findings = pcb_drc::lint::lint(&validation_problem, &candidate);
        if !findings.is_empty() {
            continue;
        }
        let candidate_result = RouteResult {
            solution: candidate.clone(),
            failed: Vec::new(),
            engine: ENGINE.to_owned(),
        };
        let q = RouteQuality::of(&validation_problem, &candidate_result, 0);
        clean.push((candidate, q, candidate_diversity_key(route)));
    }
    select_diverse_candidates(clean)
}

fn select_diverse_candidates(
    mut clean: Vec<(RouteSolution, RouteQuality, CandidateDiversityKey)>,
) -> Vec<RouteSolution> {
    clean.sort_by(|(_, a, _), (_, b, _)| compare_quality(a, b));
    let mut selected = Vec::new();
    let mut diversity_seen = BTreeSet::new();
    let mut used = vec![false; clean.len()];

    for (idx, (_, _, diversity)) in clean.iter().enumerate() {
        if selected.len() >= PATTERN_NET_CANDIDATE_LIMIT {
            break;
        }
        if diversity_seen.insert(diversity.clone()) {
            selected.push(idx);
            used[idx] = true;
        }
    }
    for (idx, was_used) in used.iter().enumerate() {
        if selected.len() >= PATTERN_NET_CANDIDATE_LIMIT {
            break;
        }
        if !was_used {
            selected.push(idx);
        }
    }

    selected
        .into_iter()
        .map(|idx| clean[idx].0.clone())
        .collect()
}

fn candidate_diversity_key(route: &RouteSolution) -> CandidateDiversityKey {
    let mut layers: Vec<String> = route
        .traces
        .iter()
        .map(|trace| trace.layer.0.clone())
        .collect();
    layers.sort();
    layers.dedup();
    let bend_count = route
        .traces
        .iter()
        .map(|trace| trace.path.len().saturating_sub(2))
        .sum();
    (layers, route.vias.len(), bend_count)
}

fn prune_pattern_states(problem: &RoutingView, states: &mut Vec<PatternState>) {
    states.sort_by(|a, b| {
        let ar = RouteResult {
            solution: a.solution.clone(),
            failed: a.failed.clone(),
            engine: ENGINE.to_owned(),
        };
        let br = RouteResult {
            solution: b.solution.clone(),
            failed: b.failed.clone(),
            engine: ENGINE.to_owned(),
        };
        compare_quality(&route_quality(problem, &ar), &route_quality(problem, &br))
            .then_with(|| a.routed.cmp(&b.routed))
            .then_with(|| failed_names(&a.failed).cmp(&failed_names(&b.failed)))
    });
    let mut seen = BTreeSet::new();
    states.retain(|state| seen.insert(pattern_state_key(state)));
    states.truncate(PATTERN_BEAM_WIDTH);
}

fn pattern_state_key(state: &PatternState) -> PatternStateKey {
    (
        solution_candidate_key(&state.solution),
        state.routed.clone(),
        state
            .failed
            .iter()
            .map(|failed| failed.connection.clone())
            .collect(),
    )
}

fn failed_names(failed: &[FailedNet]) -> Vec<&str> {
    failed.iter().map(|f| f.connection.as_str()).collect()
}

fn problem_with_connections(problem: &RoutingView, indices: &[usize]) -> RoutingView {
    let mut out = problem.clone();
    out.connections = indices
        .iter()
        .filter_map(|&idx| problem.connections.get(idx).cloned())
        .collect();
    out
}

fn problem_with_connections_and_extra(
    problem: &RoutingView,
    indices: &[usize],
    extra: usize,
) -> RoutingView {
    let mut all = indices.to_vec();
    all.push(extra);
    problem_with_connections(problem, &all)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcb_model::{Connection, LayerRef, Obstacle, Point2, Rect, RoutePoint, Trace};

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

    fn pad(connected_to: &[&str], center: (f64, f64), layer: LayerRef) -> Obstacle {
        Obstacle {
            kind: "rect".to_owned(),
            layers: vec![layer],
            center: Point2 {
                x: center.0,
                y: center.1,
            },
            width: 0.6,
            height: 0.6,
            connected_to: connected_to.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    fn trace(connection: &str, path: &[(f64, f64)]) -> Trace {
        Trace {
            connection: connection.to_owned(),
            layer: LayerRef::top(),
            width: 0.2,
            path: path.iter().map(|&(x, y)| Point2 { x, y }).collect(),
        }
    }

    fn wall(layers: Vec<LayerRef>, center: (f64, f64), height: f64) -> Obstacle {
        Obstacle {
            kind: "rect".to_owned(),
            layers,
            center: Point2 {
                x: center.0,
                y: center.1,
            },
            width: 1.0,
            height,
            connected_to: vec![],
        }
    }

    fn base() -> RoutingView {
        RoutingView {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles: Vec::new(),
            connections: Vec::new(),
            bounds: Rect {
                min_x: 0.0,
                max_x: 30.0,
                min_y: 0.0,
                max_y: 20.0,
            },
            clearance: 0.2,
            via_diameter: 0.6,
            via_drill: 0.3,
            net_widths: Default::default(),
            outline: None,
            escape_layers: Default::default(),
            plane_nets: Default::default(),
            fixed_copper: Default::default(),
            nets: None,
        }
    }

    #[test]
    fn composes_direct_layer_hop_and_via_escape_nets() {
        let mut p = base();
        p.connections = vec![
            conn("D", &[(2.0, 2.0, "top"), (8.0, 2.0, "top")]),
            conn("L", &[(2.0, 6.0, "top"), (8.0, 6.0, "bottom")]),
            conn("V", &[(2.0, 14.0, "top"), (20.0, 14.0, "top")]),
        ];
        p.obstacles = vec![
            pad(&["D"], (2.0, 2.0), LayerRef::top()),
            pad(&["D"], (8.0, 2.0), LayerRef::top()),
            pad(&["L"], (2.0, 6.0), LayerRef::top()),
            pad(&["L"], (8.0, 6.0), LayerRef::bottom()),
            pad(&["V"], (2.0, 14.0), LayerRef::top()),
            pad(&["V"], (20.0, 14.0), LayerRef::top()),
            wall(vec![LayerRef::top()], (11.0, 10.0), 20.0),
        ];

        let direct = crate::direct::route_direct(&p);
        let layer_hop = crate::layer_hop::route_layer_hop(&p);
        let via_escape = crate::via_escape::route_via_escape(&p);
        assert!(!direct.failed.is_empty());
        assert!(!layer_hop.failed.is_empty());
        assert!(!via_escape.failed.is_empty());

        let r = route_pattern(&p);

        assert!(r.failed.is_empty(), "{:?}", r.failed);
        assert_eq!(r.solution.traces.len(), 3);
        assert_eq!(r.solution.vias.len(), 3);
        assert!(pcb_drc::lint::lint(&p, &r.solution).is_empty());
    }

    #[test]
    fn net_order_portfolio_keeps_input_then_prioritizes_specialized_nets() {
        let mut p = base();
        p.connections = vec![
            conn("D", &[(2.0, 2.0, "top"), (8.0, 2.0, "top")]),
            conn("L", &[(2.0, 6.0, "top"), (8.0, 6.0, "bottom")]),
            conn("V", &[(2.0, 14.0, "top"), (20.0, 14.0, "top")]),
        ];
        p.obstacles = vec![
            pad(&["D"], (2.0, 2.0), LayerRef::top()),
            pad(&["D"], (8.0, 2.0), LayerRef::top()),
            pad(&["L"], (2.0, 6.0), LayerRef::top()),
            pad(&["L"], (8.0, 6.0), LayerRef::bottom()),
            pad(&["V"], (2.0, 14.0), LayerRef::top()),
            pad(&["V"], (20.0, 14.0), LayerRef::top()),
            wall(vec![LayerRef::top()], (11.0, 10.0), 20.0),
        ];
        let direct = DirectLineRouter;
        let layer_hop = LayerHopRouter;
        let via_escape = ViaEscapeRouter;
        let channel = ChannelRouter;
        let routers: [&dyn PatternStep; 4] = [&direct, &layer_hop, &via_escape, &channel];
        let candidates = isolated_candidates(&p, &routers);
        let counts: Vec<usize> = candidates.iter().map(Vec::len).collect();

        let orders = net_order_portfolio(&p, &candidates);

        assert_eq!(orders[0], vec![0, 1, 2]);
        assert!(
            counts.iter().all(|count| *count >= 1),
            "isolated candidates should keep at least one clean route per specialized net: {counts:?}"
        );
        assert_eq!(
            orders[1][0], 2,
            "specialized order routes the constrained via-escape net before open-area nets"
        );
    }

    #[test]
    fn net_order_portfolio_uses_obstacle_pressure_before_span() {
        let mut p = base();
        p.connections = vec![
            conn("OPEN", &[(2.0, 2.0, "top"), (24.0, 2.0, "top")]),
            conn("PINCHED", &[(2.0, 10.0, "top"), (10.0, 10.0, "top")]),
            conn("MID", &[(2.0, 16.0, "top"), (12.0, 16.0, "top")]),
        ];
        p.obstacles = vec![wall(vec![LayerRef::top()], (6.0, 10.0), 2.0)];
        let empty = RouteSolution {
            traces: vec![],
            vias: vec![],
        };
        let equal_candidates = vec![vec![empty.clone()], vec![empty.clone()], vec![empty]];

        let orders = net_order_portfolio(&p, &equal_candidates);

        assert_eq!(orders[0], vec![0, 1, 2]);
        assert_eq!(
            orders[1][0], 1,
            "the obstacle-overlapped net should run before longer open-area nets"
        );
    }

    #[test]
    fn net_order_portfolio_includes_segment_obstacle_pressure_order() {
        let mut p = base();
        p.connections = vec![
            conn("BBOX_ONLY", &[(1.0, 1.0, "top"), (9.0, 9.0, "top")]),
            conn("SEGMENT_BLOCKED", &[(1.0, 1.0, "top"), (9.0, 1.0, "top")]),
            conn("TAIL", &[(1.0, 11.0, "top"), (4.0, 11.0, "top")]),
        ];
        p.obstacles = vec![
            wall(vec![LayerRef::top()], (8.0, 2.0), 0.5),
            wall(vec![LayerRef::top()], (5.0, 1.0), 0.5),
        ];
        let empty = RouteSolution {
            traces: vec![],
            vias: vec![],
        };
        let equal_candidates = vec![vec![empty.clone()], vec![empty.clone()], vec![empty]];
        let metrics = net_order_metrics(&p, &equal_candidates);

        let orders = net_order_portfolio(&p, &equal_candidates);

        assert_eq!(metrics[0].segment_obstacle_pressure_um, 0);
        assert!(metrics[0].obstacle_pressure_um > 0);
        assert!(metrics[1].segment_obstacle_pressure_um > 0);
        assert!(
            orders.iter().any(|order| order.as_slice() == [1, 0, 2]),
            "pattern router should include the segment-obstacle-pressure-first order: {orders:?}"
        );
    }

    #[test]
    fn failed_priority_order_uses_segment_pressure_before_bbox_pressure() {
        let mut p = base();
        p.connections = vec![
            conn("BBOX_ONLY", &[(1.0, 1.0, "top"), (9.0, 9.0, "top")]),
            conn("SEGMENT_BLOCKED", &[(1.0, 1.0, "top"), (9.0, 1.0, "top")]),
            conn("TAIL", &[(1.0, 11.0, "top"), (4.0, 11.0, "top")]),
        ];
        p.obstacles = vec![
            wall(vec![LayerRef::top()], (8.0, 2.0), 0.5),
            wall(vec![LayerRef::top()], (5.0, 1.0), 0.5),
        ];
        let empty = RouteSolution {
            traces: vec![],
            vias: vec![],
        };
        let equal_candidates = vec![vec![empty.clone()], vec![empty.clone()], vec![empty]];
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

        let retry = failed_priority_order(&p, &equal_candidates, &[0, 1, 2], &failed);

        assert_eq!(
            &retry[..2],
            &[1, 0],
            "pattern retry should prioritize actual segment-corridor blockage before bbox-only pressure"
        );
        assert_eq!(&retry[2..], &[2]);
    }

    #[test]
    fn net_order_portfolio_includes_crossing_pressure_order() {
        let mut p = base();
        p.connections = vec![
            conn("TAIL", &[(1.0, 1.0, "top"), (3.0, 1.0, "top")]),
            conn("V1", &[(5.0, 1.0, "top"), (5.0, 19.0, "top")]),
            conn("V2", &[(8.0, 1.0, "top"), (8.0, 19.0, "top")]),
            conn("SPINE", &[(1.0, 10.0, "top"), (19.0, 10.0, "top")]),
        ];
        let empty = RouteSolution {
            traces: vec![],
            vias: vec![],
        };
        let equal_candidates = vec![
            vec![empty.clone()],
            vec![empty.clone()],
            vec![empty.clone()],
            vec![empty],
        ];

        let orders = net_order_portfolio(&p, &equal_candidates);

        assert!(
            orders.iter().any(|order| order.as_slice() == [3, 1, 2, 0]),
            "pattern router should try high-crossing nets before isolated tails: {orders:?}"
        );
    }

    #[test]
    fn isolated_candidate_cache_keeps_clean_candidates_per_net() {
        let mut p = base();
        p.connections = vec![
            conn("D", &[(2.0, 2.0, "top"), (8.0, 2.0, "top")]),
            conn("L", &[(2.0, 6.0, "top"), (8.0, 6.0, "bottom")]),
            conn("V", &[(2.0, 14.0, "top"), (20.0, 14.0, "top")]),
        ];
        p.obstacles = vec![
            pad(&["D"], (2.0, 2.0), LayerRef::top()),
            pad(&["D"], (8.0, 2.0), LayerRef::top()),
            pad(&["L"], (2.0, 6.0), LayerRef::top()),
            pad(&["L"], (8.0, 6.0), LayerRef::bottom()),
            pad(&["V"], (2.0, 14.0), LayerRef::top()),
            pad(&["V"], (20.0, 14.0), LayerRef::top()),
            wall(vec![LayerRef::top()], (11.0, 10.0), 20.0),
        ];
        let direct = DirectLineRouter;
        let layer_hop = LayerHopRouter;
        let via_escape = ViaEscapeRouter;
        let channel = ChannelRouter;
        let routers: [&dyn PatternStep; 4] = [&direct, &layer_hop, &via_escape, &channel];

        let candidates = isolated_candidates(&p, &routers);
        let counts: Vec<usize> = candidates.iter().map(Vec::len).collect();

        assert!(
            counts.iter().all(|count| *count >= 1),
            "direct/layer-hop/via-escape candidates should all survive dedupe at least once: {counts:?}"
        );
        for (idx, routes) in candidates.iter().enumerate() {
            let subproblem = problem_with_connections(&p, &[idx]);
            assert!(routes.iter().all(|solution| {
                (!solution.traces.is_empty() || !solution.vias.is_empty())
                    && pcb_drc::lint::lint(&subproblem, solution).is_empty()
            }));
        }
    }

    #[test]
    fn isolated_candidates_include_channel_router_options() {
        let mut p = base();
        p.connections = vec![conn("C", &[(2.0, 10.0, "top"), (10.0, 18.0, "top")])];
        p.obstacles = vec![
            pad(&["C"], (2.0, 10.0), LayerRef::top()),
            pad(&["C"], (10.0, 18.0), LayerRef::top()),
            Obstacle {
                kind: "rect".to_owned(),
                layers: vec![LayerRef::top()],
                center: Point2 { x: 10.0, y: 14.0 },
                width: 1.0,
                height: 4.0,
                connected_to: vec![],
            },
            Obstacle {
                kind: "rect".to_owned(),
                layers: vec![LayerRef::top(), LayerRef::bottom()],
                center: Point2 { x: 6.0, y: 14.0 },
                width: 1.0,
                height: 1.0,
                connected_to: vec![],
            },
            Obstacle {
                kind: "rect".to_owned(),
                layers: vec![LayerRef::bottom()],
                center: Point2 { x: 6.0, y: 10.0 },
                width: 4.0,
                height: 1.0,
                connected_to: vec![],
            },
            Obstacle {
                kind: "rect".to_owned(),
                layers: vec![LayerRef::bottom()],
                center: Point2 { x: 6.0, y: 18.0 },
                width: 4.0,
                height: 1.0,
                connected_to: vec![],
            },
        ];
        let direct = DirectLineRouter;
        let layer_hop = LayerHopRouter;
        let via_escape = ViaEscapeRouter;
        let channel = ChannelRouter;
        let without_channel: [&dyn PatternStep; 3] = [&direct, &layer_hop, &via_escape];
        let with_channel: [&dyn PatternStep; 4] = [&direct, &layer_hop, &via_escape, &channel];

        let base_candidates = isolated_candidates(&p, &without_channel);
        let channel_candidates = isolated_candidates(&p, &with_channel);

        assert!(
            !base_candidates[0]
                .iter()
                .any(|solution| solution.traces.len() > 1),
            "base micro-routers should not emit the split preferred-direction channel topology: {:?}",
            base_candidates[0]
        );
        assert!(
            channel_candidates[0]
                .iter()
                .any(|solution| solution.traces.len() > 1),
            "channel router should add a unique split preferred-direction candidate: {:?}",
            channel_candidates[0]
        );
    }

    #[test]
    fn isolated_candidates_include_multi_pin_channel_tree_options() {
        let mut p = base();
        p.connections = vec![conn(
            "BUS",
            &[(2.0, 2.0, "top"), (8.0, 2.0, "top"), (8.0, 8.0, "top")],
        )];
        p.obstacles = vec![
            pad(&["BUS"], (2.0, 2.0), LayerRef::top()),
            pad(&["BUS"], (8.0, 2.0), LayerRef::top()),
            pad(&["BUS"], (8.0, 8.0), LayerRef::top()),
            Obstacle {
                kind: "rect".to_owned(),
                layers: vec![LayerRef::top()],
                center: Point2 { x: 8.0, y: 5.0 },
                width: 1.0,
                height: 3.0,
                connected_to: vec![],
            },
        ];
        let direct = DirectLineRouter;
        let layer_hop = LayerHopRouter;
        let via_escape = ViaEscapeRouter;
        let channel = ChannelRouter;
        let without_channel: [&dyn PatternStep; 3] = [&direct, &layer_hop, &via_escape];
        let with_channel: [&dyn PatternStep; 4] = [&direct, &layer_hop, &via_escape, &channel];

        let base_candidates = isolated_candidates(&p, &without_channel);
        let channel_candidates = isolated_candidates(&p, &with_channel);

        assert!(
            channel_candidates[0].len() > base_candidates[0].len(),
            "channel should expand the isolated candidate set for a multi-pin channel tree: base={:?} channel={:?}",
            base_candidates[0],
            channel_candidates[0]
        );
        assert!(
            channel_candidates[0].iter().any(|solution| {
                solution.traces.len() >= 2
                    && pcb_drc::lint::lint(&problem_with_connections(&p, &[0]), solution).is_empty()
            }),
            "channel router should add a clean multi-pin preferred-direction tree: {:?}",
            channel_candidates[0]
        );
    }

    #[test]
    fn isolated_candidates_add_same_layer_detours_for_future_conflicts() {
        let mut p = base();
        p.connections = vec![
            conn("A", &[(2.0, 10.0, "top"), (18.0, 10.0, "top")]),
            conn("B", &[(10.0, 8.0, "top"), (10.0, 12.0, "top")]),
        ];
        p.obstacles = vec![
            pad(&["A"], (2.0, 10.0), LayerRef::top()),
            pad(&["A"], (18.0, 10.0), LayerRef::top()),
            pad(&["B"], (10.0, 8.0), LayerRef::top()),
            pad(&["B"], (10.0, 12.0), LayerRef::top()),
        ];
        let direct = DirectLineRouter;
        let layer_hop = LayerHopRouter;
        let via_escape = ViaEscapeRouter;
        let channel = ChannelRouter;
        let routers: [&dyn PatternStep; 4] = [&direct, &layer_hop, &via_escape, &channel];

        let candidates = isolated_candidates(&p, &routers);
        let r = route_pattern(&p);

        assert!(
            candidates[0].len() > 1,
            "A should expose synthetic detours around B's future access corridor: {:?}",
            candidates[0]
        );
        assert!(r.failed.is_empty(), "{:?}", r.failed);
        assert_eq!(r.solution.traces.len(), 2);
        assert!(
            r.solution.traces.iter().any(|trace| {
                trace.connection == "A" && trace.path.iter().any(|p| p.y < 7.5 || p.y > 12.5)
            }),
            "pattern should select A's longer synthetic detour when the straight route blocks B: {:?}",
            r.solution.traces
        );
        assert!(pcb_drc::lint::lint(&p, &r.solution).is_empty());
    }

    #[test]
    fn isolated_candidate_key_treats_reversed_trace_as_duplicate() {
        let a = RouteSolution {
            traces: vec![trace("N", &[(1.0, 1.0), (4.0, 1.0)])],
            vias: vec![],
        };
        let b = RouteSolution {
            traces: vec![trace("N", &[(4.0, 1.0), (1.0, 1.0)])],
            vias: vec![],
        };

        assert_eq!(solution_candidate_key(&a), solution_candidate_key(&b));
    }

    #[test]
    fn route_pattern_order_keeps_alternate_candidate_that_unblocks_later_net() {
        let mut p = base();
        p.connections = vec![
            conn("A", &[(2.0, 10.0, "top"), (18.0, 10.0, "top")]),
            conn("B", &[(10.0, 8.0, "top"), (10.0, 12.0, "top")]),
        ];

        let short_blocking_a = RouteSolution {
            traces: vec![trace("A", &[(2.0, 10.0), (18.0, 10.0)])],
            vias: vec![],
        };
        let longer_unblocking_a = RouteSolution {
            traces: vec![trace(
                "A",
                &[(2.0, 10.0), (2.0, 14.0), (18.0, 14.0), (18.0, 10.0)],
            )],
            vias: vec![],
        };
        let b = RouteSolution {
            traces: vec![trace("B", &[(10.0, 8.0), (10.0, 12.0)])],
            vias: vec![],
        };
        let candidates = vec![vec![short_blocking_a, longer_unblocking_a], vec![b]];

        let r = route_pattern_order(&p, &candidates, &[0, 1]);

        assert!(r.failed.is_empty(), "{:?}", r.failed);
        assert_eq!(r.solution.traces.len(), 2);
        assert!(
            r.solution
                .traces
                .iter()
                .any(|t| t.connection == "A" && t.path.iter().any(|p| (p.y - 14.0).abs() < 1e-9)),
            "beam should retain the longer A candidate because it lets B route"
        );
        assert!(pcb_drc::lint::lint(&p, &r.solution).is_empty());
    }

    #[test]
    fn route_one_net_candidates_preserves_topology_diversity_under_limit() {
        let mut p = base();
        p.connections = vec![conn("C", &[(2.0, 10.0, "top"), (10.0, 18.0, "top")])];
        p.obstacles = vec![
            pad(&["C"], (2.0, 10.0), LayerRef::top()),
            pad(&["C"], (10.0, 18.0), LayerRef::top()),
        ];
        let top = |pts: &[(f64, f64)]| RouteSolution {
            traces: vec![trace("C", pts)],
            vias: vec![],
        };
        let via_candidate = RouteSolution {
            traces: vec![Trace {
                connection: "C".to_owned(),
                layer: LayerRef::bottom(),
                width: p.min_trace_width,
                path: vec![Point2 { x: 2.0, y: 10.0 }, Point2 { x: 10.0, y: 18.0 }],
            }],
            vias: vec![
                Via {
                    connection: "C".to_owned(),
                    at: Point2 { x: 2.0, y: 10.0 },
                    diameter: p.via_diameter,
                    drill: p.via_drill,
                    span: ViaSpan::Through,
                },
                Via {
                    connection: "C".to_owned(),
                    at: Point2 { x: 10.0, y: 18.0 },
                    diameter: p.via_diameter,
                    drill: p.via_drill,
                    span: ViaSpan::Through,
                },
            ],
        };
        let candidates = vec![vec![
            top(&[(2.0, 10.0), (10.0, 18.0)]),
            top(&[(2.0, 10.0), (2.0, 18.0), (10.0, 18.0)]),
            top(&[(2.0, 10.0), (10.0, 10.0), (10.0, 18.0)]),
            top(&[(2.0, 10.0), (4.0, 10.0), (4.0, 18.0), (10.0, 18.0)]),
            via_candidate,
        ]];

        let selected = route_one_net_candidates(
            &p,
            &RouteSolution {
                traces: vec![],
                vias: vec![],
            },
            &[],
            0,
            &candidates,
        );

        assert_eq!(selected.len(), PATTERN_NET_CANDIDATE_LIMIT);
        assert!(
            selected.iter().any(|solution| {
                solution
                    .traces
                    .iter()
                    .any(|trace| trace.connection == "C" && trace.layer == LayerRef::bottom())
                    && solution.vias.len() == 2
            }),
            "diversity-preserving truncation should keep the via/layer candidate: {selected:?}"
        );
    }

    #[test]
    fn route_one_net_candidates_normalizes_duplicate_vias_before_lint() {
        let mut p = base();
        p.connections = vec![conn("N", &[(2.0, 10.0, "top"), (18.0, 10.0, "bottom")])];
        p.obstacles = vec![
            pad(&["N"], (2.0, 10.0), LayerRef::top()),
            pad(&["N"], (18.0, 10.0), LayerRef::bottom()),
        ];
        let via = Via {
            connection: "N".to_owned(),
            at: Point2 { x: 10.0, y: 10.0 },
            diameter: p.via_diameter,
            drill: p.via_drill,
            span: ViaSpan::Through,
        };
        let duplicate_via_candidate = RouteSolution {
            traces: vec![
                Trace {
                    connection: "N".to_owned(),
                    layer: LayerRef::top(),
                    width: p.min_trace_width,
                    path: vec![Point2 { x: 2.0, y: 10.0 }, Point2 { x: 10.0, y: 10.0 }],
                },
                Trace {
                    connection: "N".to_owned(),
                    layer: LayerRef::bottom(),
                    width: p.min_trace_width,
                    path: vec![Point2 { x: 10.0, y: 10.0 }, Point2 { x: 18.0, y: 10.0 }],
                },
            ],
            vias: vec![via.clone(), via],
        };

        let selected = route_one_net_candidates(
            &p,
            &RouteSolution {
                traces: vec![],
                vias: vec![],
            },
            &[],
            0,
            &[vec![duplicate_via_candidate]],
        );

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].vias.len(), 1);
        assert!(pcb_drc::lint::lint(&p, &selected[0]).is_empty());
    }

    #[test]
    fn route_pattern_order_retries_failed_net_first_to_reveal_detour() {
        let mut p = base();
        p.connections = vec![
            conn("A", &[(2.0, 10.0, "top"), (18.0, 10.0, "top")]),
            conn("B", &[(10.0, 8.0, "top"), (10.0, 12.0, "top")]),
        ];

        let blocking_a: Vec<RouteSolution> = [8.5, 9.0, 10.0, 11.0]
            .into_iter()
            .map(|y| RouteSolution {
                traces: vec![trace(
                    "A",
                    &[(2.0, 10.0), (2.0, y), (18.0, y), (18.0, 10.0)],
                )],
                vias: vec![],
            })
            .collect();
        let mut a_candidates = blocking_a;
        a_candidates.push(RouteSolution {
            traces: vec![trace(
                "A",
                &[(2.0, 10.0), (2.0, 14.0), (18.0, 14.0), (18.0, 10.0)],
            )],
            vias: vec![],
        });
        let b = RouteSolution {
            traces: vec![trace("B", &[(10.0, 8.0), (10.0, 12.0)])],
            vias: vec![],
        };
        let candidates = vec![a_candidates, vec![b]];

        let r = route_pattern_order(&p, &candidates, &[0, 1]);

        assert!(r.failed.is_empty(), "{:?}", r.failed);
        assert_eq!(r.solution.traces.len(), 2);
        assert!(
            r.solution
                .traces
                .iter()
                .any(|t| t.connection == "A" && t.path.iter().any(|p| (p.y - 14.0).abs() < 1e-9)),
            "failed-net retry should route B first, forcing A onto its longer clean detour: {:?}",
            r.solution.traces
        );
        assert!(pcb_drc::lint::lint(&p, &r.solution).is_empty());
    }

    #[test]
    fn synthetic_corner_paths_add_two_axis_detours() {
        let start = Point2 { x: 2.0, y: 3.0 };
        let end = Point2 { x: 18.0, y: 17.0 };
        let mut paths = Vec::new();
        let mut seen = BTreeSet::new();

        add_synthetic_corner_paths(&mut paths, &mut seen, start, end, &[7.0], &[13.0]);

        assert!(paths.iter().any(|path| {
            path == &vec![
                start,
                Point2 { x: 7.0, y: 3.0 },
                Point2 { x: 7.0, y: 13.0 },
                Point2 { x: 18.0, y: 13.0 },
                end,
            ]
        }));
        assert!(paths.iter().any(|path| {
            path == &vec![
                start,
                Point2 { x: 2.0, y: 13.0 },
                Point2 { x: 7.0, y: 13.0 },
                Point2 { x: 7.0, y: 17.0 },
                end,
            ]
        }));
    }

    #[test]
    fn prune_pattern_states_dedupes_equivalent_states_before_beam_cut() {
        let mut states: Vec<PatternState> = (0..PATTERN_BEAM_WIDTH)
            .map(|_| PatternState {
                solution: RouteSolution {
                    traces: vec![],
                    vias: vec![],
                },
                routed: vec![0],
                failed: vec![],
            })
            .collect();
        states.push(PatternState {
            solution: RouteSolution {
                traces: vec![trace("N", &[(1.0, 1.0), (2.0, 1.0)])],
                vias: vec![],
            },
            routed: vec![0],
            failed: vec![],
        });
        let p = base();

        prune_pattern_states(&p, &mut states);

        assert_eq!(
            states.len(),
            2,
            "equivalent states should collapse before the beam-width cutoff"
        );
        assert!(
            states.iter().any(|state| !state.solution.traces.is_empty()),
            "dedupe should leave room for the distinct later candidate"
        );
    }
}
