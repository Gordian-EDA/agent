//! Production placement and routing policy facade.
//!
//! Workflows invoke placement and routing independently so the saved KiCAD
//! board can be inspected or edited between phases. Concrete algorithm crates
//! remain implementation details of this boundary.

use pcb_model::{PlaceResult, PlacementHints, PlacementView, RoutingView};

/// Run the production placement policy for an imported board.
pub fn place_tuned(problem: &PlacementView, hints: &PlacementHints) -> PlaceResult {
    pcb_place::place_tuned(problem, hints)
}

/// Apply a fully prescribed placement without the tuned search portfolio.
pub fn place_prescribed(problem: &PlacementView, hints: &PlacementHints) -> PlaceResult {
    pcb_place::placement::place(problem, hints)
}

/// Run the production routing portfolio and retain its per-pass diagnostics.
pub fn route_tuned(problem: &RoutingView) -> pcb_route_mesh::pipeline::TunedRouteRun {
    let problem = routing_problem(problem);
    pcb_route_mesh::pipeline::route_tuned_with_diagnostics(&problem)
}

fn routing_problem(problem: &RoutingView) -> RoutingView {
    let mut scoped = problem.clone();
    if let Some(nets) = &problem.nets {
        scoped
            .connections
            .retain(|connection| nets.contains(&connection.name));
    }
    scoped
        .obstacles
        .extend(pcb_route_mesh::copper::copper_obstacles(
            problem,
            &problem.fixed_copper,
        ));
    scoped
}

#[cfg(test)]
mod tests {
    use pcb_model::{
        Connection, LayerRef, Point2, Rect, RoutePoint, RouteSolution, RoutingView, Trace,
    };

    use super::routing_problem;

    fn routing_view() -> RoutingView {
        RoutingView {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles: Vec::new(),
            connections: ["A", "B"]
                .into_iter()
                .map(|name| Connection {
                    name: name.to_owned(),
                    points_to_connect: vec![
                        RoutePoint {
                            x: 1.0,
                            y: 1.0,
                            layer: LayerRef::top(),
                        },
                        RoutePoint {
                            x: 9.0,
                            y: 9.0,
                            layer: LayerRef::top(),
                        },
                    ],
                })
                .collect(),
            bounds: Rect::new(0.0, 0.0, 10.0, 10.0),
            clearance: 0.2,
            via_diameter: 0.6,
            via_drill: 0.3,
            net_widths: Default::default(),
            outline: None,
            plane_nets: Default::default(),
            escape_layers: Default::default(),
            fixed_copper: RouteSolution {
                traces: vec![Trace {
                    connection: "FIXED".to_owned(),
                    layer: LayerRef::top(),
                    width: 0.2,
                    path: vec![Point2::new(2.0, 2.0), Point2::new(8.0, 2.0)],
                }],
                vias: Vec::new(),
            },
            nets: None,
        }
    }

    #[test]
    fn unscoped_routing_keeps_all_connections_and_fixed_copper() {
        let problem = routing_problem(&routing_view());

        assert_eq!(problem.connections.len(), 2);
        assert!(!problem.obstacles.is_empty());
    }

    #[test]
    fn scoped_routing_keeps_only_requested_connections() {
        let mut problem = routing_view();
        problem.nets = Some(vec!["B".to_owned()]);

        let problem = routing_problem(&problem);

        assert_eq!(problem.connections.len(), 1);
        assert_eq!(problem.connections[0].name, "B");
    }
}
