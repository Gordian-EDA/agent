use pcb_model::{RouteProblem, RouteQuality, RouteResult, RouteSolution, Trace};

pub(crate) fn route_quality(problem: &RouteProblem, result: &RouteResult) -> RouteQuality {
    RouteQuality::of(
        problem,
        result,
        grid_astar::router::geometry_violations(problem, &result.solution),
    )
}

pub(crate) fn compare_route_quality(a: &RouteQuality, b: &RouteQuality) -> std::cmp::Ordering {
    a.faults()
        .cmp(&b.faults())
        .then_with(|| a.failed_nets.cmp(&b.failed_nets))
        .then_with(|| a.via_count.cmp(&b.via_count))
        .then_with(|| a.wirelength.total_cmp(&b.wirelength))
}

pub(crate) fn keep_route_candidate(incumbent: &RouteQuality, challenger: &RouteQuality) -> bool {
    compare_route_quality(incumbent, challenger) != std::cmp::Ordering::Greater
}

pub(crate) fn trace_route_cost_um(
    problem: &RouteProblem,
    solution: &RouteSolution,
    trace: &Trace,
    length_um: u64,
) -> u64 {
    length_um.saturating_add(trace_proximity_penalty_um(problem, solution, trace))
}

pub(crate) fn trace_proximity_penalty_um(
    problem: &RouteProblem,
    solution: &RouteSolution,
    trace: &Trace,
) -> u64 {
    let mut penalty = 0u64;
    for segment_points in trace.path.windows(2) {
        let segment = geom::Segment::new(segment_points[0], segment_points[1]);
        let segment_len = segment.length();
        if segment_len <= geom::EPS {
            continue;
        }

        for obstacle in &problem.obstacles {
            if obstacle
                .connected_to
                .iter()
                .any(|net| net == &trace.connection)
                || !obstacle.layers.iter().any(|layer| layer == &trace.layer)
            {
                continue;
            }
            let obstacle_rect = geom::Rect::from_center_half(
                obstacle.center,
                (obstacle.width / 2.0, obstacle.height / 2.0),
            );
            let clearance = problem.clearance + trace.width / 2.0;
            penalty = penalty.saturating_add(clearance_margin_penalty_um(
                segment.dist_to_rect(&obstacle_rect),
                clearance,
                segment_len,
                problem,
            ));
        }

        for other in &solution.traces {
            if other.connection == trace.connection || other.layer != trace.layer {
                continue;
            }
            for other_points in other.path.windows(2) {
                let other_segment = geom::Segment::new(other_points[0], other_points[1]);
                let clearance = problem.clearance + trace.width / 2.0 + other.width / 2.0;
                penalty = penalty.saturating_add(clearance_margin_penalty_um(
                    segment.dist_to_segment(other_segment),
                    clearance,
                    segment_len,
                    problem,
                ));
            }
        }

        for via in &solution.vias {
            if via.connection == trace.connection {
                continue;
            }
            let clearance = problem.clearance + trace.width / 2.0 + via.diameter / 2.0;
            penalty = penalty.saturating_add(clearance_margin_penalty_um(
                segment.dist_to_point(via.at),
                clearance,
                segment_len,
                problem,
            ));
        }
    }
    penalty
}

fn clearance_margin_penalty_um(
    distance: f64,
    required_clearance: f64,
    segment_len: f64,
    problem: &RouteProblem,
) -> u64 {
    let margin = distance - required_clearance;
    let window = (problem.clearance + problem.min_trace_width).max(0.1);
    if margin >= window {
        return 0;
    }
    let closeness = ((window - margin).max(0.0) / window).clamp(0.0, 1.0);
    (closeness * segment_len * 1000.0).round().max(0.0) as u64
}
