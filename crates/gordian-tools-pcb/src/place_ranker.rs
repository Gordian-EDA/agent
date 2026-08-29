use pcb_model::{RouteProblem, RouteResult, Router, failed_pad_weight};
use pcb_place_api::{RouteRankKey, RouteRanker};

pub struct GridRouteRanker;

const PORTFOLIO_MAX_TERMINALS: usize = 40;
const SINGLE_PASS_MAX_TERMINALS: usize = 48;

impl RouteRanker for GridRouteRanker {
    fn faults(&self, problem: &RouteProblem) -> usize {
        self.rank_key(problem).0
    }

    fn rank_key(&self, problem: &RouteProblem) -> RouteRankKey {
        let terminals = terminal_count(problem);
        if terminals > SINGLE_PASS_MAX_TERMINALS {
            return (0, 0, 0, 0, 0);
        }
        if terminals > PORTFOLIO_MAX_TERMINALS {
            return route_rank_key(
                problem,
                &pcb_route_grid::router::route_orthogonal_single_pass(problem),
            );
        }

        let strict = route_rank_key(problem, &pcb_route_grid::router::route_orthogonal(problem));
        if clean_via_free(strict) {
            return strict;
        }
        let lenient = route_rank_key(
            problem,
            &pcb_route_grid::router::route_orthogonal_lenient(problem),
        );
        let orthogonal = strict.min(lenient);
        if clean_via_free(orthogonal) {
            return orthogonal;
        }

        let max_connections = if orthogonal.0 > 0 { 6 } else { 4 };
        let max_terminals = if orthogonal.0 > 0 { 18 } else { 12 };
        if problem.connections.len() <= max_connections && terminals <= max_terminals {
            orthogonal.min(route_rank_key(
                problem,
                &pcb_route_grid::router::GridAStarRouter.route(problem),
            ))
        } else {
            orthogonal
        }
    }
}

fn terminal_count(problem: &RouteProblem) -> usize {
    problem
        .connections
        .iter()
        .map(|connection| connection.points_to_connect.len())
        .sum()
}

fn route_rank_key(problem: &RouteProblem, routed: &RouteResult) -> RouteRankKey {
    let geometry = pcb_drc::lint::lint(problem, &routed.solution)
        .iter()
        .filter(|finding| !matches!(finding, pcb_drc::DrcViolation::Connectivity { .. }))
        .count();
    let metrics = routed.solution.metrics();
    (
        failed_pad_weight(problem, &routed.failed) + geometry,
        geometry,
        routed.failed.len(),
        metrics.via_count,
        (metrics.wirelength * 1000.0).round() as u64,
    )
}

fn clean_via_free(key: RouteRankKey) -> bool {
    key.0 == 0 && key.3 == 0
}
