use pcb_model::{LayerRef, Obstacle, Point2, RouteProblem, RouteSolution, Trace, Via, ViaSpan};

pub fn copper_obstacles(problem: &RouteProblem, solution: &RouteSolution) -> Vec<Obstacle> {
    let mut obstacles = Vec::new();
    for trace in &solution.traces {
        obstacles.extend(trace_obstacles(problem, trace));
    }
    for via in &solution.vias {
        obstacles.push(via_obstacle(problem, via));
    }
    obstacles
}

pub(crate) fn trace_obstacles(problem: &RouteProblem, trace: &Trace) -> Vec<Obstacle> {
    let pitch = pcb_route_grid::grid::grid_pitch(problem);
    trace
        .path
        .windows(2)
        .flat_map(|segment| {
            let a = segment[0];
            let b = segment[1];
            let len = a.dist(b);
            if len <= 1e-9 {
                return Vec::new();
            }
            let steps = (len / pitch).ceil().max(1.0) as usize;
            let mut out = Vec::with_capacity(steps + 1);
            for step in 0..=steps {
                let t = step as f64 / steps as f64;
                out.push(Obstacle {
                    kind: "route-trace".to_owned(),
                    layers: vec![trace.layer.clone()],
                    center: Point2 {
                        x: a.x + (b.x - a.x) * t,
                        y: a.y + (b.y - a.y) * t,
                    },
                    width: trace.width,
                    height: trace.width,
                    connected_to: vec![trace.connection.clone()],
                });
            }
            out
        })
        .collect()
}

fn via_obstacle(problem: &RouteProblem, via: &Via) -> Obstacle {
    Obstacle {
        kind: "route-via".to_owned(),
        layers: via_layers(problem, &via.span),
        center: via.at,
        width: via.diameter,
        height: via.diameter,
        connected_to: vec![via.connection.clone()],
    }
}

fn via_layers(problem: &RouteProblem, span: &ViaSpan) -> Vec<LayerRef> {
    match span {
        ViaSpan::Through => (0..problem.layer_count)
            .map(|idx| layer_ref_from_index(idx, problem.layer_count))
            .collect(),
        ViaSpan::Partial { from, to, .. } => {
            let lo = (*from).min(*to);
            let hi = (*from).max(*to).min(problem.layer_count.saturating_sub(1));
            (lo..=hi)
                .map(|idx| layer_ref_from_index(idx, problem.layer_count))
                .collect()
        }
    }
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
