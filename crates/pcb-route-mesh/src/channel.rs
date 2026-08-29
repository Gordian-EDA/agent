//! Directional channel router.
//!
//! This is a narrow, non-A* portfolio member that borrows the classic PCB
//! preferred-direction idea: horizontal spans prefer one signal layer, vertical
//! spans prefer another, with vias only at terminal/bend transitions. It is meant
//! to catch simple crossing/channel cases before the detailed negotiated mesh or
//! the grid fallback are paid. Multi-pin nets grow a deterministic nearest-tree
//! over the same preferred-direction leg candidates.

use crate::heuristics::{
    connection_crossing_pressures, connection_segment_obstacle_pressure_um, connection_span_um,
};
use crate::quality::{
    keep_route_candidate as keep_candidate, route_quality, trace_proximity_penalty_um,
    trace_route_cost_um,
};
use pcb_model::{
    Capabilities, Connection, FailedNet, LayerRef, Point2, RoutePoint, RouteProblem, RouteQuality,
    RouteResult, RouteSolution, Router, Trace, Via, ViaSpan,
};
use std::collections::BTreeSet;

/// This engine's [`RouteResult::engine`] provenance tag.
pub const ENGINE: &str = "channel";
const CHANNEL_MAX_CONNECTIONS: usize = 6;
const CHANNEL_MAX_MULTILAYER_CONNECTIONS: usize = 5;
type ChannelLegKey = (usize, u64, u64, usize, usize);
type ChannelBestLeg = (usize, RouteSolution, ChannelLegKey);

/// Deterministic preferred-direction router for simple channel-shaped nets.
#[derive(Debug, Clone, Copy, Default)]
pub struct ChannelRouter;

impl Router for ChannelRouter {
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
            && problem.connections.len() <= CHANNEL_MAX_CONNECTIONS
            && (problem.layer_count <= 2
                || problem.connections.len() <= CHANNEL_MAX_MULTILAYER_CONNECTIONS)
    }

    fn route(&self, problem: &RouteProblem) -> RouteResult {
        route_channel(problem)
    }
}

/// Route eligible two-pin nets with preferred horizontal/vertical signal layers.
pub fn route_channel(problem: &RouteProblem) -> RouteResult {
    let mut best: Option<(RouteResult, RouteQuality)> = None;
    for order in net_order_portfolio(problem) {
        let result = route_order(problem, &order);
        let q = route_quality(problem, &result);
        best = match best.take() {
            None => Some((result, q)),
            Some((bi, bq)) if keep_candidate(&bq, &q) => Some((bi, bq)),
            Some(_) => Some((result, q)),
        };
    }
    best.map(|(result, _)| result)
        .unwrap_or_else(|| route_order(problem, &[]))
}

fn route_order(problem: &RouteProblem, order: &[usize]) -> RouteResult {
    let mut best = route_order_once(problem, order);
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

        let candidate = route_order_once(problem, &retry_order);
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

fn route_order_once(problem: &RouteProblem, order: &[usize]) -> RouteResult {
    let mut solution = RouteSolution {
        traces: Vec::new(),
        vias: Vec::new(),
    };
    let mut failed = Vec::new();
    let mut routed = Vec::new();

    for &idx in order {
        let Some(conn) = problem.connections.get(idx) else {
            continue;
        };
        match conn.points_to_connect.as_slice() {
            [] | [_] => {}
            [_, _] => {
                if let Some(candidate) = route_two_pin(problem, &solution, &routed, idx) {
                    solution = candidate;
                    routed.push(idx);
                } else {
                    failed.push(FailedNet {
                        connection: conn.name.clone(),
                        reason: "channel router found no clean preferred-direction route"
                            .to_owned(),
                    });
                }
            }
            _ => {
                if let Some(candidate) = route_multi_pin(problem, &solution, &routed, idx) {
                    solution = candidate;
                    routed.push(idx);
                } else {
                    failed.push(FailedNet {
                        connection: conn.name.clone(),
                        reason: "channel router found no clean preferred-direction tree".to_owned(),
                    });
                }
            }
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
    metrics: &[ChannelOrderMetric],
    a: usize,
    b: usize,
) -> std::cmp::Ordering {
    metrics[b]
        .crossing_pressure
        .cmp(&metrics[a].crossing_pressure)
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
}

fn route_two_pin(
    problem: &RouteProblem,
    solution: &RouteSolution,
    routed: &[usize],
    idx: usize,
) -> Option<RouteSolution> {
    let conn = problem.connections.get(idx)?;
    let mut candidates: Vec<(RouteSolution, (usize, u64, u64, usize))> = Vec::new();
    for mut candidate in two_pin_candidates(problem, solution, conn) {
        let validation = problem_with_connections_and_extra(problem, routed, idx);
        crate::via_cleanup::normalize_redundant_vias(&validation, &mut candidate);
        if !pcb_drc::lint::lint(&validation, &candidate).is_empty() {
            continue;
        }
        let key = solution_tree_key(problem, solution, &candidate);
        candidates.push((candidate, key));
    }
    candidates.sort_by(|(_, a), (_, b)| a.cmp(b));
    candidates.into_iter().next().map(|(solution, _)| solution)
}

fn route_multi_pin(
    problem: &RouteProblem,
    solution: &RouteSolution,
    routed: &[usize],
    idx: usize,
) -> Option<RouteSolution> {
    let conn = problem.connections.get(idx)?;
    let validation = problem_with_connections_and_extra(problem, routed, idx);
    let mut best: Option<(RouteSolution, (usize, u64, u64, usize))> = None;

    for root in 0..conn.points_to_connect.len() {
        let Some(candidate) = route_multi_pin_from_root(problem, &validation, solution, conn, root)
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

fn route_multi_pin_from_root(
    problem: &RouteProblem,
    validation: &RouteProblem,
    solution: &RouteSolution,
    conn: &Connection,
    root: usize,
) -> Option<RouteSolution> {
    let mut candidate = solution.clone();
    let mut connected = vec![root];
    let mut remaining: BTreeSet<usize> = (0..conn.points_to_connect.len())
        .filter(|idx| *idx != root)
        .collect();

    while !remaining.is_empty() {
        let mut best_leg: Option<ChannelBestLeg> = None;
        for &from in &connected {
            for &to in &remaining {
                for leg in point_pair_candidates(
                    problem,
                    &candidate,
                    &conn.name,
                    &conn.points_to_connect[from],
                    &conn.points_to_connect[to],
                ) {
                    if !geometry_clean(validation, &leg) {
                        continue;
                    }
                    let key = leg_key(problem, &candidate, &leg, from, to);
                    best_leg = match best_leg.take() {
                        None => Some((to, leg, key)),
                        Some((best_to, best_solution, best_key)) if best_key <= key => {
                            Some((best_to, best_solution, best_key))
                        }
                        Some(_) => Some((to, leg, key)),
                    };
                }
            }
        }
        let (to, leg, _) = best_leg?;
        candidate = leg;
        connected.push(to);
        remaining.remove(&to);
    }

    pcb_drc::lint::lint(validation, &candidate)
        .is_empty()
        .then_some(candidate)
}

fn two_pin_candidates(
    problem: &RouteProblem,
    solution: &RouteSolution,
    conn: &Connection,
) -> Vec<RouteSolution> {
    point_pair_candidates(
        problem,
        solution,
        &conn.name,
        &conn.points_to_connect[0],
        &conn.points_to_connect[1],
    )
}

fn point_pair_candidates(
    problem: &RouteProblem,
    solution: &RouteSolution,
    connection: &str,
    a: &RoutePoint,
    b: &RoutePoint,
) -> Vec<RouteSolution> {
    let start_layer = a.layer.index(problem.layer_count);
    let end_layer = b.layer.index(problem.layer_count);
    let Some((horizontal, vertical)) = preferred_layers(problem) else {
        return Vec::new();
    };

    let start = a.point();
    let end = b.point();
    let bends = [
        Point2 {
            x: end.x,
            y: start.y,
        },
        Point2 {
            x: start.x,
            y: end.y,
        },
    ];
    let mut candidates = Vec::new();
    let mut seen = BTreeSet::new();

    for bend in bends {
        let segments = [
            (
                start,
                bend,
                segment_layer(start, bend, &horizontal, &vertical),
            ),
            (bend, end, segment_layer(bend, end, &horizontal, &vertical)),
        ];
        push_candidate(
            problem,
            solution,
            connection,
            a,
            b,
            &segments,
            &mut candidates,
            &mut seen,
        );
    }

    if let (Some(start_idx), Some(end_idx)) = (start_layer, end_layer) {
        let start_ref = layer_ref_from_index(start_idx, problem.layer_count);
        for bend in bends {
            let segments = [
                (start, bend, start_ref.clone()),
                (bend, end, start_ref.clone()),
            ];
            push_candidate(
                problem,
                solution,
                connection,
                a,
                b,
                &segments,
                &mut candidates,
                &mut seen,
            );
        }
        if start_idx != end_idx {
            let end_ref = layer_ref_from_index(end_idx, problem.layer_count);
            for bend in bends {
                let segments = [(start, bend, end_ref.clone()), (bend, end, end_ref.clone())];
                push_candidate(
                    problem,
                    solution,
                    connection,
                    a,
                    b,
                    &segments,
                    &mut candidates,
                    &mut seen,
                );
            }
        }
    }

    candidates
}

#[allow(clippy::too_many_arguments)]
fn push_candidate(
    problem: &RouteProblem,
    solution: &RouteSolution,
    connection: &str,
    a: &RoutePoint,
    b: &RoutePoint,
    segments: &[(Point2, Point2, LayerRef)],
    candidates: &mut Vec<RouteSolution>,
    seen: &mut BTreeSet<SolutionKey>,
) {
    let Some(candidate) = build_segment_candidate(problem, solution, connection, a, b, segments)
    else {
        return;
    };
    let key = solution_key(&candidate, solution);
    if seen.insert(key) {
        candidates.push(candidate);
    }
}

fn build_segment_candidate(
    problem: &RouteProblem,
    solution: &RouteSolution,
    connection: &str,
    a: &RoutePoint,
    b: &RoutePoint,
    segments: &[(Point2, Point2, LayerRef)],
) -> Option<RouteSolution> {
    let start_layer = a.layer.index(problem.layer_count)?;
    let target_layer = b.layer.index(problem.layer_count)?;
    let mut candidate = solution.clone();
    let mut current_layer = start_layer;
    let width = problem.net_width(connection);
    let mut emitted = false;

    for &(from, to, ref layer) in segments {
        if from.dist(to) < geom::EPS {
            continue;
        }
        let segment_layer = layer.index(problem.layer_count)?;
        if current_layer != segment_layer {
            candidate.vias.push(Via {
                connection: connection.to_owned(),
                at: from,
                diameter: problem.via_diameter,
                drill: problem.via_drill,
                span: ViaSpan::Through,
            });
            current_layer = segment_layer;
        }
        candidate.traces.push(Trace {
            connection: connection.to_owned(),
            layer: layer.clone(),
            width,
            path: vec![from, to],
        });
        emitted = true;
    }

    if !emitted {
        return None;
    }

    if current_layer != target_layer {
        candidate.vias.push(Via {
            connection: connection.to_owned(),
            at: b.point(),
            diameter: problem.via_diameter,
            drill: problem.via_drill,
            span: ViaSpan::Through,
        });
    }
    Some(candidate)
}

fn solution_tree_key(
    problem: &RouteProblem,
    before: &RouteSolution,
    after: &RouteSolution,
) -> (usize, u64, u64, usize) {
    let trace_base = before.traces.len();
    let via_base = before.vias.len();
    let added = &after.traces[trace_base..];
    let wire_um: u64 = added.iter().map(trace_length_um).sum();
    let proximity_um = added
        .iter()
        .map(|trace| trace_proximity_penalty_um(problem, before, trace))
        .sum::<u64>();
    let vias = after.vias[via_base..].len();
    let traces = added.len();
    (vias, wire_um.saturating_add(proximity_um), wire_um, traces)
}

fn leg_key(
    problem: &RouteProblem,
    before: &RouteSolution,
    after: &RouteSolution,
    from: usize,
    to: usize,
) -> (usize, u64, u64, usize, usize) {
    let trace_base = before.traces.len();
    let via_base = before.vias.len();
    let added = &after.traces[trace_base..];
    let wire_um: u64 = added.iter().map(trace_length_um).sum();
    let route_cost_um = added
        .iter()
        .map(|trace| trace_route_cost_um(problem, before, trace, trace_length_um(trace)))
        .sum::<u64>();
    let vias = after.vias[via_base..].len();
    (vias, route_cost_um, wire_um, to, from)
}

fn trace_length_um(trace: &Trace) -> u64 {
    (trace.path.windows(2).map(|w| w[1].dist(w[0])).sum::<f64>() * 1000.0).round() as u64
}

fn geometry_clean(problem: &RouteProblem, solution: &RouteSolution) -> bool {
    pcb_drc::lint::lint(problem, solution)
        .iter()
        .all(|v| matches!(v, pcb_drc::lint::DrcViolation::Connectivity { .. }))
}

fn segment_layer(a: Point2, b: Point2, horizontal: &LayerRef, vertical: &LayerRef) -> LayerRef {
    if (a.y - b.y).abs() <= (a.x - b.x).abs() {
        horizontal.clone()
    } else {
        vertical.clone()
    }
}

fn preferred_layers(problem: &RouteProblem) -> Option<(LayerRef, LayerRef)> {
    let signal = signal_layers(problem);
    let horizontal = signal
        .iter()
        .find(|&&(idx, _)| idx == 0)
        .or_else(|| signal.first())?
        .1
        .clone();
    let vertical = signal
        .iter()
        .find(|&&(idx, _)| idx + 1 == problem.layer_count)
        .or_else(|| signal.iter().find(|(_, layer)| *layer != horizontal))
        .map(|(_, layer)| layer.clone())
        .unwrap_or_else(|| horizontal.clone());
    Some((horizontal, vertical))
}

fn signal_layers(problem: &RouteProblem) -> Vec<(u32, LayerRef)> {
    let planes: BTreeSet<u32> = pcb_route_grid::router::plane_layers(problem.layer_count as usize)
        .into_iter()
        .collect();
    (0..problem.layer_count)
        .filter(|idx| !planes.contains(idx))
        .map(|idx| (idx, layer_ref_from_index(idx, problem.layer_count)))
        .collect()
}

fn layer_ref_from_index(idx: u32, layer_count: u32) -> LayerRef {
    if idx == 0 {
        LayerRef::top()
    } else if idx + 1 == layer_count {
        LayerRef::bottom()
    } else {
        LayerRef(format!("inner{idx}"))
    }
}

fn net_order_portfolio(problem: &RouteProblem) -> Vec<Vec<usize>> {
    let base: Vec<usize> = (0..problem.connections.len()).collect();
    let metrics = net_order_metrics(problem);
    let mut orders = Vec::new();
    push_order(&mut orders, base.clone());

    let mut short_first = base.clone();
    short_first.sort_by(|&a, &b| {
        metrics[a].span_um.cmp(&metrics[b].span_um).then_with(|| {
            problem.connections[a]
                .name
                .cmp(&problem.connections[b].name)
        })
    });
    push_order(&mut orders, short_first);

    let mut long_first = base.clone();
    long_first.sort_by(|&a, &b| {
        metrics[b].span_um.cmp(&metrics[a].span_um).then_with(|| {
            problem.connections[a]
                .name
                .cmp(&problem.connections[b].name)
        })
    });
    push_order(&mut orders, long_first);

    let mut segment_crowded_first = base.clone();
    segment_crowded_first.sort_by(|&a, &b| {
        metrics[b]
            .segment_obstacle_pressure_um
            .cmp(&metrics[a].segment_obstacle_pressure_um)
            .then_with(|| metrics[b].span_um.cmp(&metrics[a].span_um))
            .then_with(|| {
                problem.connections[a]
                    .name
                    .cmp(&problem.connections[b].name)
            })
    });
    push_order(&mut orders, segment_crowded_first);

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
struct ChannelOrderMetric {
    span_um: u64,
    segment_obstacle_pressure_um: u64,
    crossing_pressure: usize,
}

fn net_order_metrics(problem: &RouteProblem) -> Vec<ChannelOrderMetric> {
    let crossing_pressures = connection_crossing_pressures(problem);
    problem
        .connections
        .iter()
        .enumerate()
        .map(|(idx, conn)| ChannelOrderMetric {
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

fn problem_with_connections_and_extra(
    problem: &RouteProblem,
    indices: &[usize],
    extra: usize,
) -> RouteProblem {
    let mut all = indices.to_vec();
    all.push(extra);
    let mut out = problem.clone();
    out.connections = all
        .into_iter()
        .filter_map(|idx| problem.connections.get(idx).cloned())
        .collect();
    out
}

fn reconcile(problem: &RouteProblem, solution: &mut RouteSolution, failed: &mut Vec<FailedNet>) {
    crate::via_cleanup::normalize_redundant_vias(problem, solution);
    let mut dropped = pcb_drc::lint::drop_violating_copper(problem, solution);
    dropped.extend(pcb_drc::lint::drop_unconnected_copper(problem, solution));
    let known: BTreeSet<String> = failed.iter().map(|f| f.connection.clone()).collect();
    let mut seen = BTreeSet::new();
    failed.extend(
        dropped
            .into_iter()
            .filter(|name| !known.contains(name) && seen.insert(name.clone()))
            .map(|connection| FailedNet {
                connection,
                reason: "DRC oracle: channel route dropped".to_owned(),
            }),
    );
}

type TraceKey = (String, String, Vec<(i64, i64)>);
type ViaKey = (String, (i64, i64), (u32, u32, bool));
type SolutionKey = (Vec<TraceKey>, Vec<ViaKey>);

fn solution_key(candidate: &RouteSolution, before: &RouteSolution) -> SolutionKey {
    let trace_base = before.traces.len();
    let via_base = before.vias.len();
    let traces = candidate.traces[trace_base..]
        .iter()
        .map(|trace| {
            (
                trace.connection.clone(),
                trace.layer.0.clone(),
                trace
                    .path
                    .iter()
                    .map(|p| (quantize_mm(p.x), quantize_mm(p.y)))
                    .collect(),
            )
        })
        .collect();
    let vias = candidate.vias[via_base..]
        .iter()
        .map(|via| {
            (
                via.connection.clone(),
                (quantize_mm(via.at.x), quantize_mm(via.at.y)),
                via_span_key(&via.span),
            )
        })
        .collect();
    (traces, vias)
}

fn via_span_key(span: &ViaSpan) -> (u32, u32, bool) {
    match span {
        ViaSpan::Through => (0, u32::MAX, false),
        ViaSpan::Partial { from, to, micro } => (*from, *to, *micro),
    }
}

fn quantize_mm(v: f64) -> i64 {
    (v * 1000.0).round() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcb_model::{Obstacle, Rect, RoutePoint};

    fn conn(name: &str, pts: &[(f64, f64, &str)]) -> Connection {
        Connection {
            name: name.to_owned(),
            points_to_connect: pts
                .iter()
                .map(|&(x, y, layer)| RoutePoint {
                    x,
                    y,
                    layer: LayerRef(layer.to_owned()),
                })
                .collect(),
        }
    }

    fn pad(net: &str, at: (f64, f64)) -> Obstacle {
        Obstacle {
            kind: "rect".to_owned(),
            layers: vec![LayerRef::top()],
            center: Point2 { x: at.0, y: at.1 },
            width: 0.6,
            height: 0.6,
            connected_to: vec![net.to_owned()],
        }
    }

    fn keepout(at: (f64, f64)) -> Obstacle {
        Obstacle {
            kind: "rect".to_owned(),
            layers: vec![LayerRef::top()],
            center: Point2 { x: at.0, y: at.1 },
            width: 0.5,
            height: 0.5,
            connected_to: vec![],
        }
    }

    fn problem(connections: Vec<Connection>) -> RouteProblem {
        let mut obstacles = Vec::new();
        for conn in &connections {
            for pt in &conn.points_to_connect {
                obstacles.push(pad(&conn.name, (pt.x, pt.y)));
            }
        }
        RouteProblem {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles,
            connections,
            bounds: Rect {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 20.0,
                max_y: 20.0,
            },
            clearance: 0.2,
            via_diameter: 0.6,
            via_drill: 0.3,
            net_widths: Default::default(),
            outline: None,
            escape_layers: Default::default(),
            plane_nets: Default::default(),
        }
    }

    #[test]
    fn channel_routes_crossing_same_layer_nets_on_direction_layers() {
        let p = problem(vec![
            conn("H", &[(2.0, 10.0, "top"), (18.0, 10.0, "top")]),
            conn("V", &[(10.0, 2.0, "top"), (10.0, 18.0, "top")]),
        ]);

        let result = route_channel(&p);

        assert!(result.failed.is_empty(), "{:?}", result.failed);
        assert_eq!(result.engine, ENGINE);
        assert!(pcb_drc::lint::lint(&p, &result.solution).is_empty());
        assert!(
            result
                .solution
                .traces
                .iter()
                .any(|trace| trace.connection == "V" && trace.layer == LayerRef::bottom()),
            "vertical crossing net should use the preferred vertical layer: {:?}",
            result.solution
        );
        assert!(
            result
                .solution
                .vias
                .iter()
                .filter(|via| via.connection == "V")
                .count()
                >= 2,
            "same-layer top terminal routed on bottom needs terminal vias"
        );
    }

    #[test]
    fn channel_routes_clean_multi_pin_tree() {
        let p = problem(vec![conn(
            "STAR",
            &[(2.0, 2.0, "top"), (8.0, 2.0, "top"), (8.0, 8.0, "top")],
        )]);

        let result = route_channel(&p);

        assert!(result.failed.is_empty(), "{:?}", result.failed);
        assert_eq!(result.engine, ENGINE);
        assert!(pcb_drc::lint::lint(&p, &result.solution).is_empty());
        assert!(
            result
                .solution
                .traces
                .iter()
                .filter(|trace| trace.connection == "STAR")
                .count()
                >= 2,
            "multi-pin channel tree should emit multiple clean legs: {:?}",
            result.solution
        );
    }

    #[test]
    fn channel_declines_large_two_layer_portfolios() {
        let p = problem(
            (0..=CHANNEL_MAX_CONNECTIONS)
                .map(|idx| {
                    conn(
                        &format!("N{idx}"),
                        &[
                            (1.0, idx as f64 + 1.0, "top"),
                            (5.0, idx as f64 + 1.0, "top"),
                        ],
                    )
                })
                .collect(),
        );

        assert!(!ChannelRouter.can_route(&p));
    }

    #[test]
    fn failed_priority_order_promotes_failed_crossing_channel_nets() {
        let p = problem(vec![
            conn("OPEN", &[(1.0, 1.0, "top"), (6.0, 1.0, "top")]),
            conn("CROSS_A", &[(2.0, 10.0, "top"), (18.0, 10.0, "top")]),
            conn("CROSS_B", &[(10.0, 2.0, "top"), (10.0, 18.0, "top")]),
            conn("LONG", &[(1.0, 18.0, "top"), (19.0, 18.0, "top")]),
        ]);
        let failed = vec![
            FailedNet {
                connection: "OPEN".to_owned(),
                reason: "blocked".to_owned(),
            },
            FailedNet {
                connection: "CROSS_A".to_owned(),
                reason: "blocked".to_owned(),
            },
        ];

        let retry = failed_priority_order(&p, &[0, 1, 2, 3], &failed);

        assert_eq!(retry, vec![1, 0, 2, 3]);
    }

    #[test]
    fn failed_priority_order_uses_segment_pressure_after_crossing_pressure() {
        let mut p = problem(vec![
            conn("BBOX_ONLY", &[(1.0, 3.0, "top"), (9.0, 9.0, "top")]),
            conn("SEGMENT_BLOCKED", &[(1.0, 1.0, "top"), (9.0, 1.0, "top")]),
            conn("TAIL", &[(1.0, 11.0, "top"), (4.0, 11.0, "top")]),
        ]);
        p.obstacles.push(keepout((8.0, 4.0)));
        p.obstacles.push(keepout((5.0, 1.0)));
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
            "channel retry should prefer true segment-corridor blockage once crossing pressure ties"
        );
        assert_eq!(&retry[2..], &[2]);
    }

    #[test]
    fn net_order_portfolio_includes_segment_obstacle_pressure_order() {
        let mut p = problem(vec![
            conn("BBOX_ONLY", &[(1.0, 3.0, "top"), (9.0, 9.0, "top")]),
            conn("SEGMENT_BLOCKED", &[(1.0, 1.0, "top"), (9.0, 1.0, "top")]),
            conn("TAIL", &[(1.0, 11.0, "top"), (4.0, 11.0, "top")]),
        ]);
        p.obstacles.push(keepout((8.0, 4.0)));
        p.obstacles.push(keepout((5.0, 1.0)));
        let metrics = net_order_metrics(&p);
        let orders = net_order_portfolio(&p);

        assert_eq!(metrics[0].segment_obstacle_pressure_um, 0);
        assert!(metrics[1].segment_obstacle_pressure_um > 0);
        assert!(
            orders.iter().any(|order| order.as_slice() == [1, 0, 2]),
            "channel portfolio should include segment-obstacle-pressure-first order: {orders:?}"
        );
    }

    #[test]
    fn two_pin_candidate_key_prefers_roomier_l_bend() {
        let mut p = problem(vec![conn("N", &[(2.0, 10.0, "top"), (18.0, 12.0, "top")])]);
        p.obstacles.push(keepout((10.0, 9.2)));

        let selected = route_two_pin(
            &p,
            &RouteSolution {
                traces: Vec::new(),
                vias: Vec::new(),
            },
            &[],
            0,
        )
        .expect("both L-bend candidates should be legal");

        assert!(
            selected.traces.iter().any(|trace| trace.path
                == vec![Point2 { x: 2.0, y: 10.0 }, Point2 { x: 2.0, y: 12.0 }]
                || trace.path == vec![Point2 { x: 2.0, y: 12.0 }, Point2 { x: 18.0, y: 12.0 }]),
            "channel should choose the L-bend away from the tight keepout margin: {:?}",
            selected.traces
        );
        assert!(pcb_drc::lint::lint(&p, &selected).is_empty());
    }
}
