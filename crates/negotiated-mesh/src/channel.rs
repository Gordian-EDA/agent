//! Directional channel router.
//!
//! This is a narrow, non-A* portfolio member that borrows the classic PCB
//! preferred-direction idea: horizontal spans prefer one signal layer, vertical
//! spans prefer another, with vias only at terminal/bend transitions. It is meant
//! to catch simple two-pin crossing/channel cases before the detailed negotiated
//! mesh or the grid fallback are paid.

use crate::problem::{
    Capabilities, Connection, FailedNet, LayerRef, Point2, RouteProblem, RouteQuality, RouteResult,
    RouteSolution, Router, Trace, Via, ViaSpan,
};
use std::collections::BTreeSet;

/// This engine's [`RouteResult::engine`] provenance tag.
pub const ENGINE: &str = "channel";

/// Deterministic preferred-direction router for two-pin nets.
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

        let candidate = route_order_once(problem, &retry_order);
        let incumbent_quality = route_quality(problem, &best);
        let candidate_quality = route_quality(problem, &candidate);
        if keep_candidate(&incumbent_quality, &candidate_quality) {
            break;
        }
        best = candidate;
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
            _ => failed.push(FailedNet {
                connection: conn.name.clone(),
                reason: "channel router only handles two-pin nets".to_owned(),
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
    connection_crossing_pressure(problem, b)
        .cmp(&connection_crossing_pressure(problem, a))
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

fn route_two_pin(
    problem: &RouteProblem,
    solution: &RouteSolution,
    routed: &[usize],
    idx: usize,
) -> Option<RouteSolution> {
    let conn = problem.connections.get(idx)?;
    let mut candidates: Vec<(RouteSolution, RouteQuality)> = Vec::new();
    for candidate in two_pin_candidates(problem, solution, conn) {
        let validation = problem_with_connections_and_extra(problem, routed, idx);
        if !crate::lint::lint(&validation, &candidate).is_empty() {
            continue;
        }
        let result = RouteResult {
            solution: candidate.clone(),
            failed: Vec::new(),
            engine: ENGINE.to_owned(),
        };
        candidates.push((candidate, RouteQuality::of(&validation, &result, 0)));
    }
    candidates.sort_by(|(_, a), (_, b)| compare_quality(a, b));
    candidates.into_iter().next().map(|(solution, _)| solution)
}

fn two_pin_candidates(
    problem: &RouteProblem,
    solution: &RouteSolution,
    conn: &Connection,
) -> Vec<RouteSolution> {
    let a = &conn.points_to_connect[0];
    let b = &conn.points_to_connect[1];
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
            conn,
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
                conn,
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
                    conn,
                    &segments,
                    &mut candidates,
                    &mut seen,
                );
            }
        }
    }

    candidates
}

fn push_candidate(
    problem: &RouteProblem,
    solution: &RouteSolution,
    conn: &Connection,
    segments: &[(Point2, Point2, LayerRef)],
    candidates: &mut Vec<RouteSolution>,
    seen: &mut BTreeSet<SolutionKey>,
) {
    let Some(candidate) = build_segment_candidate(problem, solution, conn, segments) else {
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
    conn: &Connection,
    segments: &[(Point2, Point2, LayerRef)],
) -> Option<RouteSolution> {
    let start_layer = conn.points_to_connect[0].layer.index(problem.layer_count)?;
    let target_layer = conn.points_to_connect[1].layer.index(problem.layer_count)?;
    let mut candidate = solution.clone();
    let mut current_layer = start_layer;
    let width = problem.net_width(&conn.name);
    let mut emitted = false;

    for &(from, to, ref layer) in segments {
        if from.dist(to) < geom::EPS {
            continue;
        }
        let segment_layer = layer.index(problem.layer_count)?;
        if current_layer != segment_layer {
            candidate.vias.push(Via {
                connection: conn.name.clone(),
                at: from,
                diameter: problem.via_diameter,
                drill: problem.via_drill,
                span: ViaSpan::Through,
            });
            current_layer = segment_layer;
        }
        candidate.traces.push(Trace {
            connection: conn.name.clone(),
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
            connection: conn.name.clone(),
            at: conn.points_to_connect[1].point(),
            diameter: problem.via_diameter,
            drill: problem.via_drill,
            span: ViaSpan::Through,
        });
    }
    Some(candidate)
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
    let planes: BTreeSet<u32> = crate::router::plane_layers(problem.layer_count as usize)
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
    let mut orders = Vec::new();
    push_order(&mut orders, base.clone());

    let mut short_first = base.clone();
    short_first.sort_by(|&a, &b| {
        connection_span_um(&problem.connections[a])
            .cmp(&connection_span_um(&problem.connections[b]))
            .then_with(|| {
                problem.connections[a]
                    .name
                    .cmp(&problem.connections[b].name)
            })
    });
    push_order(&mut orders, short_first);

    let mut long_first = base.clone();
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

    let mut crossing_first = base;
    crossing_first.sort_by(|&a, &b| {
        connection_crossing_pressure(problem, b)
            .cmp(&connection_crossing_pressure(problem, a))
            .then_with(|| {
                connection_span_um(&problem.connections[b])
                    .cmp(&connection_span_um(&problem.connections[a]))
            })
            .then_with(|| {
                problem.connections[a]
                    .name
                    .cmp(&problem.connections[b].name)
            })
    });
    push_order(&mut orders, crossing_first);

    orders
}

fn push_order(orders: &mut Vec<Vec<usize>>, order: Vec<usize>) {
    if !orders.iter().any(|existing| existing == &order) {
        orders.push(order);
    }
}

fn connection_span_um(conn: &Connection) -> u64 {
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

fn connection_crossing_pressure(problem: &RouteProblem, idx: usize) -> usize {
    let Some(conn) = problem.connections.get(idx) else {
        return 0;
    };
    let Some((a, b)) = two_pin_points(conn) else {
        return 0;
    };
    problem
        .connections
        .iter()
        .enumerate()
        .filter(|(other_idx, other)| *other_idx != idx && two_pin_points(other).is_some())
        .filter_map(|(_, other)| two_pin_points(other))
        .filter(|&(c, d)| segments_cross(a, b, c, d))
        .count()
}

fn two_pin_points(conn: &Connection) -> Option<(Point2, Point2)> {
    match conn.points_to_connect.as_slice() {
        [a, b] => Some((a.point(), b.point())),
        _ => None,
    }
}

fn segments_cross(a: Point2, b: Point2, c: Point2, d: Point2) -> bool {
    let orient = |p: Point2, q: Point2, r: Point2| {
        let v = (q.x - p.x) * (r.y - p.y) - (q.y - p.y) * (r.x - p.x);
        if v.abs() < geom::EPS {
            0
        } else if v > 0.0 {
            1
        } else {
            -1
        }
    };
    orient(a, b, c) * orient(a, b, d) < 0 && orient(c, d, a) * orient(c, d, b) < 0
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
    let mut dropped = crate::lint::drop_violating_copper(problem, solution);
    dropped.extend(crate::lint::drop_unconnected_copper(problem, solution));
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

fn compare_quality(a: &RouteQuality, b: &RouteQuality) -> std::cmp::Ordering {
    a.faults()
        .cmp(&b.faults())
        .then_with(|| a.failed_nets.cmp(&b.failed_nets))
        .then_with(|| a.via_count.cmp(&b.via_count))
        .then_with(|| a.wirelength.total_cmp(&b.wirelength))
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
    use crate::problem::{Obstacle, Rect, RoutePoint};

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
        assert!(crate::lint::lint(&p, &result.solution).is_empty());
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
    fn channel_reports_multi_pin_nets_honestly() {
        let p = problem(vec![conn(
            "STAR",
            &[(2.0, 2.0, "top"), (8.0, 2.0, "top"), (8.0, 8.0, "top")],
        )]);

        let result = route_channel(&p);

        assert_eq!(result.failed.len(), 1);
        assert_eq!(result.failed[0].connection, "STAR");
        assert!(result.solution.traces.is_empty());
        assert!(result.solution.vias.is_empty());
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
}
