//! Contextual sequential grid router.
//!
//! This router sits between the narrow pattern routers and the full negotiated
//! mesh. It routes one net at a time with the grid A* router, converting already
//! accepted copper into temporary obstacles before routing the next net. That
//! gives it a different search shape from the whole-board grid fallback: later
//! nets see the exact copper chosen by earlier nets instead of only the grid
//! occupancy produced inside one full-board pass.

use crate::copper::copper_obstacles;
#[cfg(test)]
use crate::copper::trace_obstacles;
use crate::heuristics::{
    connection_crossing_pressures, connection_obstacle_pressure_um,
    connection_segment_obstacle_pressure_um, connection_span_um,
};
use crate::problem::{
    Capabilities, FailedNet, RouteProblem, RouteQuality, RouteResult, RouteSolution, Router,
};
use crate::quality::{keep_route_candidate as keep_candidate, route_quality};
use crate::router::GridAStarRouter;

/// This engine's [`RouteResult::engine`] provenance tag.
pub const ENGINE: &str = "sequential-grid";
const SEQUENTIAL_ORDER_PORTFOLIO_MAX_CONNECTIONS: usize = 6;

/// Route nets one by one with grid A*, feeding accepted copper back as obstacles.
#[derive(Debug, Clone, Copy, Default)]
pub struct SequentialGridRouter;

impl Router for SequentialGridRouter {
    fn name(&self) -> &'static str {
        ENGINE
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            max_layers: u32::MAX,
            honors_escape_layers: true,
            honors_net_widths: true,
            honors_outline: true,
        }
    }

    fn can_route(&self, problem: &RouteProblem) -> bool {
        self.capabilities().can_route(problem)
            && problem.connections.len() <= SEQUENTIAL_ORDER_PORTFOLIO_MAX_CONNECTIONS
    }

    fn route(&self, problem: &RouteProblem) -> RouteResult {
        route_sequential(problem)
    }
}

/// Route `problem` with the contextual sequential grid portfolio.
pub fn route_sequential(problem: &RouteProblem) -> RouteResult {
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
        .unwrap_or_else(|| RouteResult {
            solution: RouteSolution {
                traces: Vec::new(),
                vias: Vec::new(),
            },
            failed: Vec::new(),
            engine: ENGINE.to_owned(),
        })
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
    let grid = GridAStarRouter;
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
        if conn.points_to_connect.len() < 2 {
            continue;
        }

        let subproblem = problem_with_single_connection_and_copper(problem, idx, &solution);
        let candidate = grid.route(&subproblem);
        if !candidate.failed.is_empty() {
            failed.push(FailedNet {
                connection: conn.name.clone(),
                reason: "sequential grid failed with routed-copper obstacles".to_owned(),
            });
            continue;
        }

        let mut combined = solution.clone();
        combined.traces.extend(candidate.solution.traces);
        combined.vias.extend(candidate.solution.vias);
        let validation = problem_with_connections(problem, &routed, idx);
        crate::via_cleanup::normalize_redundant_vias(&validation, &mut combined);
        if !crate::lint::lint(&validation, &combined).is_empty() {
            failed.push(FailedNet {
                connection: conn.name.clone(),
                reason: "sequential grid route conflicted with accepted copper".to_owned(),
            });
            continue;
        }

        solution = combined;
        routed.push(idx);
    }

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
    let failed_names: std::collections::BTreeSet<&str> =
        failed.iter().map(|f| f.connection.as_str()).collect();
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
    metrics: &[NetOrderMetric],
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

fn net_order_portfolio(problem: &RouteProblem) -> Vec<Vec<usize>> {
    let base: Vec<usize> = (0..problem.connections.len()).collect();
    let mut orders = Vec::new();
    push_order(&mut orders, base.clone());
    if base.len() > SEQUENTIAL_ORDER_PORTFOLIO_MAX_CONNECTIONS {
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

    let mut long_first = base.clone();
    long_first.sort_by(|&a, &b| {
        metrics[b].span_um.cmp(&metrics[a].span_um).then_with(|| {
            problem.connections[a]
                .name
                .cmp(&problem.connections[b].name)
        })
    });
    push_order(&mut orders, long_first);

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
    push_order(&mut orders, many_pins_first);

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
struct NetOrderMetric {
    pin_count: usize,
    span_um: u64,
    segment_obstacle_pressure_um: u64,
    obstacle_pressure_um: u64,
    crossing_pressure: usize,
}

fn net_order_metrics(problem: &RouteProblem) -> Vec<NetOrderMetric> {
    let crossing_pressures = connection_crossing_pressures(problem);
    problem
        .connections
        .iter()
        .enumerate()
        .map(|(idx, conn)| NetOrderMetric {
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

fn problem_with_single_connection_and_copper(
    problem: &RouteProblem,
    idx: usize,
    solution: &RouteSolution,
) -> RouteProblem {
    let mut out = problem.clone();
    out.connections = problem
        .connections
        .get(idx)
        .cloned()
        .into_iter()
        .collect::<Vec<_>>();
    out.obstacles.extend(copper_obstacles(problem, solution));
    out
}

fn problem_with_connections(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heuristics::connection_crossing_pressure;
    use crate::problem::{Connection, LayerRef, Obstacle, Point2, Rect, RoutePoint, Trace};

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

    fn pad(net: &str, at: (f64, f64), layer: &str) -> Obstacle {
        Obstacle {
            kind: "rect".to_owned(),
            layers: vec![LayerRef(layer.to_owned())],
            center: Point2 { x: at.0, y: at.1 },
            width: 0.5,
            height: 0.5,
            connected_to: vec![net.to_owned()],
        }
    }

    fn base() -> RouteProblem {
        RouteProblem {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles: Vec::new(),
            connections: Vec::new(),
            bounds: Rect {
                min_x: 0.0,
                max_x: 20.0,
                min_y: 0.0,
                max_y: 12.0,
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
    fn net_order_portfolio_includes_many_pins_first_order() {
        let mut p = base();
        p.connections = vec![
            conn("SIG", &[(1.0, 1.0, "top"), (4.0, 1.0, "top")]),
            conn(
                "BUS",
                &[(1.0, 3.0, "top"), (4.0, 3.0, "top"), (4.0, 5.0, "top")],
            ),
            conn("CLK", &[(1.0, 7.0, "top"), (18.0, 7.0, "top")]),
        ];

        let orders = net_order_portfolio(&p);
        let many_pins = vec![1, 2, 0];

        assert!(
            orders.iter().any(|order| order == &many_pins),
            "sequential portfolio should let high-pin nets claim trunk space early: {orders:?}"
        );
    }

    #[test]
    fn net_order_portfolio_bounds_large_boards_to_base_order() {
        let mut p = base();
        p.connections = (0..=SEQUENTIAL_ORDER_PORTFOLIO_MAX_CONNECTIONS)
            .map(|idx| {
                conn(
                    &format!("N{idx}"),
                    &[
                        (1.0, 1.0 + idx as f64, "top"),
                        (10.0, 1.0 + idx as f64, "top"),
                    ],
                )
            })
            .collect();

        let orders = net_order_portfolio(&p);

        assert_eq!(
            orders,
            vec![(0..=SEQUENTIAL_ORDER_PORTFOLIO_MAX_CONNECTIONS).collect::<Vec<_>>()],
            "large boards should not pay the full sequential ordering portfolio before primary routing"
        );
    }

    #[test]
    fn sequential_router_declines_large_boards() {
        let mut p = base();
        p.connections = (0..=SEQUENTIAL_ORDER_PORTFOLIO_MAX_CONNECTIONS)
            .map(|idx| {
                conn(
                    &format!("N{idx}"),
                    &[
                        (1.0, 1.0 + idx as f64, "top"),
                        (10.0, 1.0 + idx as f64, "top"),
                    ],
                )
            })
            .collect();

        assert!(
            !SequentialGridRouter.can_route(&p),
            "large boards should proceed to the primary/fallback routers instead of the contextual sequential portfolio"
        );
    }

    #[test]
    fn failed_priority_order_promotes_failed_nets_by_routing_impact() {
        let mut p = base();
        p.connections = vec![
            conn("BLOCKER", &[(1.0, 1.0, "top"), (4.0, 1.0, "top")]),
            conn("SIG", &[(1.0, 3.0, "top"), (15.0, 3.0, "top")]),
            conn(
                "BUS",
                &[(1.0, 5.0, "top"), (15.0, 5.0, "top"), (15.0, 7.0, "top")],
            ),
            conn("TAIL", &[(1.0, 9.0, "top"), (4.0, 9.0, "top")]),
        ];
        p.obstacles = vec![pad("BUS", (8.0, 6.0), "top")];
        let order = vec![0, 1, 2, 3];
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

        let retry = failed_priority_order(&p, &order, &failed);

        assert_eq!(
            &retry[..2],
            &[2, 1],
            "failed high-pin/high-pressure nets should be promoted before their blockers"
        );
        assert_eq!(
            &retry[2..],
            &[0, 3],
            "non-failed nets should keep the original relative order"
        );
    }

    #[test]
    fn failed_priority_order_uses_crossing_pressure_after_obstacle_pressure() {
        let mut p = base();
        p.bounds.max_y = 20.0;
        p.connections = vec![
            conn("OPEN", &[(1.0, 1.0, "top"), (4.0, 1.0, "top")]),
            conn("CROSS_A", &[(2.0, 10.0, "top"), (18.0, 10.0, "top")]),
            conn("CROSS_B", &[(10.0, 2.0, "top"), (10.0, 18.0, "top")]),
            conn("TAIL", &[(1.0, 18.0, "top"), (4.0, 18.0, "top")]),
        ];
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

        assert_eq!(
            &retry[..2],
            &[1, 0],
            "crossing failed nets should retry before otherwise equal open failed nets"
        );
        assert_eq!(&retry[2..], &[2, 3]);
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
            Obstacle {
                kind: "rect".to_owned(),
                layers: vec![LayerRef::top()],
                center: Point2 { x: 8.0, y: 2.0 },
                width: 0.5,
                height: 0.5,
                connected_to: vec![],
            },
            Obstacle {
                kind: "rect".to_owned(),
                layers: vec![LayerRef::top()],
                center: Point2 { x: 5.0, y: 1.0 },
                width: 0.5,
                height: 0.5,
                connected_to: vec![],
            },
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
            "failed retry should prioritize actual segment-corridor blockage before bbox-only pressure"
        );
        assert_eq!(&retry[2..], &[2]);
    }

    #[test]
    fn net_order_portfolio_includes_crossing_pressure_order() {
        let mut p = base();
        p.bounds.max_y = 20.0;
        p.connections = vec![
            conn("OPEN", &[(1.0, 1.0, "top"), (4.0, 1.0, "top")]),
            conn("CROSS_A", &[(2.0, 10.0, "top"), (18.0, 10.0, "top")]),
            conn("CROSS_B", &[(10.0, 2.0, "top"), (10.0, 18.0, "top")]),
            conn("LONG", &[(1.0, 18.0, "top"), (19.0, 18.0, "top")]),
        ];

        let orders = net_order_portfolio(&p);

        assert!(
            orders.iter().any(|order| order == &[1, 2, 3, 0]),
            "sequential portfolio should include the crossing-pressure-first order: {orders:?}"
        );
        assert_eq!(connection_crossing_pressure(&p, 0), 0);
        assert_eq!(connection_crossing_pressure(&p, 1), 1);
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
            Obstacle {
                kind: "rect".to_owned(),
                layers: vec![LayerRef::top()],
                center: Point2 { x: 8.0, y: 2.0 },
                width: 0.5,
                height: 0.5,
                connected_to: vec![],
            },
            Obstacle {
                kind: "rect".to_owned(),
                layers: vec![LayerRef::top()],
                center: Point2 { x: 5.0, y: 1.0 },
                width: 0.5,
                height: 0.5,
                connected_to: vec![],
            },
        ];
        let metrics = net_order_metrics(&p);
        let orders = net_order_portfolio(&p);

        assert_eq!(metrics[0].segment_obstacle_pressure_um, 0);
        assert!(metrics[0].obstacle_pressure_um > 0);
        assert!(metrics[1].segment_obstacle_pressure_um > 0);
        assert!(
            orders.iter().any(|order| order == &[1, 0, 2]),
            "sequential portfolio should include the segment-obstacle-pressure-first order: {orders:?}"
        );
    }

    #[test]
    fn net_order_metrics_cache_routing_pressure_signals() {
        let mut p = base();
        p.bounds.max_y = 20.0;
        p.connections = vec![
            conn("OPEN", &[(1.0, 1.0, "top"), (4.0, 1.0, "top")]),
            conn("CROSS", &[(2.0, 10.0, "top"), (18.0, 10.0, "top")]),
            conn("VERT", &[(10.0, 2.0, "top"), (10.0, 18.0, "top")]),
        ];
        p.obstacles = vec![Obstacle {
            kind: "rect".to_owned(),
            layers: vec![LayerRef::top()],
            center: Point2 { x: 8.0, y: 10.0 },
            width: 0.5,
            height: 0.5,
            connected_to: vec![],
        }];

        let metrics = net_order_metrics(&p);

        assert_eq!(metrics[1].pin_count, 2);
        assert_eq!(metrics[1].span_um, connection_span_um(&p.connections[1]));
        assert_eq!(
            metrics[1].segment_obstacle_pressure_um,
            connection_segment_obstacle_pressure_um(&p, &p.connections[1])
        );
        assert_eq!(
            metrics[1].obstacle_pressure_um,
            connection_obstacle_pressure_um(&p, &p.connections[1])
        );
        assert_eq!(metrics[1].crossing_pressure, 1);
        assert_eq!(metrics[0].crossing_pressure, 0);
    }

    #[test]
    fn obstacle_pressure_uses_active_net_width_and_ignores_own_pads() {
        let mut p = base();
        p.connections = vec![conn("SIG", &[(1.0, 1.0, "top"), (4.0, 1.0, "top")])];
        p.obstacles = vec![
            pad("SIG", (2.0, 1.0), "top"),
            Obstacle {
                kind: "rect".to_owned(),
                layers: vec![LayerRef::top()],
                center: Point2 { x: 5.2, y: 1.0 },
                width: 0.5,
                height: 0.5,
                connected_to: vec![],
            },
        ];

        let thin_pressure = connection_obstacle_pressure_um(&p, &p.connections[0]);
        p.net_widths.insert("FAT_POWER".to_owned(), 4.0);
        let with_unrelated_fat_net = connection_obstacle_pressure_um(&p, &p.connections[0]);

        assert_eq!(
            thin_pressure, 0,
            "own pads and obstacles outside SIG's active-width corridor should not add pressure"
        );
        assert_eq!(
            with_unrelated_fat_net, thin_pressure,
            "an unrelated wide net must not widen the pressure window for SIG"
        );
    }

    #[test]
    fn sequential_grid_routes_later_net_around_accepted_copper() {
        let mut p = base();
        p.connections = vec![
            conn("A", &[(2.0, 6.0, "top"), (18.0, 6.0, "top")]),
            conn("B", &[(10.0, 2.0, "top"), (10.0, 10.0, "top")]),
        ];
        p.obstacles = vec![
            pad("A", (2.0, 6.0), "top"),
            pad("A", (18.0, 6.0), "top"),
            pad("B", (10.0, 2.0), "top"),
            pad("B", (10.0, 10.0), "top"),
        ];

        let result = route_sequential(&p);

        assert_eq!(result.engine, ENGINE);
        assert!(
            result.failed.is_empty(),
            "sequential grid should route crossing top-layer nets by adapting to prior copper: {:?}",
            result.failed
        );
        assert!(
            crate::lint::lint(&p, &result.solution).is_empty(),
            "accepted sequential copper must be DRC-clean"
        );
    }

    #[test]
    fn diagonal_trace_obstacles_do_not_block_empty_bbox_corners() {
        let p = base();
        let trace = Trace {
            connection: "A".to_owned(),
            layer: LayerRef::top(),
            width: p.min_trace_width,
            path: vec![Point2 { x: 2.0, y: 2.0 }, Point2 { x: 18.0, y: 10.0 }],
        };

        let obstacles = trace_obstacles(&p, &trace);
        let inflation = p.clearance + p.min_trace_width / 2.0;
        let empty_bbox_corner = Point2 { x: 10.0, y: 2.0 };
        let blocks_empty_corner = obstacles.iter().any(|ob| {
            (empty_bbox_corner.x - ob.center.x).abs() <= ob.width / 2.0 + inflation
                && (empty_bbox_corner.y - ob.center.y).abs() <= ob.height / 2.0 + inflation
        });

        assert!(
            !blocks_empty_corner,
            "diagonal routed copper should block a narrow sampled capsule, not the empty corner of its bbox"
        );
    }
}
