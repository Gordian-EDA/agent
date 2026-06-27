//! Deterministic layer-hop router for trivial layer changes.
//!
//! This portfolio candidate covers the narrow case neither direct nor via-escape
//! owns: a small net's terminals already live on different layers, and a clean
//! route can be made by choosing one signal layer plus the minimum terminal vias.
//! It is a cheap pattern router, not a maze router; heavier engines still handle
//! congestion.

use crate::problem::{
    Capabilities, FailedNet, LayerRef, Point2, RouteProblem, RouteQuality, RouteResult,
    RouteSolution, Router, Trace, Via, ViaSpan,
};
use std::collections::BTreeSet;

/// This engine's [`RouteResult::engine`] provenance tag.
pub const ENGINE: &str = "layer-hop";

/// A narrow router for simple mixed-layer point/star nets.
#[derive(Debug, Clone, Copy, Default)]
pub struct LayerHopRouter;

impl Router for LayerHopRouter {
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

    fn route(&self, problem: &RouteProblem) -> RouteResult {
        route_layer_hop(problem)
    }
}

/// Route eligible mixed-layer point/star nets with terminal vias only.
pub fn route_layer_hop(problem: &RouteProblem) -> RouteResult {
    let mut best: Option<(RouteResult, RouteQuality)> = None;
    for order in net_order_portfolio(problem) {
        let result = route_layer_hop_order(problem, &order);
        let q = route_quality(problem, &result);
        best = match best.take() {
            None => Some((result, q)),
            Some((bi, bq)) if keep_candidate(&bq, &q) => Some((bi, bq)),
            Some(_) => Some((result, q)),
        };
    }
    best.map(|(r, _)| r)
        .unwrap_or_else(|| route_layer_hop_order(problem, &[]))
}

fn route_layer_hop_order(problem: &RouteProblem, order: &[usize]) -> RouteResult {
    let mut best = route_layer_hop_order_once(problem, order);
    let mut tried = vec![order.to_vec()];

    for _ in 0..2 {
        if best.failed.is_empty() {
            break;
        }

        let retry_order = failed_priority_order(problem, order, &best.failed);
        if tried.iter().any(|existing| existing == &retry_order) {
            break;
        }
        tried.push(retry_order.clone());

        let candidate = route_layer_hop_order_once(problem, &retry_order);
        let incumbent_quality = route_quality(problem, &best);
        let candidate_quality = route_quality(problem, &candidate);
        if keep_candidate(&incumbent_quality, &candidate_quality) {
            break;
        }
        best = candidate;
    }

    best
}

fn route_layer_hop_order_once(problem: &RouteProblem, order: &[usize]) -> RouteResult {
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
            points if mixed_layers(points) => {
                if let Some(next) = route_mixed_layer_tree(problem, &solution, conn) {
                    solution = next;
                } else {
                    failed.push(FailedNet {
                        connection: conn.name.clone(),
                        reason: "layer-hop found no clean terminal-via tree".to_string(),
                    });
                }
            }
            [_, _] => failed.push(FailedNet {
                connection: conn.name.clone(),
                reason: "layer-hop only handles different-layer two-point nets".to_string(),
            }),
            _ => failed.push(FailedNet {
                connection: conn.name.clone(),
                reason: "layer-hop only handles mixed-layer point/star nets".to_string(),
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

    promoted.sort_by(|&a, &b| failed_priority_cmp(problem, a, b));
    promoted.extend(rest);
    promoted
}

fn failed_priority_cmp(problem: &RouteProblem, a: usize, b: usize) -> std::cmp::Ordering {
    estimated_required_vias(problem, &problem.connections[a])
        .cmp(&estimated_required_vias(problem, &problem.connections[b]))
        .then_with(|| {
            problem.connections[b]
                .points_to_connect
                .len()
                .cmp(&problem.connections[a].points_to_connect.len())
        })
        .then_with(|| {
            connection_span_um(&problem.connections[b])
                .cmp(&connection_span_um(&problem.connections[a]))
        })
        .then_with(|| {
            problem.connections[a]
                .name
                .cmp(&problem.connections[b].name)
        })
}

fn route_mixed_layer_tree(
    problem: &RouteProblem,
    solution: &RouteSolution,
    conn: &crate::problem::Connection,
) -> Option<RouteSolution> {
    if conn.points_to_connect.len() < 2 {
        return None;
    }
    let terminal_layers: Vec<u32> = conn
        .points_to_connect
        .iter()
        .map(|pt| pt.layer.index(problem.layer_count))
        .collect::<Option<_>>()?;
    let mut best: Option<(RouteSolution, (usize, u64, u32, usize))> = None;
    for layer in candidate_layers(problem, &terminal_layers) {
        let layer_idx = layer.index(problem.layer_count)?;
        let mut seed = solution.clone();
        for (pt, &terminal_layer) in conn.points_to_connect.iter().zip(&terminal_layers) {
            push_via_if_layer_differs(
                problem,
                &mut seed,
                &conn.name,
                pt.point(),
                terminal_layer,
                layer_idx,
            );
        }
        let Some(candidate) = route_tree_on_layer(problem, &seed, conn, &layer) else {
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

fn route_tree_on_layer(
    problem: &RouteProblem,
    solution: &RouteSolution,
    conn: &crate::problem::Connection,
    layer: &LayerRef,
) -> Option<RouteSolution> {
    let mut best: Option<(RouteSolution, (usize, u64, u32, usize))> = None;
    for root in 0..conn.points_to_connect.len() {
        let Some(candidate) = route_tree_on_layer_from_root(problem, solution, conn, layer, root)
        else {
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
        let mut best_leg: Option<(usize, Option<Trace>, (u64, u32, usize, usize))> = None;
        for &from in &connected {
            for &to in &remaining {
                let Some((trace, key)) = route_leg_on_layer(
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
        if let Some(trace) = trace {
            candidate.traces.push(trace);
        }
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
) -> Option<(Option<Trace>, (u64, u32, usize, usize))> {
    if a.dist(b) < geom::EPS {
        return candidate_is_geometry_clean(problem, solution, &conn.name)
            .then_some((None, (0, 0, to, from)));
    }
    let mut best: Option<(Trace, (u64, u32, usize, usize))> = None;
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
        let key = trace_tree_key(&trace, from, to);
        best = match best.take() {
            None => Some((trace, key)),
            Some((incumbent, incumbent_key)) if incumbent_key <= key => {
                Some((incumbent, incumbent_key))
            }
            Some(_) => Some((trace, key)),
        };
    }
    best.map(|(trace, key)| (Some(trace), key))
}

fn solution_tree_key(before: &RouteSolution, after: &RouteSolution) -> (usize, u64, u32, usize) {
    let added = &after.traces[before.traces.len()..];
    let added_vias = after.vias.len().saturating_sub(before.vias.len());
    let length_um = added.iter().map(trace_length_um).sum();
    let bends = added.iter().map(trace_bends).sum();
    (added_vias, length_um, bends, added.len())
}

fn trace_tree_key(trace: &Trace, from: usize, to: usize) -> (u64, u32, usize, usize) {
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

fn mixed_layers(points: &[crate::problem::RoutePoint]) -> bool {
    points
        .first()
        .is_some_and(|first| points.iter().any(|pt| pt.layer != first.layer))
}

fn route_layer_via_count(terminal_layers: &[u32], route_layer: u32) -> usize {
    terminal_layers
        .iter()
        .filter(|&&terminal_layer| terminal_layer != route_layer)
        .count()
}

fn push_via_if_layer_differs(
    problem: &RouteProblem,
    solution: &mut RouteSolution,
    connection: &str,
    at: Point2,
    terminal_layer: u32,
    route_layer: u32,
) {
    if terminal_layer == route_layer
        || solution
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

fn candidate_layers(problem: &RouteProblem, terminal_layers: &[u32]) -> Vec<LayerRef> {
    let layer_count = problem.layer_count.max(1);
    let plane_layers: BTreeSet<u32> = crate::router::plane_layers(layer_count as usize)
        .into_iter()
        .collect();
    let mut layer_idxs = Vec::new();
    for &idx in terminal_layers {
        if idx < layer_count && !plane_layers.contains(&idx) && !layer_idxs.contains(&idx) {
            layer_idxs.push(idx);
        }
    }
    for idx in 0..layer_count {
        if plane_layers.contains(&idx) || layer_idxs.contains(&idx) {
            continue;
        }
        layer_idxs.push(idx);
    }
    layer_idxs.sort_by_key(|&idx| route_layer_via_count(terminal_layers, idx));
    layer_idxs
        .into_iter()
        .map(|idx| layer_ref(idx, layer_count))
        .collect()
}

fn net_order_portfolio(problem: &RouteProblem) -> Vec<Vec<usize>> {
    let base: Vec<usize> = (0..problem.connections.len()).collect();
    let mut orders = Vec::new();
    push_order(&mut orders, base.clone());

    let mut fewest_vias = base.clone();
    fewest_vias.sort_by(|&a, &b| {
        estimated_required_vias(problem, &problem.connections[a])
            .cmp(&estimated_required_vias(problem, &problem.connections[b]))
            .then_with(|| {
                connection_span_um(&problem.connections[a])
                    .cmp(&connection_span_um(&problem.connections[b]))
            })
            .then_with(|| {
                problem.connections[a]
                    .name
                    .cmp(&problem.connections[b].name)
            })
    });
    push_order(&mut orders, fewest_vias);

    let mut long_first = base;
    long_first.sort_by(|&a, &b| {
        connection_span_um(&problem.connections[b])
            .cmp(&connection_span_um(&problem.connections[a]))
            .then_with(|| {
                problem.connections[a]
                    .name
                    .cmp(&problem.connections[b].name)
            })
    });
    push_order(&mut orders, long_first);
    orders
}

fn push_order(orders: &mut Vec<Vec<usize>>, order: Vec<usize>) {
    if !orders.iter().any(|existing| existing == &order) {
        orders.push(order);
    }
}

fn estimated_required_vias(problem: &RouteProblem, conn: &crate::problem::Connection) -> usize {
    if !mixed_layers(&conn.points_to_connect) || conn.points_to_connect.len() < 2 {
        return usize::MAX;
    }
    let Some(terminal_layers) = conn
        .points_to_connect
        .iter()
        .map(|pt| pt.layer.index(problem.layer_count))
        .collect::<Option<Vec<_>>>()
    else {
        return usize::MAX;
    };
    candidate_layers(problem, &terminal_layers)
        .iter()
        .filter_map(|layer| layer.index(problem.layer_count))
        .map(|idx| route_layer_via_count(&terminal_layers, idx))
        .min()
        .unwrap_or(usize::MAX)
}

fn connection_span_um(conn: &crate::problem::Connection) -> u64 {
    let Some(first) = conn.points_to_connect.first() else {
        return 0;
    };
    let (mut min_x, mut max_x, mut min_y, mut max_y) = (first.x, first.x, first.y, first.y);
    for pt in &conn.points_to_connect {
        min_x = min_x.min(pt.x);
        max_x = max_x.max(pt.x);
        min_y = min_y.min(pt.y);
        max_y = max_y.max(pt.y);
    }
    (((max_x - min_x) + (max_y - min_y)) * 1000.0).round() as u64
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

fn route_quality(problem: &RouteProblem, result: &RouteResult) -> RouteQuality {
    RouteQuality::of(
        problem,
        result,
        crate::router::geometry_violations(problem, &result.solution),
    )
}

fn keep_candidate(incumbent: &RouteQuality, challenger: &RouteQuality) -> bool {
    if incumbent.faults() != challenger.faults() {
        incumbent.faults() < challenger.faults()
    } else if incumbent.failed_nets != challenger.failed_nets {
        incumbent.failed_nets < challenger.failed_nets
    } else if incumbent.via_count != challenger.via_count {
        incumbent.via_count < challenger.via_count
    } else {
        incumbent.wirelength <= challenger.wirelength
    }
}

fn candidate_is_geometry_clean(
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

fn reconcile(problem: &RouteProblem, solution: &mut RouteSolution, failed: &mut Vec<FailedNet>) {
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
                reason: "DRC oracle: layer-hop copper dropped (clearance/connectivity)".to_string(),
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

    fn base(obstacles: Vec<Obstacle>) -> RouteProblem {
        RouteProblem {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles,
            connections: vec![conn("N", &[(2.0, 10.0, "top"), (18.0, 10.0, "bottom")])],
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
    fn routes_different_layer_two_point_net_with_one_endpoint_via() {
        let p = base(vec![
            pad(&["N"], (2.0, 10.0), LayerRef::top()),
            pad(&["N"], (18.0, 10.0), LayerRef::bottom()),
        ]);
        let direct = crate::direct::route_direct(&p);
        assert!(
            !direct.failed.is_empty(),
            "direct router must refuse layer changes"
        );

        let r = route_layer_hop(&p);

        assert!(r.failed.is_empty(), "{:?}", r.failed);
        assert_eq!(r.solution.traces.len(), 1);
        assert_eq!(r.solution.vias.len(), 1);
        assert!(crate::lint::lint(&p, &r.solution).is_empty());
    }

    #[test]
    fn failed_priority_order_promotes_lower_via_failed_layer_hop_nets() {
        let mut p = base(vec![]);
        p.connections = vec![
            conn("BLOCKER", &[(2.0, 2.0, "top"), (4.0, 2.0, "bottom")]),
            conn("TWO_VIA", &[(2.0, 6.0, "top"), (8.0, 6.0, "bottom")]),
            conn(
                "ONE_VIA_BUS",
                &[
                    (2.0, 10.0, "top"),
                    (8.0, 10.0, "bottom"),
                    (12.0, 10.0, "bottom"),
                ],
            ),
            conn("TAIL", &[(2.0, 14.0, "top"), (4.0, 14.0, "bottom")]),
        ];
        let failed = vec![
            FailedNet {
                connection: "TWO_VIA".to_owned(),
                reason: "blocked".to_owned(),
            },
            FailedNet {
                connection: "ONE_VIA_BUS".to_owned(),
                reason: "blocked".to_owned(),
            },
        ];

        let retry = failed_priority_order(&p, &[0, 1, 2, 3], &failed);

        assert_eq!(
            &retry[..2],
            &[2, 1],
            "failed net with a one-via common layer should claim the retry before a two-via net"
        );
        assert_eq!(&retry[2..], &[0, 3]);
    }

    #[test]
    fn routes_same_xy_layer_change_as_via_only() {
        let mut p = base(vec![
            pad(&["N"], (10.0, 10.0), LayerRef::top()),
            pad(&["N"], (10.0, 10.0), LayerRef::bottom()),
        ]);
        p.connections = vec![conn("N", &[(10.0, 10.0, "top"), (10.0, 10.0, "bottom")])];

        let r = route_layer_hop(&p);

        assert!(r.failed.is_empty(), "{:?}", r.failed);
        assert!(r.solution.traces.is_empty());
        assert_eq!(r.solution.vias.len(), 1);
        assert!(crate::lint::lint(&p, &r.solution).is_empty());
    }

    #[test]
    fn escapes_blocked_endpoint_layer_by_routing_on_other_terminal_layer() {
        let p = base(vec![
            pad(&["N"], (2.0, 10.0), LayerRef::top()),
            pad(&["N"], (18.0, 10.0), LayerRef::bottom()),
            wall(vec![LayerRef::top()]),
        ]);

        let r = route_layer_hop(&p);

        assert!(r.failed.is_empty(), "{:?}", r.failed);
        assert_eq!(r.solution.traces.len(), 1);
        assert_eq!(r.solution.traces[0].layer, LayerRef::bottom());
        assert_eq!(r.solution.vias.len(), 1);
        assert!(crate::lint::lint(&p, &r.solution).is_empty());
    }

    #[test]
    fn routes_mixed_layer_multi_point_star_on_majority_layer() {
        let mut p = base(vec![
            pad(&["N"], (2.0, 10.0), LayerRef::top()),
            pad(&["N"], (18.0, 10.0), LayerRef::bottom()),
            pad(&["N"], (18.0, 14.0), LayerRef::bottom()),
        ]);
        p.connections = vec![conn(
            "N",
            &[
                (2.0, 10.0, "top"),
                (18.0, 10.0, "bottom"),
                (18.0, 14.0, "bottom"),
            ],
        )];
        let direct = crate::direct::route_direct(&p);
        assert!(
            !direct.failed.is_empty(),
            "direct router must refuse mixed-layer stars"
        );

        let r = route_layer_hop(&p);

        assert!(r.failed.is_empty(), "{:?}", r.failed);
        assert_eq!(r.solution.traces.len(), 2);
        assert!(
            r.solution
                .traces
                .iter()
                .all(|t| t.layer == LayerRef::bottom()),
            "majority bottom terminals should make bottom the tidy star layer: {:?}",
            r.solution.traces
        );
        assert_eq!(r.solution.vias.len(), 1);
        assert!(crate::lint::lint(&p, &r.solution).is_empty());
    }

    #[test]
    fn multi_point_tree_uses_short_chain_instead_of_fixed_root_star() {
        let mut p = base(vec![
            pad(&["N"], (2.0, 10.0), LayerRef::top()),
            pad(&["N"], (10.0, 10.0), LayerRef::bottom()),
            pad(&["N"], (18.0, 10.0), LayerRef::bottom()),
        ]);
        p.connections = vec![conn(
            "N",
            &[
                (2.0, 10.0, "top"),
                (10.0, 10.0, "bottom"),
                (18.0, 10.0, "bottom"),
            ],
        )];

        let r = route_layer_hop(&p);

        assert!(r.failed.is_empty(), "{:?}", r.failed);
        assert_eq!(r.solution.traces.len(), 2);
        assert_eq!(r.solution.vias.len(), 1);
        assert!(
            (r.solution.metrics().wirelength - 16.0).abs() < 1e-9,
            "layer-hop tree should avoid the 24mm fixed-root star: {:?}",
            r.solution
        );
        assert!(crate::lint::lint(&p, &r.solution).is_empty());
    }

    #[test]
    fn multi_point_tree_prefers_fewer_terminal_vias_over_shorter_layer() {
        let mut p = base(vec![
            pad(&["N"], (2.0, 10.0), LayerRef::top()),
            pad(&["N"], (18.0, 10.0), LayerRef::bottom()),
            pad(&["N"], (18.0, 14.0), LayerRef::bottom()),
            Obstacle {
                kind: "rect".to_owned(),
                layers: vec![LayerRef::bottom()],
                center: Point2 { x: 10.0, y: 10.0 },
                width: 4.0,
                height: 4.0,
                connected_to: vec![],
            },
        ]);
        p.connections = vec![conn(
            "N",
            &[
                (2.0, 10.0, "top"),
                (18.0, 10.0, "bottom"),
                (18.0, 14.0, "bottom"),
            ],
        )];

        let r = route_layer_hop(&p);

        assert!(r.failed.is_empty(), "{:?}", r.failed);
        assert_eq!(
            r.solution.vias.len(),
            1,
            "one-via bottom-layer detour should beat a shorter two-via top-layer route"
        );
        assert!(
            r.solution
                .traces
                .iter()
                .all(|t| t.layer == LayerRef::bottom()),
            "expected bottom-layer detour, got {:?}",
            r.solution
        );
        assert!(crate::lint::lint(&p, &r.solution).is_empty());
    }

    #[test]
    fn route_leg_uses_existing_route_layer_copper_axes_for_local_detour() {
        let mut p = base(vec![]);
        p.connections = vec![
            conn("A", &[(4.0, 10.0, "bottom"), (16.0, 10.0, "bottom")]),
            conn("B", &[(10.0, 8.0, "top"), (10.0, 12.0, "bottom")]),
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

        let (trace, _) = route_leg_on_layer(
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
        let trace = trace.expect("points are distinct, so a trace is required");

        assert!(
            trace_length_um(&trace) < 19_000,
            "layer-hop leg should detour locally around prior copper even when an unrelated fat net exists: {:?}",
            trace.path
        );
        assert!(
            trace
                .path
                .iter()
                .any(|p| (p.x - 3.6).abs() < 0.05 || (p.x - 16.4).abs() < 0.05),
            "detour should use signal-width local axes generated from the existing route-layer trace: {:?}",
            trace.path
        );
        solution.traces.push(trace);
        assert_eq!(crate::router::geometry_violations(&p, &solution), 0);
    }

    #[test]
    fn reports_same_layer_net_without_copper() {
        let mut p = base(vec![
            pad(&["N"], (2.0, 10.0), LayerRef::top()),
            pad(&["N"], (18.0, 10.0), LayerRef::top()),
        ]);
        p.connections = vec![conn("N", &[(2.0, 10.0, "top"), (18.0, 10.0, "top")])];

        let r = route_layer_hop(&p);

        assert_eq!(r.failed.len(), 1);
        assert!(r.solution.traces.is_empty());
        assert!(r.solution.vias.is_empty());
    }
}
