//! Grid A*: shortest net-aware path through a [`RouteGrid`] for one connection.
//!
//! State is 3-D `(layer, ix, iy)`. Moves:
//! - **Step** — to a 4-neighbour cell on the same layer, cost [`STEP_COST`],
//!   plus [`bend_cost`](AStarCosts::bend) when the heading changes from the
//!   move that entered the current cell (keeps routes straight).
//! - **Via** — change layer in place, cost [`AStarCosts::via`]. Allowed only
//!   where the cell is free for this connection on *every* layer (the
//!   conservative through-via barrel check; correct for the v1 2-layer scope).
//!
//! The heuristic is layer-agnostic Manhattan distance to the nearest target,
//! scaled by [`STEP_COST`] — admissible because the cheapest way to close one
//! cell of distance is a single straight step. With via and bend costs
//! non-negative and the heuristic never counting them, A* stays optimal.
//!
//! ## Determinism
//!
//! The open set is a max-heap of `Reverse((cost, heading, state))` so equal
//! `(cost, heading, state)` keys pop in a total, reproducible order; `g`-scores
//! and the came-from map are keyed by the dense state index. No hashing, no
//! float scores (all costs are integer grid units), no iteration over a
//! `HashMap` leaks into the result.

use crate::grid::RouteGrid;
use std::cmp::Reverse;
use std::collections::BinaryHeap;

/// Cost of one orthogonal grid step.
pub const STEP_COST: u32 = 1;

/// Tunable A* movement costs, in grid-step units.
#[derive(Debug, Clone, Copy)]
pub struct AStarCosts {
    /// Extra cost charged when a step changes heading (a corner/bend).
    pub bend: u32,
    /// Cost of a layer change (a via).
    pub via: u32,
}

impl Default for AStarCosts {
    fn default() -> Self {
        // Per the design constants: via ≈ 25 grid steps, bend ≈ 2 steps.
        Self { bend: 2, via: 25 }
    }
}

/// A grid cell on a specific layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct State {
    /// Copper layer index (0 = top).
    pub layer: usize,
    /// Cell column.
    pub ix: usize,
    /// Cell row.
    pub iy: usize,
}

/// Heading of the move that entered a cell, for bend accounting. Ordered so the
/// heap tie-break is total and reproducible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Heading {
    /// Start cell — no prior move, so the first step is never a bend.
    None,
    PlusX,
    MinusX,
    PlusY,
    MinusY,
    /// Arrived by a via (layer change) — the next planar step is not a bend.
    Via,
}

/// A* from any of `starts` to the nearest of `targets`, routing for connection
/// index `conn` over `grid`. Returns the cell path (start → target inclusive)
/// of minimum cost, or `None` when no target is reachable.
///
/// Both `starts` and `targets` are cell sets (a pad spans several cells); the
/// search seeds every start at cost 0 and stops at the first target popped.
pub fn search(
    grid: &RouteGrid,
    conn: usize,
    starts: &[State],
    targets: &[State],
    costs: AStarCosts,
) -> Option<Vec<State>> {
    if starts.is_empty() || targets.is_empty() {
        return None;
    }
    // Target cell set for O(1) goal tests and the multi-target heuristic.
    let target_cells: Vec<(usize, usize)> = {
        let mut v: Vec<(usize, usize)> = targets.iter().map(|t| (t.ix, t.iy)).collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    let is_target = |s: &State| targets.iter().any(|t| t == s);

    let plane = grid.nx * grid.ny;
    let n = plane * grid.layer_count;
    let sid = |s: &State| s.layer * plane + s.iy * grid.nx + s.ix;

    // g-score per state index; u32::MAX == unvisited. came_from stores the
    // predecessor state index (usize::MAX == none).
    let mut g = vec![u32::MAX; n];
    let mut came_from = vec![usize::MAX; n];

    // Heap entries: (f, g, heading, state). Reverse → min-heap by f, then by g,
    // then heading and state as total tie-breakers for determinism. Carrying g
    // lets us drop stale entries (a cheaper path to the state was finalized);
    // the heading rides along so a popped entry can charge bends correctly.
    let mut open: BinaryHeap<Reverse<(u32, u32, Heading, State)>> = BinaryHeap::new();

    let heuristic = |s: &State| -> u32 {
        target_cells
            .iter()
            .map(|&(tx, ty)| {
                let dx = s.ix.abs_diff(tx);
                let dy = s.iy.abs_diff(ty);
                ((dx + dy) as u32) * STEP_COST
            })
            .min()
            .unwrap_or(0)
    };

    for s in starts {
        if s.layer >= grid.layer_count || s.ix >= grid.nx || s.iy >= grid.ny {
            continue;
        }
        if !grid.is_free_for(s.layer, s.ix, s.iy, conn) {
            continue;
        }
        let id = sid(s);
        if g[id] != 0 {
            g[id] = 0;
            came_from[id] = usize::MAX;
            open.push(Reverse((heuristic(s), 0, Heading::None, *s)));
        }
    }

    while let Some(Reverse((_f, popped_g, heading, cur))) = open.pop() {
        let cur_id = sid(&cur);
        // Stale heap entry: a cheaper path to `cur` was already finalized.
        if popped_g > g[cur_id] {
            continue;
        }
        if is_target(&cur) {
            return Some(reconstruct(&came_from, cur, plane, grid.nx));
        }
        let g_cur = g[cur_id];

        // Planar steps.
        for (dir, dx, dy) in NEIGHBORS {
            let nx = cur.ix as isize + dx;
            let ny = cur.iy as isize + dy;
            if nx < 0 || ny < 0 || nx >= grid.nx as isize || ny >= grid.ny as isize {
                continue;
            }
            let next = State {
                layer: cur.layer,
                ix: nx as usize,
                iy: ny as usize,
            };
            if !grid.is_free_for(next.layer, next.ix, next.iy, conn) {
                continue;
            }
            let bend = if matches!(heading, Heading::None | Heading::Via) || heading == dir {
                0
            } else {
                costs.bend
            };
            let tentative = g_cur + STEP_COST + bend;
            relax(
                next, tentative, dir, cur_id, &mut g, &mut came_from, &mut open, &heuristic, sid,
            );
        }

        // Via: change layer in place, if the barrel is clear on every layer.
        if grid.layer_count > 1 && via_barrel_clear(grid, conn, cur.ix, cur.iy) {
            for layer in 0..grid.layer_count {
                if layer == cur.layer {
                    continue;
                }
                let next = State {
                    layer,
                    ix: cur.ix,
                    iy: cur.iy,
                };
                let tentative = g_cur + costs.via;
                relax(
                    next,
                    tentative,
                    Heading::Via,
                    cur_id,
                    &mut g,
                    &mut came_from,
                    &mut open,
                    &heuristic,
                    sid,
                );
            }
        }
    }

    None
}

/// 4-neighbourhood as `(heading, dx, dy)`.
const NEIGHBORS: [(Heading, isize, isize); 4] = [
    (Heading::PlusX, 1, 0),
    (Heading::MinusX, -1, 0),
    (Heading::PlusY, 0, 1),
    (Heading::MinusY, 0, -1),
];

/// Is the cell free for `conn` on *every* layer (through-via barrel check)?
fn via_barrel_clear(grid: &RouteGrid, conn: usize, ix: usize, iy: usize) -> bool {
    (0..grid.layer_count).all(|l| grid.is_free_for(l, ix, iy, conn))
}

/// Relax the edge into `next` with cost `tentative`, arriving by `arrive_dir`.
#[allow(clippy::too_many_arguments)]
fn relax<H, S>(
    next: State,
    tentative: u32,
    arrive_dir: Heading,
    cur_id: usize,
    g: &mut [u32],
    came_from: &mut [usize],
    open: &mut BinaryHeap<Reverse<(u32, u32, Heading, State)>>,
    heuristic: &H,
    sid: S,
) where
    H: Fn(&State) -> u32,
    S: Fn(&State) -> usize,
{
    let id = sid(&next);
    if tentative < g[id] {
        g[id] = tentative;
        came_from[id] = cur_id;
        let f = tentative.saturating_add(heuristic(&next));
        open.push(Reverse((f, tentative, arrive_dir, next)));
    }
}

/// Walk `came_from` back from `goal` to a start and reverse into a forward path.
fn reconstruct(came_from: &[usize], goal: State, plane: usize, nx: usize) -> Vec<State> {
    let unpack = |id: usize| -> State {
        let layer = id / plane;
        let rem = id % plane;
        State {
            layer,
            iy: rem / nx,
            ix: rem % nx,
        }
    };
    let mut path = vec![goal];
    let mut id = goal.layer * plane + goal.iy * nx + goal.ix;
    while came_from[id] != usize::MAX {
        id = came_from[id];
        path.push(unpack(id));
    }
    path.reverse();
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::problem::{Bounds, Connection, LayerRef, Obstacle, Point2, RoutePoint, RouteProblem};

    /// A wide-open board with two connections so cell tags exist for tests.
    fn open_problem() -> RouteProblem {
        RouteProblem {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles: vec![],
            connections: vec![
                conn("A", &[(2.0, 2.0, "top")]),
                conn("B", &[(2.0, 4.0, "top")]),
            ],
            bounds: Bounds {
                min_x: 0.0,
                max_x: 20.0,
                min_y: 0.0,
                max_y: 20.0,
            },
            clearance: 0.2,
            via_diameter: 0.6,
            via_drill: 0.3,
        }
    }

    fn conn(name: &str, pts: &[(f64, f64, &str)]) -> Connection {
        Connection {
            name: name.to_owned(),
            points_to_connect: pts
                .iter()
                .map(|&(x, y, l)| RoutePoint {
                    x,
                    y,
                    layer: LayerRef(l.to_owned()),
                })
                .collect(),
        }
    }

    fn pad(connected_to: &[&str], center: (f64, f64), w: f64, h: f64, layers: &[&str]) -> Obstacle {
        Obstacle {
            kind: "rect".to_owned(),
            layers: layers.iter().map(|l| LayerRef((*l).to_owned())).collect(),
            center: Point2 {
                x: center.0,
                y: center.1,
            },
            width: w,
            height: h,
            connected_to: connected_to.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    fn st(layer: usize, ix: usize, iy: usize) -> State {
        State { layer, ix, iy }
    }

    #[test]
    fn straight_route_on_empty_grid_has_no_bends() {
        let g = RouteGrid::build(&open_problem());
        let a = g.connection_index("A").unwrap();
        let (sx, sy) = g.cell_of(4.0, 10.0);
        let (tx, ty) = g.cell_of(14.0, 10.0);
        let path = search(
            &g,
            a,
            &[st(0, sx, sy)],
            &[st(0, tx, ty)],
            AStarCosts::default(),
        )
        .expect("reachable");
        assert_eq!(path.first(), Some(&st(0, sx, sy)));
        assert_eq!(path.last(), Some(&st(0, tx, ty)));
        // A straight horizontal run: constant y, monotonic x, no vias.
        assert!(path.iter().all(|s| s.iy == sy && s.layer == 0));
        assert_eq!(path.len(), tx - sx + 1);
    }

    #[test]
    fn detours_around_a_wall() {
        // A short vertical keepout wall blocks the direct path; going around
        // on the same layer is cheaper than a via, so A* must detour planarly.
        let mut p = open_problem();
        // Wall straddling y=10 but only ~3mm tall: a small detour beats a via.
        p.obstacles.push(pad(&[], (10.0, 10.0), 0.6, 3.0, &["top"]));
        let g = RouteGrid::build(&p);
        let a = g.connection_index("A").unwrap();
        let (sx, sy) = g.cell_of(4.0, 10.0);
        let (tx, ty) = g.cell_of(16.0, 10.0);
        let path = search(
            &g,
            a,
            &[st(0, sx, sy)],
            &[st(0, tx, ty)],
            AStarCosts::default(),
        )
        .expect("reachable around the wall");
        assert_eq!(path.last(), Some(&st(0, tx, ty)));
        // No cell of the path may be blocked for A.
        assert!(path.iter().all(|s| g.is_free_for(s.layer, s.ix, s.iy, a)));
        // It deviated from the straight line (touched a different row).
        assert!(path.iter().any(|s| s.iy != sy), "expected a vertical detour");
    }

    #[test]
    fn via_hop_when_a_layer_is_fully_walled() {
        // A full-height wall on the top layer forces a hop to the bottom layer.
        let mut p = open_problem();
        p.bounds = Bounds {
            min_x: 0.0,
            max_x: 20.0,
            min_y: 0.0,
            max_y: 20.0,
        };
        // Top-layer wall spanning the entire height, blocking all of column ~10.
        p.obstacles.push(pad(&[], (10.0, 10.0), 0.6, 22.0, &["top"]));
        let g = RouteGrid::build(&p);
        let a = g.connection_index("A").unwrap();
        let (sx, sy) = g.cell_of(4.0, 10.0);
        let (tx, ty) = g.cell_of(16.0, 10.0);
        let path = search(
            &g,
            a,
            &[st(0, sx, sy)],
            &[st(0, tx, ty)],
            AStarCosts::default(),
        )
        .expect("reachable via the bottom layer");
        // It must use the bottom layer at some point (a via hop happened).
        assert!(path.iter().any(|s| s.layer == 1), "expected a via to layer 1");
        assert!(path.iter().all(|s| g.is_free_for(s.layer, s.ix, s.iy, a)));
    }

    #[test]
    fn unreachable_returns_none() {
        // Wall off the entire board width on both layers around the target.
        let mut p = open_problem();
        p.obstacles
            .push(pad(&[], (10.0, 10.0), 0.6, 22.0, &["top", "bottom"]));
        let g = RouteGrid::build(&p);
        let a = g.connection_index("A").unwrap();
        let (sx, sy) = g.cell_of(4.0, 10.0);
        let (tx, ty) = g.cell_of(16.0, 10.0);
        assert!(search(
            &g,
            a,
            &[st(0, sx, sy)],
            &[st(0, tx, ty)],
            AStarCosts::default()
        )
        .is_none());
    }

    #[test]
    fn multi_target_reaches_nearest() {
        let g = RouteGrid::build(&open_problem());
        let a = g.connection_index("A").unwrap();
        let (sx, sy) = g.cell_of(4.0, 10.0);
        let (near_x, near_y) = g.cell_of(8.0, 10.0);
        let (far_x, far_y) = g.cell_of(18.0, 10.0);
        let path = search(
            &g,
            a,
            &[st(0, sx, sy)],
            &[st(0, near_x, near_y), st(0, far_x, far_y)],
            AStarCosts::default(),
        )
        .expect("reachable");
        // Stops at the nearer target.
        assert_eq!(path.last(), Some(&st(0, near_x, near_y)));
    }

    #[test]
    fn search_is_deterministic() {
        let g = RouteGrid::build(&open_problem());
        let a = g.connection_index("A").unwrap();
        let (sx, sy) = g.cell_of(4.0, 6.0);
        let (tx, ty) = g.cell_of(16.0, 14.0);
        let run = || {
            search(
                &g,
                a,
                &[st(0, sx, sy)],
                &[st(0, tx, ty)],
                AStarCosts::default(),
            )
        };
        let first = run().unwrap();
        for _ in 0..5 {
            assert_eq!(run().unwrap(), first, "A* must be deterministic");
        }
    }
}
