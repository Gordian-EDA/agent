use pcb_model::{Drc, RouteSolution, RoutingView, ViaSpan};

const VIA_POINT_KEY_SCALE: f64 = 1e9;

pub(crate) fn normalize_redundant_vias(drc: &dyn Drc, problem: &RoutingView, solution: &mut RouteSolution) {
    drop_duplicate_vias(solution);
    drop_covered_vias(drc, problem, solution);
}

fn drop_duplicate_vias(solution: &mut RouteSolution) {
    let mut seen = std::collections::BTreeSet::new();
    solution.vias.retain(|via| {
        seen.insert((
            via.connection.clone(),
            via.at.quantized_key(VIA_POINT_KEY_SCALE),
            (via.diameter * 1000.0).round() as i64,
            (via.drill * 1000.0).round() as i64,
            via_span_key(&via.span),
        ))
    });
}

fn via_span_key(span: &ViaSpan) -> (u32, u32, bool, bool) {
    match span {
        ViaSpan::Through => (0, 0, false, false),
        ViaSpan::Partial { from, to, micro } => (*from, *to, *micro, true),
    }
}

fn drop_covered_vias(drc: &dyn Drc, problem: &RoutingView, solution: &mut RouteSolution) {
    let mut baseline = drc.check(problem, solution);
    let mut idx = 0usize;
    while idx < solution.vias.len() {
        if !via_is_covered_by_another(problem, solution, idx) {
            idx += 1;
            continue;
        }

        let mut candidate = solution.clone();
        candidate.vias.remove(idx);
        let findings = drc.check(problem, &candidate);
        if !introduces_new_findings(&baseline, &findings)
            && candidate.metrics().via_count < solution.metrics().via_count
        {
            *solution = candidate;
            baseline = findings;
        } else {
            idx += 1;
        }
    }
}

fn introduces_new_findings(
    baseline: &[pcb_model::Finding],
    candidate: &[pcb_model::Finding],
) -> bool {
    candidate
        .iter()
        .any(|finding| !baseline.iter().any(|known| known == finding))
}

fn via_is_covered_by_another(problem: &RoutingView, solution: &RouteSolution, idx: usize) -> bool {
    let via = &solution.vias[idx];
    let Some(span) = via_layer_span(problem, &via.span) else {
        return false;
    };
    solution.vias.iter().enumerate().any(|(other_idx, other)| {
        other_idx != idx
            && other.connection == via.connection
            && other.at.dist(via.at) < geom::EPS
            && via_layer_span(problem, &other.span).is_some_and(|other_span| {
                other_span.0 <= span.0
                    && other_span.1 >= span.1
                    && (other_span.0, other_span.1) != span
            })
    })
}

fn via_layer_span(problem: &RoutingView, span: &ViaSpan) -> Option<(u32, u32)> {
    match span {
        ViaSpan::Through => Some((0, problem.layer_count.saturating_sub(1))),
        ViaSpan::Partial { from, to, .. } => {
            let lo = (*from).min(*to);
            let hi = (*from).max(*to);
            (hi < problem.layer_count).then_some((lo, hi))
        }
    }
}

#[cfg(test)]
mod tests {
    use pcb_model::Drc as _;

static DRC: pcb_drc::StandardDrc = pcb_drc::StandardDrc;

    use super::*;
    use pcb_model::{Connection, LayerRef, Point2, Rect, RoutePoint, Trace, Via, ViaSpan};

    fn layer_change_problem() -> RoutingView {
        RoutingView {
            layer_count: 4,
            min_trace_width: 0.2,
            obstacles: vec![],
            connections: vec![Connection {
                name: "N".to_owned(),
                points_to_connect: vec![
                    RoutePoint {
                        x: 1.0,
                        y: 1.0,
                        layer: LayerRef::top(),
                    },
                    RoutePoint {
                        x: 5.0,
                        y: 1.0,
                        layer: LayerRef::bottom(),
                    },
                ],
            }],
            bounds: Rect {
                min_x: 0.0,
                max_x: 10.0,
                min_y: 0.0,
                max_y: 10.0,
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

    fn solution_with_vias(vias: Vec<Via>) -> (RoutingView, RouteSolution) {
        let problem = layer_change_problem();
        let solution = RouteSolution {
            traces: vec![
                Trace {
                    connection: "N".to_owned(),
                    layer: LayerRef::top(),
                    width: problem.min_trace_width,
                    path: vec![Point2 { x: 1.0, y: 1.0 }, Point2 { x: 3.0, y: 1.0 }],
                },
                Trace {
                    connection: "N".to_owned(),
                    layer: LayerRef::bottom(),
                    width: problem.min_trace_width,
                    path: vec![Point2 { x: 3.0, y: 1.0 }, Point2 { x: 5.0, y: 1.0 }],
                },
            ],
            vias,
        };
        (problem, solution)
    }

    fn through_via() -> Via {
        Via {
            connection: "N".to_owned(),
            at: Point2 { x: 3.0, y: 1.0 },
            diameter: 0.6,
            drill: 0.3,
            span: ViaSpan::Through,
        }
    }

    #[test]
    fn normalize_redundant_vias_drops_exact_duplicate_before_drc() {
        let via = through_via();
        let (problem, mut solution) = solution_with_vias(vec![via.clone(), via]);

        normalize_redundant_vias(&DRC, &problem, &mut solution);

        assert_eq!(solution.vias.len(), 1);
        assert!(matches!(solution.vias[0].span, ViaSpan::Through));
        assert!(DRC.check(&problem, &solution).is_empty());
    }

    #[test]
    fn normalize_redundant_vias_drops_partial_covered_by_through() {
        let (problem, mut solution) = solution_with_vias(vec![
            through_via(),
            Via {
                connection: "N".to_owned(),
                at: Point2 { x: 3.0, y: 1.0 },
                diameter: 0.6,
                drill: 0.3,
                span: ViaSpan::Partial {
                    from: 0,
                    to: 1,
                    micro: false,
                },
            },
        ]);

        normalize_redundant_vias(&DRC, &problem, &mut solution);

        assert_eq!(solution.vias.len(), 1);
        assert!(matches!(solution.vias[0].span, ViaSpan::Through));
        assert!(DRC.check(&problem, &solution).is_empty());
    }
}
