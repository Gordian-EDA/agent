//! Grid A*: shortest net-aware path through a [`RouteGrid`] for one connection.
//!
//! State is 3-D `(layer, ix, iy)`. Moves:
//! - **Step** — to a 4-neighbour cell on the same layer, cost [`STEP_COST`],
//!   plus [`bend_cost`](AStarCosts::bend) when the heading changes from the
//!   move that entered the current cell (keeps routes straight).
//! - **Diagonal step** — to one of the 4 diagonal neighbours on the same layer,
//!   cost [`AStarCosts::diag`] (= `ceil(√2 × STEP_COST) = 2` in the integer cost
//!   scale), enabled only when [`AStarCosts::moves`] is [`MoveSet::Octilinear`].
//!   *Corner-cutting is forbidden*: a diagonal into `(ix±1, iy±1)` is allowed
//!   only when **both** orthogonally-adjacent cells (`(ix±1, iy)` and
//!   `(ix, iy±1)`) are also free for this connection, so the 45° corner keeps
//!   the full clearance two abutting tracks would. Slice-1 callers default to
//!   [`MoveSet::Orthogonal`] and never see diagonals — their behaviour is
//!   bit-identical to before this move was added.
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

/// Cost of one diagonal grid step: `ceil(√2 × STEP_COST)` in the integer cost
/// scale. With `STEP_COST = 1` this is `2`, which is strictly cheaper than the
/// two orthogonal steps (`2 × STEP_COST`) plus the bend they would incur to
/// cover the same diagonal cell, so octilinear search prefers a true 45° run
/// over an orthogonal staircase. The Manhattan heuristic stays admissible: a
/// diagonal closes two cells of Manhattan distance for cost 2 (= 1 per cell),
/// never under-counting.
pub const DIAG_COST: u32 = 2;

/// Which neighbour moves the planar search may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveSet {
    /// 4-neighbour (N/S/E/W) only — the slice-1 baseline. Bit-identical to the
    /// router before diagonals existed.
    Orthogonal,
    /// 4 orthogonal + 4 diagonal neighbours (45° routing), corner-cutting
    /// forbidden. Used by the per-cell detailed router ([`crate::detail`]).
    Octilinear,
}

/// Tunable A* movement costs, in grid-step units.
#[derive(Debug, Clone, Copy)]
pub struct AStarCosts {
    /// Extra cost charged when a step changes heading (a corner/bend).
    pub bend: u32,
    /// Cost of a layer change (a via).
    pub via: u32,
    /// Cost of a single diagonal step (only used when `moves` is
    /// [`MoveSet::Octilinear`]). Defaults to [`DIAG_COST`].
    pub diag: u32,
    /// Which neighbour moves the planar search uses. Defaults to
    /// [`MoveSet::Orthogonal`] so slice-1 callers are unchanged.
    pub moves: MoveSet,
    /// Clearance radius (in grid cells) a via barrel must keep clear of foreign
    /// copper on *every* layer before the search may place a via at a cell. `0`
    /// (the default) means only the via cell itself is checked — bit-identical to
    /// the slice-1 behaviour. The detailed router sets this to the via-clearance
    /// halo in cells so a spontaneous mid-path via cannot land too close to a
    /// foreign trace already routed (the lint measures the via barrel in exact
    /// geometry).
    pub via_clear_radius_cells: usize,
    /// Whether the search may place vias (change layer). `true` (the default) keeps
    /// the slice-1 behaviour. The detailed finisher first tries a net with this
    /// `false`: a planar (no-via) A* skips the per-cell via-barrel clearance scan —
    /// the dominant cost of a full-board search on a multi-layer board — and succeeds
    /// for the common single-layer net; only if that fails does it retry with vias
    /// allowed. Correct either way (a net that needs a via just falls to the retry).
    pub allow_via: bool,
    /// Bitmask of PLANE layers (bit `l` set ⇒ layer `l` carries a solid GND/VCC
    /// plane). The search never *routes* on a plane layer — signal copper there would
    /// short to the plane — so the via step skips these layers as destinations; a via
    /// still passes THROUGH them (a through-via, anti-padded at synth). `0` (default)
    /// = every layer is a signal layer (2-layer / fixture default). This is what lets
    /// an inner BGA ball escape F→B instead of taking the cheaper F→In1 hop onto the
    /// plane and being dropped as a short.
    pub plane_mask: u32,
    /// Extra planar clearance (grid cells, per LAYER) the CURRENT net's trace needs
    /// beyond the min-width clearance already baked into the grid: `ceil((w-min)/2 /
    /// pitch)`. `0` (the default, every min-width net) means no scan — bit-identical to
    /// before. A wider power/HF net scans this radius so its fat copper keeps full
    /// clearance without inflating spacing for thin nets.
    pub trace_clear_radius_cells: usize,
    /// Bitmask of layers this net's search may ROUTE on. `0` (the default) means every
    /// layer is allowed — bit-identical to before. A BGA inner-ball escape sets this to
    /// `{top, bottom, its assigned escape layer}` so its A* is a 3-layer problem and the
    /// ring→layer assignment holds (a free choice over all 8 layers self-blocks on the
    /// via field and blows up runtime — measured). The plane mask still applies on top:
    /// a layer in this mask that is also a plane is never a routing destination.
    pub layer_mask: u32,
}

impl Default for AStarCosts {
    fn default() -> Self {
        // Per the design constants: via ≈ 25 grid steps, bend ≈ 2 steps. The
        // default move set is orthogonal so the slice-1 router is unchanged.
        Self {
            bend: 2,
            via: 25,
            diag: DIAG_COST,
            moves: MoveSet::Orthogonal,
            via_clear_radius_cells: 0,
            allow_via: true,
            plane_mask: 0,
            trace_clear_radius_cells: 0,
            layer_mask: 0,
        }
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
    /// Diagonal headings (octilinear search only).
    PlusXPlusY,
    PlusXMinusY,
    MinusXPlusY,
    MinusXMinusY,
    /// Arrived by a via (layer change) — the next planar step is not a bend.
    Via,
}

/// An inclusive cell-index rectangle the planar search may not leave. Used by the
/// detailed router to confine a job's A* to its leaf window while the underlying
/// grid spans the whole board (shared cross-cell occupancy).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellBounds {
    pub ix0: usize,
    pub iy0: usize,
    pub ix1: usize,
    pub iy1: usize,
}

impl CellBounds {
    #[inline]
    fn contains(&self, ix: usize, iy: usize) -> bool {
        ix >= self.ix0 && ix <= self.ix1 && iy >= self.iy0 && iy <= self.iy1
    }
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
    search_bounded(grid, conn, starts, targets, costs, None)
}

/// [`search`], optionally confined to a [`CellBounds`] cell rectangle: planar
/// moves whose target cell lies outside `bounds` are pruned (via moves stay in
/// place, so they are always allowed). With `bounds == None` this is exactly
/// [`search`] — slice-1 callers pass `None` (via the [`search`] wrapper) and are
/// bit-identical to before this parameter existed.
pub fn search_bounded(
    grid: &RouteGrid,
    conn: usize,
    starts: &[State],
    targets: &[State],
    costs: AStarCosts,
    bounds: Option<CellBounds>,
) -> Option<Vec<State>> {
    if starts.is_empty() || targets.is_empty() {
        return None;
    }
    let in_bounds = |ix: usize, iy: usize| bounds.is_none_or(|b| b.contains(ix, iy));
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

        // Orthogonal planar steps.
        for (dir, dx, dy) in NEIGHBORS {
            let nx = cur.ix as isize + dx;
            let ny = cur.iy as isize + dy;
            if nx < 0 || ny < 0 || nx >= grid.nx as isize || ny >= grid.ny as isize {
                continue;
            }
            if !in_bounds(nx as usize, ny as usize) {
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
            // A wider-than-min net keeps its extra half-width clear of foreign copper.
            if costs.trace_clear_radius_cells > 0
                && !planar_clear(grid, conn, next.layer, next.ix, next.iy, costs.trace_clear_radius_cells)
            {
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

        // Diagonal planar steps (octilinear search only). Corner-cutting is
        // forbidden: a diagonal is allowed only when both orthogonally-adjacent
        // cells are free for this connection, keeping the 45° corner clear.
        if costs.moves == MoveSet::Octilinear {
            for (dir, dx, dy) in DIAGONALS {
                let nx = cur.ix as isize + dx;
                let ny = cur.iy as isize + dy;
                if nx < 0 || ny < 0 || nx >= grid.nx as isize || ny >= grid.ny as isize {
                    continue;
                }
                if !in_bounds(nx as usize, ny as usize) {
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
                if costs.trace_clear_radius_cells > 0
                    && !planar_clear(grid, conn, next.layer, next.ix, next.iy, costs.trace_clear_radius_cells)
                {
                    continue;
                }
                // Both orthogonal neighbours bridging this diagonal must be free
                // (no corner-cutting through a blocked orthogonal pair).
                let side_x = grid.is_free_for(
                    cur.layer,
                    (cur.ix as isize + dx) as usize,
                    cur.iy,
                    conn,
                );
                let side_y = grid.is_free_for(
                    cur.layer,
                    cur.ix,
                    (cur.iy as isize + dy) as usize,
                    conn,
                );
                if !side_x || !side_y {
                    continue;
                }
                let bend = if matches!(heading, Heading::None | Heading::Via) || heading == dir {
                    0
                } else {
                    costs.bend
                };
                let tentative = g_cur + costs.diag + bend;
                relax(
                    next, tentative, dir, cur_id, &mut g, &mut came_from, &mut open, &heuristic, sid,
                );
            }
        }

        // Via: change layer in place, if the barrel — and its clearance halo — is
        // clear on every layer. Skipped entirely when the caller forbids vias (a
        // single-layer net), which also skips the per-cell barrel-clearance scan.
        if costs.allow_via
            && grid.layer_count > 1
            && via_barrel_clear(grid, conn, cur.ix, cur.iy, costs.via_clear_radius_cells)
        {
            for layer in 0..grid.layer_count {
                if layer == cur.layer {
                    continue;
                }
                // Never route ON a plane layer (a signal there shorts to the plane);
                // a via still tunnels through it to reach the far signal layer.
                if costs.plane_mask & (1u32 << layer) != 0 {
                    continue;
                }
                // A net with a restricted layer set (a forced BGA escape) may only land
                // a via on one of its allowed layers; a via still tunnels THROUGH the
                // others. `0` = unrestricted (the default).
                if costs.layer_mask != 0 && costs.layer_mask & (1u32 << layer) == 0 {
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

/// 4 diagonal neighbours as `(heading, dx, dy)` (octilinear search only).
const DIAGONALS: [(Heading, isize, isize); 4] = [
    (Heading::PlusXPlusY, 1, 1),
    (Heading::PlusXMinusY, 1, -1),
    (Heading::MinusXPlusY, -1, 1),
    (Heading::MinusXMinusY, -1, -1),
];

/// Is the cell free for `conn` on *every* layer (through-via barrel check), and —
/// when `radius_cells > 0` — is every cell whose centre lies within `radius_cells`
/// (Euclidean) also free for `conn` on every layer? The Euclidean disc (not a
/// Chebyshev box) enforces a via's clearance halo at its true geometry, so a
/// spontaneous via cannot sit too close to foreign copper without over-blocking the
/// box corners.
/// Is every cell within `radius_cells` (Euclidean) of `(ix,iy)` on `layer` free for
/// `conn`? The single-layer analog of [`via_barrel_clear`]: a wider-than-min trace must
/// keep its extra half-width clear of foreign copper on its OWN layer (a trace lives on
/// one layer, unlike a through via). Off-board reads blocked — fat copper may not poke
/// past the edge. `conn`'s own copper reads free, so a trace runs freely along itself.
fn planar_clear(grid: &RouteGrid, conn: usize, layer: usize, ix: usize, iy: usize, radius_cells: usize) -> bool {
    let r = radius_cells as isize;
    let r2 = (radius_cells * radius_cells) as isize;
    for dy in -r..=r {
        for dx in -r..=r {
            if dx * dx + dy * dy > r2 {
                continue;
            }
            let hx = ix as isize + dx;
            let hy = iy as isize + dy;
            if hx < 0 || hy < 0 || hx >= grid.nx as isize || hy >= grid.ny as isize {
                return false;
            }
            if !grid.is_free_for(layer, hx as usize, hy as usize, conn) {
                return false;
            }
        }
    }
    true
}

fn via_barrel_clear(grid: &RouteGrid, conn: usize, ix: usize, iy: usize, radius_cells: usize) -> bool {
    let r = radius_cells as isize;
    let r2 = (radius_cells * radius_cells) as isize;
    for dy in -r..=r {
        for dx in -r..=r {
            if dx * dx + dy * dy > r2 {
                continue; // outside the Euclidean clearance disc
            }
            let hx = ix as isize + dx;
            let hy = iy as isize + dy;
            if hx < 0 || hy < 0 || hx >= grid.nx as isize || hy >= grid.ny as isize {
                // Off-grid (off-board) reads blocked: a via barrel may not poke
                // past the board edge.
                return false;
            }
            if !(0..grid.layer_count).all(|l| grid.is_free_for(l, hx as usize, hy as usize, conn)) {
                return false;
            }
        }
    }
    true
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
            net_widths: Default::default(),
            outline: None,
            escape_layers: Default::default(),
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
    fn octilinear_uses_a_diagonal_when_cheaper() {
        // A corner-to-corner route on an open board: an octilinear search should
        // take the 45° diagonal (a step that changes BOTH ix and iy at once),
        // which an orthogonal search can never do.
        let g = RouteGrid::build(&open_problem());
        let a = g.connection_index("A").unwrap();
        let (sx, sy) = g.cell_of(4.0, 4.0);
        let (tx, ty) = g.cell_of(14.0, 14.0);
        let costs = AStarCosts {
            moves: MoveSet::Octilinear,
            ..AStarCosts::default()
        };
        let path = search(&g, a, &[st(0, sx, sy)], &[st(0, tx, ty)], costs)
            .expect("reachable diagonally");
        assert_eq!(path.first(), Some(&st(0, sx, sy)));
        assert_eq!(path.last(), Some(&st(0, tx, ty)));
        // At least one step moves diagonally (both coordinates change together).
        let has_diagonal = path
            .windows(2)
            .any(|w| w[0].ix != w[1].ix && w[0].iy != w[1].iy);
        assert!(has_diagonal, "octilinear search must use a 45° diagonal step");
    }

    #[test]
    fn orthogonal_never_uses_a_diagonal() {
        // The same corner-to-corner route with the default (orthogonal) move set
        // must stay on the 4-neighbour grid — no step changes both coordinates.
        let g = RouteGrid::build(&open_problem());
        let a = g.connection_index("A").unwrap();
        let (sx, sy) = g.cell_of(4.0, 4.0);
        let (tx, ty) = g.cell_of(14.0, 14.0);
        let path = search(&g, a, &[st(0, sx, sy)], &[st(0, tx, ty)], AStarCosts::default())
            .expect("reachable orthogonally");
        assert!(
            path.windows(2)
                .all(|w| (w[0].ix == w[1].ix) || (w[0].iy == w[1].iy)),
            "orthogonal search must not move diagonally"
        );
    }

    #[test]
    fn corner_cutting_is_forbidden() {
        // Build an L-shaped wall so the only diagonal that would close the gap
        // has a BLOCKED orthogonal neighbour on one side — corner-cutting. The
        // search must refuse that diagonal and detour, never slip through the
        // corner. We block the two cells orthogonally adjacent to a diagonal hop
        // and assert no path step cuts the blocked corner.
        let mut p = open_problem();
        // A blocking keepout column just right of the start, leaving a diagonal
        // gap at its corner. Place a small keepout so that the cell directly to
        // the +x of a key cell is blocked while the diagonal cell is free.
        p.obstacles
            .push(pad(&[], (8.0, 8.0), 0.6, 0.6, &["top"]));
        let g = RouteGrid::build(&p);
        let a = g.connection_index("A").unwrap();
        let costs = AStarCosts {
            moves: MoveSet::Octilinear,
            ..AStarCosts::default()
        };
        let (bx, by) = g.cell_of(8.0, 8.0); // a blocked cell (keepout, inflated)
        let (sx, sy) = g.cell_of(4.0, 8.0);
        let (tx, ty) = g.cell_of(14.0, 8.0);
        let path = search(&g, a, &[st(0, sx, sy)], &[st(0, tx, ty)], costs)
            .expect("reachable around the keepout");
        // No two consecutive path cells may form a diagonal that cuts across the
        // blocked cell (bx,by): that is, a diagonal whose shared orthogonal
        // neighbour is the blocked cell is illegal.
        for w in path.windows(2) {
            let (c, n) = (w[0], w[1]);
            let diagonal = c.ix != n.ix && c.iy != n.iy && c.layer == n.layer;
            if diagonal {
                // The two orthogonal bridge cells of this diagonal.
                let o1 = (n.ix, c.iy);
                let o2 = (c.ix, n.iy);
                assert!(
                    o1 != (bx, by) && o2 != (bx, by),
                    "diagonal {c:?}->{n:?} cut the blocked corner ({bx},{by})"
                );
                // And both bridge cells must actually be free for A.
                assert!(
                    g.is_free_for(0, o1.0, o1.1, a) && g.is_free_for(0, o2.0, o2.1, a),
                    "diagonal {c:?}->{n:?} crossed a non-free orthogonal neighbour"
                );
            }
        }
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
