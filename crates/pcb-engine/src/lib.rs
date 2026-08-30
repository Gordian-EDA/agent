//! Gordian's production PCB physical-design boundary.
//!
//! Application workflows call this crate for placement and routing policy;
//! concrete placer/router crates remain implementation details of this facade.

use pcb_model::{Obstacle, PcbEngine, PcbProblem, PcbSolution, RoutingView};
use pcb_place::{PlacementHints, PlacementView};

/// Run the production placement policy for an already-imported board.
pub fn place_tuned(problem: &PlacementView, hints: &PlacementHints) -> pcb_place::PlaceResult {
    pcb_place::place_tuned(problem, hints)
}

/// Apply a fully prescribed placement without the tuned search portfolio.
pub fn place_prescribed(problem: &PlacementView, hints: &PlacementHints) -> pcb_place::PlaceResult {
    pcb_place::placement::place(problem, hints)
}

/// Run the production routing portfolio and retain its per-pass diagnostics.
pub fn route_tuned(problem: &RoutingView) -> pcb_route_mesh::pipeline::TunedRouteRun {
    pcb_route_mesh::pipeline::route_tuned_with_diagnostics(problem)
}

/// Gordian's tuned deterministic place-then-route implementation.
#[derive(Debug, Clone, Default)]
pub struct GordianPcbEngine {
    placement_hints: PlacementHints,
}

impl GordianPcbEngine {
    pub fn new(placement_hints: PlacementHints) -> Self {
        Self { placement_hints }
    }
}

impl PcbEngine for GordianPcbEngine {
    fn solve(&self, problem: &PcbProblem) -> PcbSolution {
        let placement_problem = placement_view(problem);
        let placed = place_tuned(&placement_problem, &self.placement_hints);
        if !placed.legal {
            return PcbSolution {
                placements: placed.placements,
                copper: problem.fixed_copper.clone(),
                failed: Vec::new(),
                placement_legal: false,
                diagnostics: vec!["placement is illegal; routing was not attempted".to_owned()],
            };
        }

        let validation_problem = routing_view(problem, &placed.placements);
        let mut routing_problem = validation_problem.clone();
        routing_problem
            .obstacles
            .extend(pcb_route_mesh::copper::copper_obstacles(
                &validation_problem,
                &problem.fixed_copper,
            ));
        let routed = route_tuned(&routing_problem).result;
        let mut copper = problem.fixed_copper.clone();
        copper.traces.extend(routed.solution.traces);
        copper.vias.extend(routed.solution.vias);
        let violations = pcb_drc::lint::lint(&validation_problem, &copper);
        let diagnostics = violations
            .into_iter()
            .map(|violation| format!("{violation:?}"))
            .collect();
        PcbSolution {
            placements: placed.placements,
            copper,
            failed: routed.failed,
            placement_legal: true,
            diagnostics,
        }
    }
}

fn placement_view(problem: &PcbProblem) -> PlacementView {
    PlacementView {
        bounds: problem.bounds,
        clearance: problem.clearance,
        layer_count: problem.layer_count,
        min_trace_width: problem.min_trace_width,
        parts: problem.parts.clone(),
        keepouts: problem
            .obstacles
            .iter()
            .filter(|obstacle| obstacle.connected_to.is_empty())
            .map(obstacle_rect)
            .collect(),
        outline: problem.outline.clone(),
    }
}

fn routing_view(problem: &PcbProblem, placements: &[pcb_model::Placement]) -> RoutingView {
    let (obstacles, connections) = problem.routing_geometry(placements);
    RoutingView {
        layer_count: problem.layer_count,
        min_trace_width: problem.min_trace_width,
        obstacles,
        connections,
        bounds: routing_bounds(problem),
        clearance: problem.clearance,
        via_diameter: problem.via_diameter,
        via_drill: problem.via_drill,
        net_widths: problem.net_widths.clone(),
        outline: problem.outline.clone(),
        plane_nets: problem.plane_nets.clone(),
        escape_layers: problem.escape_layers.clone(),
    }
}

fn obstacle_rect(obstacle: &Obstacle) -> pcb_model::Rect {
    pcb_model::Rect {
        min_x: obstacle.center.x - obstacle.width / 2.0,
        max_x: obstacle.center.x + obstacle.width / 2.0,
        min_y: obstacle.center.y - obstacle.height / 2.0,
        max_y: obstacle.center.y + obstacle.height / 2.0,
    }
}

fn routing_bounds(problem: &PcbProblem) -> pcb_model::Rect {
    let max_x_inset = ((problem.bounds.max_x - problem.bounds.min_x) / 2.0 - 0.1).max(0.0);
    let max_y_inset = ((problem.bounds.max_y - problem.bounds.min_y) / 2.0 - 0.1).max(0.0);
    let inset = problem.edge_clearance.min(max_x_inset).min(max_y_inset);
    pcb_model::Rect {
        min_x: problem.bounds.min_x + inset,
        max_x: problem.bounds.max_x - inset,
        min_y: problem.bounds.min_y + inset,
        max_y: problem.bounds.max_y - inset,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcb_model::{LayerRef, Part, PartPad, Point2, Rect, RouteSolution};

    #[test]
    fn engine_places_and_routes_one_complete_problem() {
        let part = |reference: &str, net: &str| Part {
            reference: reference.to_owned(),
            courtyard_w: 1.0,
            courtyard_h: 1.0,
            pads: vec![PartPad {
                number: "1".to_owned(),
                offset: Point2::new(0.0, 0.0),
                width: 0.5,
                height: 0.5,
                layers: vec![LayerRef::top()],
                net: Some(net.to_owned()),
            }],
            edge_datum: None,
            locked: None,
        };
        let problem = PcbProblem {
            bounds: Rect::new(0.0, 0.0, 20.0, 10.0),
            layer_count: 2,
            clearance: 0.2,
            edge_clearance: 0.5,
            min_trace_width: 0.2,
            via_diameter: 0.6,
            via_drill: 0.3,
            parts: vec![part("R1", "N"), part("R2", "N")],
            obstacles: Vec::new(),
            connections: Vec::new(),
            net_widths: Default::default(),
            outline: None,
            plane_nets: Default::default(),
            escape_layers: Default::default(),
            fixed_copper: RouteSolution::default(),
        };

        let solution = GordianPcbEngine::default().solve(&problem);

        assert!(solution.placement_legal);
        assert_eq!(solution.placements.len(), 2);
        assert!(solution.failed.is_empty());
        assert!(solution.diagnostics.is_empty());
        assert!(!solution.copper.traces.is_empty());
    }

    #[test]
    fn engine_preserves_fixed_copper() {
        let fixed = pcb_model::Trace {
            connection: "fixed".to_owned(),
            layer: LayerRef::top(),
            width: 0.2,
            path: vec![Point2::new(2.0, 2.0), Point2::new(8.0, 2.0)],
        };
        let problem = PcbProblem {
            bounds: Rect::new(0.0, 0.0, 10.0, 10.0),
            layer_count: 2,
            clearance: 0.2,
            edge_clearance: 0.5,
            min_trace_width: 0.2,
            via_diameter: 0.6,
            via_drill: 0.3,
            parts: Vec::new(),
            obstacles: Vec::new(),
            connections: Vec::new(),
            net_widths: Default::default(),
            outline: None,
            plane_nets: Default::default(),
            escape_layers: Default::default(),
            fixed_copper: RouteSolution {
                traces: vec![fixed.clone()],
                vias: Vec::new(),
            },
        };

        let solution = GordianPcbEngine::default().solve(&problem);

        assert_eq!(solution.copper.traces, vec![fixed]);
    }
}
