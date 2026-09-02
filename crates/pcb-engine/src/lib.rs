//! The composition root for the PCB phases.
//!
//! Every algorithm crate is a leaf that names only `pcb-model`: the placer takes
//! its routability oracle as a [`RouteProbe`], the routers take their design-rule
//! oracle as a [`Drc`] and their sub-routers as [`PcbRouter`]s. This crate is the
//! one place that knows which concrete leaf fills each hole, so workflows invoke
//! placement and routing independently without naming an algorithm.

use pcb_drc::StandardDrc;
use pcb_model::{
    Budget, Drc, PcbPlacer, PcbRouter, PlaceResult, PlacementHints, PlacementView, RouteProbe,
    RoutingView,
};
use pcb_place::TunedPlacer;
use pcb_route_grid::probe::GridRouteProbe;
use pcb_route_grid::router::{GridRouter, GridSinglePassRouter};
use pcb_route_mesh::deps::MeshDeps;

/// The production design-rule oracle every phase reconciles against.
pub const DRC: StandardDrc = StandardDrc;

/// The production routability probe: the grid router's cheap orthogonal pass.
pub fn route_probe() -> impl RouteProbe {
    GridRouteProbe::new(&DRC)
}

/// Run `f` against the premium router's collaborators, wired to the production
/// leaves. The one place the mesh engine's holes are filled.
fn with_mesh_deps<R>(budget: Budget, f: impl FnOnce(&MeshDeps) -> R) -> R {
    let grid = GridRouter::new(&DRC);
    let grid_seed = GridSinglePassRouter::new(&DRC);
    f(&MeshDeps {
        drc: &DRC,
        grid: &grid,
        grid_seed: &grid_seed,
        budget,
    })
}

/// Run the production placement policy for an imported board.
pub fn place_tuned(problem: &PlacementView, hints: &PlacementHints) -> PlaceResult {
    TunedPlacer.place(problem, hints, &route_probe(), &Budget::unlimited())
}

/// Apply a fully prescribed placement without the tuned search portfolio.
pub fn place_prescribed(problem: &PlacementView, hints: &PlacementHints) -> PlaceResult {
    pcb_place::placement::place(problem, hints)
}

/// Run the production routing portfolio and retain its per-pass diagnostics,
/// after scoping the view to the requested nets and turning its fixed copper
/// into obstacles.
pub fn route_tuned(problem: &RoutingView) -> pcb_route_mesh::pipeline::TunedRouteRun {
    route_prepared(&routing_problem(problem))
}

/// [`route_tuned`] on a view the caller has already prepared.
pub fn route_prepared(problem: &RoutingView) -> pcb_route_mesh::pipeline::TunedRouteRun {
    with_mesh_deps(Budget::unlimited(), |deps| {
        pcb_route_mesh::pipeline::route_tuned_with_diagnostics(deps, problem)
    })
}

/// Pre-route the wide multi-pin terminals that need a dedicated escape before
/// the main routing pass sees the board.
pub fn prepare_wide_terminal_escapes(
    problem: &RoutingView,
) -> (RoutingView, pcb_model::RouteSolution) {
    with_mesh_deps(Budget::unlimited(), |deps| {
        pcb_route_mesh::pipeline::prepare_wide_terminal_escapes(deps, problem)
    })
}

/// Freerouter-style postroute cleanup for selected copper, guarded by the
/// production DRC oracle.
pub fn postroute_cleanup(problem: &RoutingView, solution: &mut pcb_model::RouteSolution) {
    with_mesh_deps(Budget::unlimited(), |deps| {
        pcb_route_mesh::pipeline::postroute_cleanup(deps, problem, solution)
    })
}

/// Route `problem` with the baseline grid router — the rescue primitive, and the
/// engine an interactive tool re-routes a handful of nets with.
pub fn route_grid(problem: &RoutingView) -> pcb_model::RouteResult {
    GridRouter::new(&DRC).route(problem, &Budget::unlimited())
}

/// The production DRC report for an emitted solution.
pub fn check(problem: &RoutingView, solution: &pcb_model::RouteSolution) -> pcb_model::Findings {
    DRC.check(problem, solution)
}

/// The geometry-only violation count (clearance / width / via / bounds).
pub fn geometry_violations(
    problem: &RoutingView,
    solution: &pcb_model::RouteSolution,
) -> usize {
    DRC.geometry_violations(problem, solution)
}

/// The connectivity oracle's verdict on an emitted solution: what is actually
/// joined, and what is shorted — independent of any router's own bookkeeping.
pub fn connectivity(
    problem: &RoutingView,
    solution: &pcb_model::RouteSolution,
) -> Vec<pcb_model::Violation> {
    pcb_drc::connectivity::check(problem, solution)
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
