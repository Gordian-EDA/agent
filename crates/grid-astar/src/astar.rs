//! Grid A*: shortest net-aware path through a [`RouteGrid`] for one connection.
//!
//! One generic 8-way (KiCad `DIRECTION_45`-style) planar enumerator,
//! [`MOVES8`], drives every search; orthogonal routing is the degenerate case
//! where the 4 diagonals are masked off by an infinite cost — there is no
//! separate code path.
//!
//! State is 3-D `(layer, ix, iy)`. Moves:
//! - **Step** — to one of the 8 same-layer neighbours in [`MOVES8`]. An
//!   orthogonal step costs [`STEP_COST`]; a diagonal step costs
//!   [`AStarCosts::diag`] (= `ceil(√2 × STEP_COST) = 2` in the integer cost
//!   scale). Either kind adds [`bend`](AStarCosts::bend) when the heading
//!   changes from the move that entered the current cell (keeps routes
//!   straight). Diagonals are *enabled* iff `costs.diag != u32::MAX`; the
//!   sentinel `u32::MAX` ("diagonals disabled") is the orthogonal mode and is
//!   pruned before any cost arithmetic. *Corner-cutting is forbidden*: a
//!   diagonal into `(ix±1, iy±1)` is allowed only when **both**
//!   orthogonally-adjacent cells (`(ix±1, iy)` and `(ix, iy±1)`) are also free
//!   for this connection, so the 45° corner keeps the full clearance two
//!   abutting tracks would. A diagonal additionally passes a **swept-body
//!   clearance** check ([`diag_body_clear`]) when
//!   [`AStarCosts::diag_body_radius_cells`] is set: the 45° segment between the
//!   two cell centres keeps its whole body clear of foreign copper, so two
//!   parallel diagonal runs one pitch apart (perpendicular spacing `pitch/√2`)
//!   can never dip under clearance — the corner guard alone protects only the
//!   cell centres and the corner, not the segment body. Orthogonal-mode callers
//!   ([`AStarCosts::default`]) set `diag = u32::MAX` and never see diagonals —
//!   their behaviour is bit-identical to a 4-neighbour search.
//! - **Via** — change layer in place, cost [`AStarCosts::via`]. Allowed only
//!   where the cell is free for this connection on *every* layer (the
//!   conservative through-via barrel check; correct for the v1 2-layer scope).
//!
//! The heuristic is Manhattan distance to the nearest target on each layer,
//! scaled by [`STEP_COST`], plus one via lower bound when the cheapest target is
//! on another layer and vias are enabled. It never counts bends and never counts
//! more than one via (the search can jump to any signal layer in one through-via),
//! so it remains admissible while avoiding a zero heuristic directly above a
//! different-layer target.
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

/// Above this many distinct target cells, precompute an exact L1 distance field
/// instead of scanning every target on each heuristic call. Multi-point nets grow
/// a routed tree, so the target set can become hundreds of cells; the distance
/// field is the same Manhattan heuristic at O(1) per state.
const TARGET_DISTANCE_FIELD_THRESHOLD: usize = 32;

/// Tunable A* movement costs, in grid-step units.
#[derive(Debug, Clone, Copy)]
pub struct AStarCosts {
    /// Extra cost charged when a step changes heading (a corner/bend).
    pub bend: u32,
    /// Cost of a layer change (a via).
    pub via: u32,
    /// Cost of a single diagonal step, **and** the diagonal enable switch. A
    /// finite value (e.g. [`DIAG_COST`]) enables octilinear (8-way) routing; the
    /// sentinel `u32::MAX` means "diagonals disabled" — orthogonal (4-way) mode,
    /// where each diagonal move in [`MOVES8`] is pruned before any cost
    /// arithmetic, so no diagonal edge is ever relaxed and the search is
    /// bit-identical to a pure 4-neighbour A*. [`AStarCosts::default`] is
    /// `u32::MAX` (orthogonal); the per-cell detailed router sets [`DIAG_COST`].
    pub diag: u32,
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
    /// Swept-body clearance radius (grid cells) a DIAGONAL step must keep clear of foreign
    /// copper along its whole 45° segment — the centre-to-centre keep-out
    /// `(clearance + w_this/2 + min_w/2) / pitch`, as an `f64` for sub-cell precision.
    /// `0.0` (the default, and every orthogonal caller) means no swept check.
    ///
    /// This closes the diagonal corner-cutting model's structural gap: the per-cell
    /// occupancy + Euclidean corner guard protect cell *centres* and the 45° corner, but
    /// NOT the swept body between two diagonal cell centres — two parallel 45° runs one
    /// pitch apart sit only `pitch/√2` apart, well under clearance (a real DRC short). The
    /// 8-way naive router (which marks Chebyshev keep-out halos, [`RouteGrid::mark_net_halo`])
    /// sets this to the net's keep-out radius so a diagonal step is taken only when every
    /// cell whose centre lies strictly within that radius of the segment is free for this
    /// net — the discrete analog of KiCad's segment-to-segment clearance / FreeRouting's
    /// trace-hull offset (see [`diag_body_clear`]). The detailed finisher closes the same
    /// gap by MARKING each routed segment's full swept capsule into its grid
    /// (`mark_segment_capsule`), so its later nets see the body through ordinary occupancy
    /// and it leaves this `0.0`. Orthogonal callers leave it `0.0` (with `diag = u32::MAX`),
    /// so no diagonal is ever relaxed and the field is inert.
    pub diag_body_radius_cells: f64,
}

impl Default for AStarCosts {
    fn default() -> Self {
        // Per the design constants: via ≈ 25 grid steps, bend ≈ 2 steps.
        // `diag = u32::MAX` disables diagonals (orthogonal mode), so the
        // default router is a pure 4-neighbour search, unchanged from slice 1.
        Self {
            bend: 2,
            via: 25,
            diag: u32::MAX,
            via_clear_radius_cells: 0,
            allow_via: true,
            plane_mask: 0,
            trace_clear_radius_cells: 0,
            layer_mask: 0,
            diag_body_radius_cells: 0.0,
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

/// One same-layer neighbour move: the heading it arrives by, its cell delta,
/// and whether it is a diagonal (`dx != 0 && dy != 0`, the grid analog of
/// KiCad `DIRECTION_45::IsDiagonal`).
struct Move8 {
    dir: Heading,
    dx: isize,
    dy: isize,
    diagonal: bool,
}

/// The 8 planar neighbour moves, **4 orthogonal then 4 diagonal**. This order
/// is load-bearing: it is the exact visitation order of the former two-loop
/// (orthogonal-then-diagonal) search and matches the [`Heading`] `Ord`, so the
/// `(f, g, heading, state)` heap tie-break — and therefore the chosen path —
/// is unchanged. Do **not** reorder or interleave. Orthogonal mode
/// (`costs.diag == u32::MAX`) prunes the 4 diagonal entries before any cost
/// arithmetic, leaving the 4 orthogonal entries visited exactly as a
/// 4-neighbour search would.
const MOVES8: [Move8; 8] = [
    Move8 {
        dir: Heading::PlusX,
        dx: 1,
        dy: 0,
        diagonal: false,
    },
    Move8 {
        dir: Heading::MinusX,
        dx: -1,
        dy: 0,
        diagonal: false,
    },
    Move8 {
        dir: Heading::PlusY,
        dx: 0,
        dy: 1,
        diagonal: false,
    },
    Move8 {
        dir: Heading::MinusY,
        dx: 0,
        dy: -1,
        diagonal: false,
    },
    Move8 {
        dir: Heading::PlusXPlusY,
        dx: 1,
        dy: 1,
        diagonal: true,
    },
    Move8 {
        dir: Heading::PlusXMinusY,
        dx: 1,
        dy: -1,
        diagonal: true,
    },
    Move8 {
        dir: Heading::MinusXPlusY,
        dx: -1,
        dy: 1,
        diagonal: true,
    },
    Move8 {
        dir: Heading::MinusXMinusY,
        dx: -1,
        dy: -1,
        diagonal: true,
    },
];

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
    let plane = grid.nx * grid.ny;
    let n = plane * grid.layer_count;
    let sid = |s: &State| s.layer * plane + s.iy * grid.nx + s.ix;

    let target_states: Vec<State> = targets
        .iter()
        .copied()
        .filter(|t| t.layer < grid.layer_count && t.ix < grid.nx && t.iy < grid.ny)
        .collect();

    // Exact layer-aware goal lookup. The old `targets.iter().any(...)` made every
    // popped state pay O(targets), which hurts tree routing where the target set is
    // every already-routed cell of the net.
    let mut target_state = vec![false; n];
    for t in &target_states {
        target_state[sid(t)] = true;
    }
    let is_target = |s: &State| target_state[sid(s)];

    let target_dist = (target_states.len() >= TARGET_DISTANCE_FIELD_THRESHOLD).then(|| {
        target_distance_fields_by_layer(grid.layer_count, grid.nx, grid.ny, &target_states)
    });
    let trace_clear_offsets = disc_offsets(costs.trace_clear_radius_cells);
    let via_clear_offsets = disc_offsets(costs.via_clear_radius_cells);

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
        target_heuristic(
            s,
            &target_states,
            target_dist.as_deref(),
            grid.nx,
            costs,
            grid.layer_count,
        )
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

        // Planar steps over the 8-way neighbourhood. Orthogonal cost STEP_COST;
        // diagonal cost `costs.diag`, with `u32::MAX` disabling diagonals
        // (orthogonal mode) — pruned here, before any bounds/clearance/relax, so
        // a disabled diagonal never perturbs a g-score or the heap.
        for m in &MOVES8 {
            let base = if m.diagonal { costs.diag } else { STEP_COST };
            if base == u32::MAX {
                continue;
            }
            let nx = cur.ix as isize + m.dx;
            let ny = cur.iy as isize + m.dy;
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
                && !planar_clear(
                    grid,
                    conn,
                    next.layer,
                    next.ix,
                    next.iy,
                    &trace_clear_offsets,
                )
            {
                continue;
            }
            // Diagonal-only guards. (1) Corner-cutting forbidden: a diagonal into
            // `(ix+dx, iy+dy)` is allowed only when both bridging orthogonal cells are
            // free for this connection, so the 45° corner keeps full clearance.
            // (2) Swept-body clearance: the segment between two diagonal cell centres
            // must keep its whole 45° body clear of foreign copper — the per-cell
            // occupancy + corner guard protect cell centres and the corner, but two
            // parallel 45° runs one pitch apart dip to `pitch/√2` apart (a DRC short).
            // Orthogonal moves are axis-aligned and already covered by the grid
            // inflation / halo, so neither guard applies to them.
            if m.diagonal {
                let side_x =
                    grid.is_free_for(cur.layer, (cur.ix as isize + m.dx) as usize, cur.iy, conn);
                let side_y =
                    grid.is_free_for(cur.layer, cur.ix, (cur.iy as isize + m.dy) as usize, conn);
                if !side_x || !side_y {
                    continue;
                }
                if costs.diag_body_radius_cells > 0.0
                    && !diag_body_clear(
                        grid,
                        conn,
                        cur.layer,
                        cur.ix,
                        cur.iy,
                        m.dx,
                        m.dy,
                        costs.diag_body_radius_cells,
                    )
                {
                    continue;
                }
            }
            let bend = if matches!(heading, Heading::None | Heading::Via) || heading == m.dir {
                0
            } else {
                costs.bend
            };
            let tentative = g_cur.saturating_add(base).saturating_add(bend);
            relax(
                next,
                tentative,
                m.dir,
                cur_id,
                &mut g,
                &mut came_from,
                &mut open,
                &heuristic,
                sid,
            );
        }

        // Via: change layer in place, if the barrel — and its clearance halo — is
        // clear on every layer. Skipped entirely when the caller forbids vias (a
        // single-layer net), which also skips the per-cell barrel-clearance scan.
        if costs.allow_via
            && grid.layer_count > 1
            && via_barrel_clear(grid, conn, cur.ix, cur.iy, &via_clear_offsets)
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

fn target_heuristic(
    s: &State,
    targets: &[State],
    target_dist_by_layer: Option<&[Option<Vec<u32>>]>,
    nx: usize,
    costs: AStarCosts,
    layer_count: usize,
) -> u32 {
    if let Some(fields) = target_dist_by_layer {
        let id = s.iy * nx + s.ix;
        return fields
            .iter()
            .enumerate()
            .filter_map(|(target_layer, dist)| {
                dist.as_ref().map(|dist| {
                    dist[id]
                        .saturating_mul(STEP_COST)
                        .saturating_add(target_layer_penalty(
                            s.layer,
                            target_layer,
                            costs,
                            layer_count,
                        ))
                })
            })
            .min()
            .unwrap_or(0);
    }

    targets
        .iter()
        .map(|target| {
            let dx = s.ix.abs_diff(target.ix);
            let dy = s.iy.abs_diff(target.iy);
            ((dx + dy) as u32)
                .saturating_mul(STEP_COST)
                .saturating_add(target_layer_penalty(
                    s.layer,
                    target.layer,
                    costs,
                    layer_count,
                ))
        })
        .min()
        .unwrap_or(0)
}

fn target_layer_penalty(
    state_layer: usize,
    target_layer: usize,
    costs: AStarCosts,
    layer_count: usize,
) -> u32 {
    if state_layer == target_layer || !costs.allow_via || layer_count < 2 {
        0
    } else {
        costs.via
    }
}

fn target_distance_fields_by_layer(
    layer_count: usize,
    nx: usize,
    ny: usize,
    targets: &[State],
) -> Vec<Option<Vec<u32>>> {
    let mut by_layer: Vec<Vec<(usize, usize)>> = vec![Vec::new(); layer_count];
    for target in targets {
        if target.layer < layer_count {
            by_layer[target.layer].push((target.ix, target.iy));
        }
    }
    by_layer
        .into_iter()
        .map(|mut cells| {
            cells.sort_unstable();
            cells.dedup();
            (!cells.is_empty()).then(|| manhattan_target_distances(nx, ny, &cells))
        })
        .collect()
}

/// Exact Manhattan distance from every grid cell to the nearest target cell,
/// ignoring obstacles/layers. This is the same admissible heuristic as scanning
/// `target_cells`, computed once with a two-pass L1 distance transform.
fn manhattan_target_distances(nx: usize, ny: usize, target_cells: &[(usize, usize)]) -> Vec<u32> {
    let inf = u32::MAX / 4;
    let mut dist = vec![inf; nx * ny];
    for &(tx, ty) in target_cells {
        if tx < nx && ty < ny {
            dist[ty * nx + tx] = 0;
        }
    }

    for y in 0..ny {
        for x in 0..nx {
            let id = y * nx + x;
            if x > 0 {
                dist[id] = dist[id].min(dist[id - 1].saturating_add(1));
            }
            if y > 0 {
                dist[id] = dist[id].min(dist[id - nx].saturating_add(1));
            }
        }
    }
    for y in (0..ny).rev() {
        for x in (0..nx).rev() {
            let id = y * nx + x;
            if x + 1 < nx {
                dist[id] = dist[id].min(dist[id + 1].saturating_add(1));
            }
            if y + 1 < ny {
                dist[id] = dist[id].min(dist[id + nx].saturating_add(1));
            }
        }
    }
    dist
}

/// Is the swept body of a DIAGONAL step from cell `(ix, iy)` to `(ix+dx, iy+dy)` clear of
/// foreign copper? `dx, dy ∈ {±1}`. Every cell on `layer` whose centre lies strictly
/// within `radius_cells` (Euclidean, point-to-segment, in cell units) of the segment must
/// be free for `conn`; a cell exactly `radius_cells` away is legal (it sits at the
/// centre-to-centre clearance), matching the DRC `seg_seg` rule.
///
/// This is the grid analog of KiCad's segment-to-segment clearance (min centreline
/// distance ≥ clearance + ½wₐ + ½w_b) and FreeRouting's trace-hull offset: the segment is
/// a capsule of radius `radius_cells`, and no foreign-occupiable cell centre may fall
/// inside it. With `radius_cells = (clearance + w_this/2 + min_w/2) / pitch`, any
/// min-width foreign trace the grid could legally place then keeps full clearance from
/// this diagonal's body — closing the gap where two parallel 45° runs one pitch apart
/// (perpendicular spacing `pitch/√2`) would short. A WIDER-than-min foreign trace is kept
/// clear by *its own* keep-out halo ([`RouteGrid::mark_net_halo`], sized to its width):
/// its cells read non-free here, so the body scan excludes them too — this radius bounds
/// the min-width case, the foreign halo covers the extra foreign half-width. Off-board
/// reads block (copper may not poke past the edge); `conn`'s own copper reads free, so a
/// route runs freely along itself.
///
/// The 8-way naive router calls this at search time (it marks Chebyshev halos, not the
/// swept body); the detailed finisher closes the identical diagonal-body gap by marking
/// each routed segment's capsule into its grid instead, so it does not call this.
#[allow(clippy::too_many_arguments)] // flat grid-coordinate args mirror `is_free_for`
pub fn diag_body_clear(
    grid: &RouteGrid,
    conn: usize,
    layer: usize,
    ix: usize,
    iy: usize,
    dx: isize,
    dy: isize,
    radius_cells: f64,
) -> bool {
    // Work in cell-offset space from `(ix, iy)`: the segment runs (0,0) → (dx, dy) and a
    // candidate cell offset `(ox, oy)` has its centre at `(ox, oy)`. All distances scale
    // uniformly with pitch, so comparing the offset point-to-segment distance against
    // `radius_cells` is exact. Scan the Chebyshev box that bounds the capsule.
    let r = radius_cells.ceil() as isize;
    let thresh = radius_cells - 1e-9; // strictly-inside (exactly-at-clearance is legal)
    let bx0 = dx.min(0) - r;
    let bx1 = dx.max(0) + r;
    let by0 = dy.min(0) - r;
    let by1 = dy.max(0) + r;
    let body = geom::Segment::new(
        geom::Point2::new(0.0, 0.0),
        geom::Point2::new(dx as f64, dy as f64),
    );
    for ox in bx0..=bx1 {
        for oy in by0..=by1 {
            if body.dist_to_point(geom::Point2::new(ox as f64, oy as f64)) >= thresh {
                continue;
            }
            let hx = ix as isize + ox;
            let hy = iy as isize + oy;
            if hx < 0 || hy < 0 || hx >= grid.nx as isize || hy >= grid.ny as isize {
                return false; // off-board: the diagonal body may not poke past the edge
            }
            if !grid.is_free_for(layer, hx as usize, hy as usize, conn) {
                return false;
            }
        }
    }
    true
}

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
fn planar_clear(
    grid: &RouteGrid,
    conn: usize,
    layer: usize,
    ix: usize,
    iy: usize,
    offsets: &[(isize, isize)],
) -> bool {
    for &(dx, dy) in offsets {
        let hx = ix as isize + dx;
        let hy = iy as isize + dy;
        if hx < 0 || hy < 0 || hx >= grid.nx as isize || hy >= grid.ny as isize {
            return false;
        }
        if !grid.is_free_for(layer, hx as usize, hy as usize, conn) {
            return false;
        }
    }
    true
}

fn via_barrel_clear(
    grid: &RouteGrid,
    conn: usize,
    ix: usize,
    iy: usize,
    offsets: &[(isize, isize)],
) -> bool {
    for &(dx, dy) in offsets {
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
    true
}

fn disc_offsets(radius_cells: usize) -> Vec<(isize, isize)> {
    let r = radius_cells as isize;
    let r2 = (radius_cells * radius_cells) as isize;
    let mut offsets = Vec::new();
    for dy in -r..=r {
        for dx in -r..=r {
            if dx * dx + dy * dy > r2 {
                continue;
            }
            offsets.push((dx, dy));
        }
    }
    offsets
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
    use crate::problem::{Connection, LayerRef, Obstacle, Point2, Rect, RoutePoint, RouteProblem};

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
            bounds: Rect {
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
            plane_nets: Default::default(),
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
    fn target_distance_field_matches_exact_manhattan_scan() {
        let targets = vec![(0, 0), (6, 2), (3, 5)];
        let nx = 8;
        let ny = 7;
        let dist = manhattan_target_distances(nx, ny, &targets);
        for y in 0..ny {
            for x in 0..nx {
                let exact = targets
                    .iter()
                    .map(|&(tx, ty)| x.abs_diff(tx) + y.abs_diff(ty))
                    .min()
                    .unwrap() as u32;
                assert_eq!(dist[y * nx + x], exact, "cell ({x},{y})");
            }
        }
    }

    #[test]
    fn target_heuristic_charges_one_via_for_other_layer_target() {
        let costs = AStarCosts {
            via: 25,
            ..AStarCosts::default()
        };
        let targets = vec![st(1, 3, 4)];

        assert_eq!(
            target_heuristic(&st(1, 3, 4), &targets, None, 8, costs, 2),
            0,
            "same-layer target cell has zero heuristic"
        );
        assert_eq!(
            target_heuristic(&st(0, 3, 4), &targets, None, 8, costs, 2),
            25,
            "same x/y but different layer still needs one via"
        );
        assert_eq!(
            target_heuristic(&st(0, 2, 4), &targets, None, 8, costs, 2),
            26,
            "other-layer target adds one via to planar distance"
        );

        let no_via = AStarCosts {
            allow_via: false,
            via: 25,
            ..AStarCosts::default()
        };
        assert_eq!(
            target_heuristic(&st(0, 3, 4), &targets, None, 8, no_via, 2),
            0,
            "when vias are forbidden, do not add an unreachable-layer penalty"
        );
    }

    #[test]
    fn disc_offsets_match_euclidean_radius_cells() {
        assert_eq!(disc_offsets(0), vec![(0, 0)]);

        let r1: std::collections::BTreeSet<_> = disc_offsets(1).into_iter().collect();
        assert_eq!(
            r1,
            [(-1, 0), (0, -1), (0, 0), (0, 1), (1, 0)]
                .into_iter()
                .collect()
        );

        let r2 = disc_offsets(2);
        assert!(r2.contains(&(1, 1)), "sqrt(2) is inside radius 2");
        assert!(!r2.contains(&(2, 1)), "sqrt(5) is outside radius 2");
        assert_eq!(r2.len(), 13);
    }

    #[test]
    fn layer_target_distance_fields_match_exact_layer_aware_scan() {
        let targets = vec![st(0, 0, 0), st(1, 6, 2), st(1, 3, 5)];
        let nx = 8;
        let ny = 7;
        let costs = AStarCosts {
            via: 13,
            ..AStarCosts::default()
        };
        let fields = target_distance_fields_by_layer(2, nx, ny, &targets);

        for layer in 0..2 {
            for y in 0..ny {
                for x in 0..nx {
                    let state = st(layer, x, y);
                    let exact = target_heuristic(&state, &targets, None, nx, costs, 2);
                    let field = target_heuristic(&state, &targets, Some(&fields), nx, costs, 2);
                    assert_eq!(field, exact, "state {state:?}");
                }
            }
        }
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
        assert!(
            path.iter().any(|s| s.iy != sy),
            "expected a vertical detour"
        );
    }

    #[test]
    fn via_hop_when_a_layer_is_fully_walled() {
        // A full-height wall on the top layer forces a hop to the bottom layer.
        let mut p = open_problem();
        p.bounds = Rect {
            min_x: 0.0,
            max_x: 20.0,
            min_y: 0.0,
            max_y: 20.0,
        };
        // Top-layer wall spanning the entire height, blocking all of column ~10.
        p.obstacles
            .push(pad(&[], (10.0, 10.0), 0.6, 22.0, &["top"]));
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
        assert!(
            path.iter().any(|s| s.layer == 1),
            "expected a via to layer 1"
        );
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
        assert!(
            search(
                &g,
                a,
                &[st(0, sx, sy)],
                &[st(0, tx, ty)],
                AStarCosts::default()
            )
            .is_none()
        );
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
            diag: DIAG_COST,
            ..AStarCosts::default()
        };
        let path =
            search(&g, a, &[st(0, sx, sy)], &[st(0, tx, ty)], costs).expect("reachable diagonally");
        assert_eq!(path.first(), Some(&st(0, sx, sy)));
        assert_eq!(path.last(), Some(&st(0, tx, ty)));
        // At least one step moves diagonally (both coordinates change together).
        let has_diagonal = path
            .windows(2)
            .any(|w| w[0].ix != w[1].ix && w[0].iy != w[1].iy);
        assert!(
            has_diagonal,
            "octilinear search must use a 45° diagonal step"
        );
    }

    #[test]
    fn orthogonal_never_uses_a_diagonal() {
        // The same corner-to-corner route with the default (orthogonal) move set
        // must stay on the 4-neighbour grid — no step changes both coordinates.
        let g = RouteGrid::build(&open_problem());
        let a = g.connection_index("A").unwrap();
        let (sx, sy) = g.cell_of(4.0, 4.0);
        let (tx, ty) = g.cell_of(14.0, 14.0);
        let path = search(
            &g,
            a,
            &[st(0, sx, sy)],
            &[st(0, tx, ty)],
            AStarCosts::default(),
        )
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
        p.obstacles.push(pad(&[], (8.0, 8.0), 0.6, 0.6, &["top"]));
        let g = RouteGrid::build(&p);
        let a = g.connection_index("A").unwrap();
        let costs = AStarCosts {
            diag: DIAG_COST,
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
    fn diag_body_clear_geometry() {
        // The swept-body predicate on a known fixture: a diagonal step (0,0)->(1,1)
        // with radius 1.0 cell must reject when a foreign cell sits within 1.0 of the
        // segment and accept when the only foreign copper is farther than 1.0 away.
        let mut g = RouteGrid::build(&open_problem());
        let a = g.connection_index("A").unwrap();
        let b = g.connection_index("B").unwrap();
        // The step runs (10,10)->(11,11). Cell (11,10) (offset (1,0)) is 1/√2 ≈ 0.707
        // from that segment, < 1.0, so marking it foreign (B) must fail the body check.
        g.mark_net(0, 11, 10, b);
        assert!(
            !diag_body_clear(&g, a, 0, 10, 10, 1, 1, 1.0),
            "a foreign cell 0.707 from the diagonal body must fail the swept check"
        );
        // A foreign cell two cells away perpendicular (offset (2,0) → dist √2 ≈ 1.414 >
        // 1.0) is outside the band, so the same step is clear.
        let mut g2 = RouteGrid::build(&open_problem());
        g2.mark_net(0, 12, 10, b);
        assert!(
            diag_body_clear(&g2, a, 0, 10, 10, 1, 1, 1.0),
            "a foreign cell 1.414 from the diagonal body is outside the band → clear"
        );
    }

    #[test]
    fn parallel_diagonals_one_pitch_apart_are_rejected() {
        // The core fix: net A routes a 45° diagonal; net B cannot route a PARALLEL
        // diagonal one pitch away (perpendicular spacing pitch/√2, a DRC short) when
        // A's swept-body radius is the min-width keep-out. We simulate A's copper as
        // a marked diagonal staircase, then assert B's parallel diagonal step is
        // refused by the body check at A's own occupancy (mirrored: B's step body
        // must clear A's cells by the same radius).
        let mut g = RouteGrid::build(&open_problem());
        let a = g.connection_index("A").unwrap();
        let b = g.connection_index("B").unwrap();
        // A's diagonal cells (10,10),(11,11),(12,12) marked as A copper.
        for i in 0..3 {
            g.mark_net(0, 10 + i, 10 + i, a);
        }
        // B wants the parallel diagonal one pitch over: (11,10)->(12,11). With the
        // min-width radius (pitch=0.2 → radius = (0.1+0.2+0.1)/0.2 = 2.0 cells), A's
        // cells fall inside B's swept band, so the step is rejected.
        let min_w_radius = 2.0;
        assert!(
            !diag_body_clear(&g, b, 0, 11, 10, 1, 1, min_w_radius),
            "B's parallel diagonal one pitch from A must be rejected (would short)"
        );
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

    #[test]
    fn orthogonal_diag_disabled_equals_baseline() {
        // Pin the EXACT orthogonal path on a fixed fixture. Guards the highest-risk
        // line of the unification: `AStarCosts::default()` must keep `diag =
        // u32::MAX` so the default search prunes every diagonal and stays a pure
        // 4-neighbour A*. A straight horizontal run is the canonical baseline.
        let g = RouteGrid::build(&open_problem());
        let a = g.connection_index("A").unwrap();
        assert_eq!(
            AStarCosts::default().diag,
            u32::MAX,
            "default must disable diagonals"
        );
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
        let expected: Vec<State> = (sx..=tx).map(|ix| st(0, ix, sy)).collect();
        assert_eq!(
            path, expected,
            "orthogonal default must yield the exact straight path"
        );
    }
}
