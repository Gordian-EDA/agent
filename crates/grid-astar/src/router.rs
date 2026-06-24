//! Sequential naive grid router: one A* per net, shortest-net-first.
//!
//! The slice-1 baseline. Nets are routed one at a time in a deterministic order
//! (ascending bounding-box half-perimeter, ties by name — short local nets
//! first). Within a net, point 0 seeds a *routed tree*; each further point is
//! A*-routed to the nearest cell already in that tree. Successful paths are
//! marked into the [`RouteGrid`] as that connection's copper, so they become
//! obstacles for later nets (the grid's net-aware occupancy lets the same net
//! cross its own copper but blocks foreign nets).
//!
//! Cell paths are converted to mm polylines, split at layer changes (a [`Via`]
//! is emitted at each transition) and collinear runs are merged (a fresh copy
//! of the simplify idea from `sch-io/src/wire.rs` — the crates stay
//! decoupled). The result is the unified [`RouteResult`]: the [`RouteSolution`]
//! plus a list of [`FailedNet`]s. A net that cannot be routed is reported, never
//! silently dropped, and the router never panics.
//!
//! [`GridAStarRouter`] is the [`Router`] impl — the free-tier engine — wrapping
//! the strict/lenient/rip-up portfolio behind the SDK trait.
//!
//! ## Design constants
//!
//! All tunables live here ([`DesignConstants`]) as the single tuning surface:
//! the grid pitch and obstacle-inflation formulas (delegated to [`crate::grid`]
//! so the grid and router agree) and the A* bend/via costs. Slice 1 keeps the
//! spec defaults; later slices retune here.

use crate::astar::{self, AStarCosts, State};
use crate::grid::{self, RouteGrid};
use crate::problem::{
    Capabilities, LayerRef, Point2, RouteProblem, RouteResult, RouteSolution, Router, Trace, Via,
    ViaSpan,
};

#[doc(inline)]
pub use crate::problem::FailedNet;

/// This engine's [`RouteResult::engine`] provenance tag.
pub const ENGINE: &str = "naive";

/// The tunable design constants for the router, in one place.
///
/// Grid pitch and obstacle inflation are computed from the problem's design
/// rules (see [`crate::grid::grid_pitch`] / [`crate::grid::obstacle_inflation`]);
/// the A* costs are fixed step/bend/via weights.
#[derive(Debug, Clone, Copy, Default)]
pub struct DesignConstants {
    /// A* movement costs (bend, via), in grid-step units.
    pub costs: AStarCosts,
}

/// The inner copper layers that carry a solid GND/VCC plane, CENTRED in the stack:
/// 4-layer → In1,In2 (`{1,2}`); 6-layer → In2,In3 (`{2,3}`, leaving In1/In4 as signal);
/// else none. Centring keeps the stack symmetric and, crucially, leaves the other inner
/// layers as SIGNAL layers — the routing-capacity lever for dense BGA corridors. The
/// router never routes ON these (a signal would short the plane); a via tunnels through.
pub fn plane_layers(layer_count: usize) -> Vec<u32> {
    // Inner copper layers are 1..=(L-2); the symmetric centred pair is the two middle
    // ones: {L/2-1, L/2}. 4→{1,2}, 6→{2,3}, 8→{3,4}, 10→{4,5}, … — generalizing the
    // original 4/6 cases so high-end stackups (8/10/12-layer) get proper GND/VCC planes
    // AND the extra inner SIGNAL layers a dense BGA needs (8-layer → 6 signal layers vs
    // 4 on a 6-layer board). Odd or <4 counts carry no plane (2-layer, or malformed).
    if layer_count >= 4 && layer_count.is_multiple_of(2) {
        vec![layer_count as u32 / 2 - 1, layer_count as u32 / 2]
    } else {
        Vec::new()
    }
}

/// Bitmask form of [`plane_layers`] for [`crate::astar::AStarCosts::plane_mask`].
pub fn plane_mask_for(layer_count: usize) -> u32 {
    plane_layers(layer_count).iter().fold(0u32, |m, &l| m | (1u32 << l))
}

/// The Chebyshev radius (in grid cells) a via barrel must keep clear of foreign
/// copper on every layer before the slice-1 search may place a via there. A via
/// is wider than a trace, so the trace-sized clearance halo is not enough: a via
/// dropped one trace-halo from a foreign pad still overhangs its clearance zone
/// (the via-to-pad clearance errors KiCAD's DRC catches). Radius = via barrel
/// radius + clearance + the foreign trace's half-width. The slice-1 router places
/// vias exactly at cell centres (grid-aligned), so — unlike the detailed router's
/// sub-cell placement — no snap-displacement slack is needed; the A* applies the
/// halo as a EUCLIDEAN disc, so it does not over-block on the diagonal.
pub fn via_clear_radius_cells(problem: &RouteProblem) -> usize {
    let pitch = grid::grid_pitch(problem);
    // The grid is ALREADY inflated by (clearance + trace_half) around every obstacle,
    // so a trace-free cell already guarantees a *trace's* clearance. A via is wider
    // than a trace by exactly (via_radius - trace_half); only THAT extra radius must be
    // re-scanned. The old formula added the full (via_radius + clearance + trace_half),
    // double-counting the clearance+trace_half already baked into the grid and
    // over-blocking vias by ~2 cells — which made an inner BGA ball's escape via
    // impossible (its neighbours' inflation halos fell inside the bloated radius) even
    // though the via geometrically clears. Scan only the genuine via overhang.
    let via_extra = (problem.via_diameter / 2.0 - problem.min_trace_width / 2.0).max(0.0);
    (via_extra / pitch).ceil() as usize
}

/// Route `problem` with the default design constants, but with the via-barrel
/// clearance radius derived from the design rules so the slice-1 router does not
/// drop a via that overhangs a foreign pad/trace. It can still produce other
/// congestion artifacts; [`crate::pipeline::route_auto`] reconciles connectivity
/// and lints both engines, so a violating or phantom route never ships when a
/// cleaner one exists.
pub fn route(problem: &RouteProblem) -> RouteResult {
    let costs = AStarCosts {
        via_clear_radius_cells: via_clear_radius_cells(problem),
        ..AStarCosts::default()
    };
    route_iterated(problem, DesignConstants { costs })
}

/// Route, then RIP-UP RETRY: if any nets failed, re-route from a fresh grid with those
/// nets prioritised (they claim corridors before their neighbours), keeping the pass
/// that routes more. Iterated a few times while it keeps improving. Purely additive —
/// the first pass is the old behaviour and a worse retry is discarded — so a board can
/// only gain routed nets, never lose them. This relieves the greedy router's corridor
/// contention (e.g. a few more inner BGA balls escape) without a full rip-up engine.
fn route_iterated(problem: &RouteProblem, design: DesignConstants) -> RouteResult {
    let empty = std::collections::BTreeSet::new();
    let mut best = route_with(problem, design, &empty);
    reconcile(problem, &mut best);
    let mut best_n = best.failed.len();
    let mut best_w = crate::problem::failed_pad_weight(problem, &best.failed);
    for _ in 0..3 {
        if best.failed.is_empty() {
            break;
        }
        let pri: std::collections::BTreeSet<String> =
            best.failed.iter().map(|f| f.connection.clone()).collect();
        let mut cand = route_with(problem, design, &pri);
        reconcile(problem, &mut cand);
        let cn = cand.failed.len();
        let cw = crate::problem::failed_pad_weight(problem, &cand.failed);
        // PARETO improvement only: never worse on EITHER failed-net count or
        // unconnected-pad weight, and strictly better on at least one. The pad-weight
        // proxy is not exactly KiCAD's unconnected count, so requiring both metrics to
        // hold stops a retry that reduces one while worsening the other (which had
        // regressed a couple of stress boards). Stop once no Pareto gain remains.
        if cn <= best_n && cw <= best_w && (cn < best_n || cw < best_w) {
            best = cand;
            best_n = cn;
            best_w = cw;
        } else {
            break;
        }
    }
    best
}

/// The slice-1 router WITHOUT the via-barrel clearance scan (the original slice-1
/// behaviour). On a board with room it routes more nets — including vias that are
/// in fact DRC-clean — that the conservative scan would refuse. It may also drop a
/// via too close to foreign copper, so it is NOT used alone: [`crate::pipeline::route_auto`]
/// runs it alongside the strict [`route`] and the detailed router and keeps
/// whichever the lint scores cleanest. The board picks the strictness it needs.
pub fn route_lenient(problem: &RouteProblem) -> RouteResult {
    route_iterated(problem, DesignConstants::default())
}

/// Make a slice-1 result DRC-honest: the lint is the authority. First drop any
/// net's copper that violates GEOMETRY (clearance / width / via / bounds) — the
/// engine must never emit copper that fails DRC — then drop any net left
/// unconnected or shorted. Every dropped net is reported failed, so `failed`
/// never undercounts and the surviving copper is DRC-clean.
fn reconcile(problem: &RouteProblem, result: &mut RouteResult) {
    let mut dropped = crate::lint::drop_violating_copper(problem, &mut result.solution);
    dropped.extend(crate::lint::drop_unconnected_copper(problem, &mut result.solution));
    let known: std::collections::BTreeSet<&str> =
        result.failed.iter().map(|f| f.connection.as_str()).collect();
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let added: Vec<FailedNet> = dropped
        .into_iter()
        .filter(|n| !known.contains(n.as_str()) && seen.insert(n.clone()))
        .map(|n| FailedNet {
            connection: n,
            reason: "DRC oracle: net dropped — could not be routed cleanly (clearance/connectivity)"
                .to_string(),
        })
        .collect();
    result.failed.extend(added);
}

/// Route `problem` with explicit design constants and an optional `priority` set of
/// net names to route first (empty = the default shortest-first order).
pub fn route_with(
    problem: &RouteProblem,
    design: DesignConstants,
    priority: &std::collections::BTreeSet<String>,
) -> RouteResult {
    let mut grid = RouteGrid::build(problem);
    let layer_count = problem.layer_count.max(1) as usize;

    // Mark plane layers so the A* never routes signals onto a power plane (it would
    // short). Engine stackup convention: a 4-layer board is F / In1(plane) /
    // In2(plane) / B, so the inner two layers are planes; 2-layer has none. Without
    // this, an inner BGA ball escapes via the cheapest F→In1 hop onto the GND plane
    // and gets dropped as a short — no signal escapes at all.
    let design = DesignConstants {
        costs: AStarCosts {
            plane_mask: plane_mask_for(layer_count),
            ..design.costs
        },
    };

    // Clearance halo: when a net claims a cell, foreign nets must stay a full
    // (width + clearance) centre-to-centre away. Sized to the WIDEST net so even a fat
    // power trace is spaced correctly (conservative — with no per-net widths it is the
    // old min_trace_width + clearance). Marked as a Chebyshev radius around each routed
    // cell so later nets keep their distance while the owning net routes freely through.
    let pitch = grid::grid_pitch(problem);
    let min_w = problem.min_trace_width;

    let mut traces: Vec<Trace> = Vec::new();
    let mut vias: Vec<Via> = Vec::new();
    let mut failed: Vec<FailedNet> = Vec::new();

    for ci in net_order(problem, priority) {
        let conn = &problem.connections[ci];
        let conn_idx = match grid.connection_index(&conn.name) {
            Some(i) => i,
            None => continue, // unreachable: every connection is indexed
        };

        // Skip trivially-connected nets (0 or 1 point).
        if conn.points_to_connect.len() < 2 {
            continue;
        }

        // Per-net width: fatter copper marks a wider keep-out halo and scans its extra
        // half-width as it routes (min-width nets => radius 0 => unchanged behaviour).
        let nw = problem.net_width(&conn.name);
        let halo =
            (((nw / 2.0 + problem.clearance + min_w / 2.0) / pitch).ceil() as usize).max(1);
        // An ENCLOSED fine-pitch ball with an assigned inner escape layer escapes
        // VERTICALLY (a via-in-pad to that layer) instead of along a surface axis. The
        // A* is then restricted to {top, bottom, escape layer} so the ring→layer
        // assignment holds and the search stays a 3-layer problem (a free all-layer maze
        // self-blocks on the via field). `None` = no assignment → the old surface escape.
        let escape_layer = problem.escape_layers.get(&conn.name).copied();
        // When ANY net has an inner-layer escape, the board routes on the full stack — so
        // every net is restricted to the OUTER pair {top, bottom} (the fast 2-layer bulk
        // maze) and an escape net additionally gets its one assigned inner layer. With no
        // escapes at all the mask is 0 (unrestricted) — bit-identical to before.
        let outer_mask = (1u32 << 0) | (1u32 << (layer_count as u32 - 1));
        let layer_mask = if problem.escape_layers.is_empty() {
            0
        } else {
            outer_mask | escape_layer.map_or(0, |l| 1u32 << l)
        };
        let costs = AStarCosts {
            trace_clear_radius_cells: (((nw - min_w) / 2.0 / pitch).ceil() as usize),
            layer_mask,
            ..design.costs
        };

        // The routed tree starts as point 0's cell; each further point is
        // routed to the nearest cell already in the tree. A fine-pitch peripheral
        // pad (a 0.5 mm QFP pin) whose grid cell is enclosed by neighbours' halos
        // gets a radial ESCAPE STUB pre-routed along its own long axis out to open
        // board, and the stub tip — not the over-blocked pad cell — seeds the tree.
        let seed_pad = point_cell(&grid, &conn.points_to_connect[0], layer_count);
        let mut tree_cells: Vec<State> = escape_cells(
            &mut grid, problem, conn_idx, &conn.points_to_connect[0], seed_pad, halo,
            escape_layer, &mut traces, &mut vias,
        );

        let mut net_failed: Option<String> = None;

        for (pi, pt) in conn.points_to_connect.iter().enumerate().skip(1) {
            let start_pad = point_cell(&grid, pt, layer_count);
            let starts = escape_cells(
                &mut grid, problem, conn_idx, pt, start_pad, halo, escape_layer,
                &mut traces, &mut vias,
            );
            // A* from the new point's cell to the nearest cell of the tree.
            let path = astar::search(&grid, conn_idx, &starts, &tree_cells, costs);
            let Some(path) = path else {
                net_failed = Some(format!(
                    "no grid path from point {pi} to the routed tree (congestion or enclosure)"
                ));
                break;
            };

            // Mark every path cell (plus clearance halo) as this net's copper
            // and fold it into the tree so subsequent points can tap anywhere
            // along it.
            for s in &path {
                grid.mark_net_halo(s.layer, s.ix, s.iy, conn_idx, halo);
                tree_cells.push(*s);
            }

            // Emit copper: split the cell path at layer changes into per-layer
            // mm polylines, with a via at each transition.
            emit_path(
                problem,
                &grid,
                &conn.name,
                &path,
                &mut traces,
                &mut vias,
            );
        }

        if let Some(reason) = net_failed {
            failed.push(FailedNet {
                connection: conn.name.clone(),
                reason,
            });
        }
    }

    RouteResult {
        solution: RouteSolution { traces, vias },
        failed,
        engine: ENGINE.to_owned(),
    }
}

/// Connection indices in routing order: any net in `priority` first (so a rip-up retry
/// can give the previously-failed nets the empty grid), then ascending bounding-box
/// half-perimeter, ties by name. Deterministic. With an empty `priority` this is exactly
/// the shortest-half-perimeter-first order.
fn net_order(problem: &RouteProblem, priority: &std::collections::BTreeSet<String>) -> Vec<usize> {
    let mut order: Vec<usize> = (0..problem.connections.len()).collect();
    order.sort_by(|&a, &b| {
        let pa = priority.contains(&problem.connections[a].name);
        let pb = priority.contains(&problem.connections[b].name);
        pb.cmp(&pa) // priority nets first
            .then_with(|| {
                let ka = half_perimeter(&problem.connections[a]);
                let kb = half_perimeter(&problem.connections[b]);
                ka.partial_cmp(&kb).unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| problem.connections[a].name.cmp(&problem.connections[b].name))
    });
    order
}

/// Half-perimeter (width + height) of a connection's point bounding box.
fn half_perimeter(conn: &crate::problem::Connection) -> f64 {
    let pts = &conn.points_to_connect;
    if pts.is_empty() {
        return 0.0;
    }
    let (mut min_x, mut max_x) = (pts[0].x, pts[0].x);
    let (mut min_y, mut max_y) = (pts[0].y, pts[0].y);
    for p in pts {
        min_x = min_x.min(p.x);
        max_x = max_x.max(p.x);
        min_y = min_y.min(p.y);
        max_y = max_y.max(p.y);
    }
    (max_x - min_x) + (max_y - min_y)
}

/// Entry cells for a terminal `pt` (already mapped to `pad_cell`), pre-routing a
/// radial ESCAPE STUB for a fine-pitch peripheral pad whose own grid cell is
/// enclosed by neighbours' clearance halos.
///
/// The pad cell is always returned, its clearance halo marked. When that cell can
/// only reach a tiny region on its own layer (an enclosed fine-pitch pin) AND the
/// pad is elongated (a QFP/SOIC-style radial pad), a straight stub is laid along
/// the pad's LONG axis, outward, to the first grid cell in open board. The stub is
/// authored in exact mm at the pad's true centre-line — sub-grid, so it is not
/// distorted by the 0.2 mm grid quantisation that makes the maze router see the
/// (genuinely legal) escape as blocked. The stub trace is emitted, its cells +
/// halo are marked as the net's copper, and the stub-tip cell is returned as an
/// extra entry so the maze router routes from open board, not the enclosed pad.
///
/// DRC safety: the stub is collinear with the pad (its own copper), so by
/// construction it keeps full clearance from the perpendicular neighbours (their
/// pads are ≥ pitch away laterally); and the final `drop_violating_copper` lint
/// gates the whole solution, so a stub that ever grazed foreign copper would be
/// dropped and the net reported failed — never shipped.
#[allow(clippy::too_many_arguments)] // one cohesive escape primitive; flat args keep it inline-able
fn escape_cells(
    grid: &mut RouteGrid,
    problem: &RouteProblem,
    conn_idx: usize,
    pt: &crate::problem::RoutePoint,
    pad_cell: State,
    halo: usize,
    escape_layer: Option<u32>,
    traces: &mut Vec<Trace>,
    vias: &mut Vec<Via>,
) -> Vec<State> {
    grid.mark_net_halo(pad_cell.layer, pad_cell.ix, pad_cell.iy, conn_idx, halo);
    let mut cells = vec![pad_cell];

    // Only an enclosed pad needs help. "Enclosed" = its own-layer free region is
    // small (a handful of cells), which is exactly the fine-pitch-pin/inner-ball case.
    if local_region(grid, conn_idx, pad_cell, 64) >= 64 {
        return cells;
    }

    // INNER-LAYER VIA-IN-PAD escape: an enclosed ball with an assigned inner signal
    // layer drops a through-via centred ON its pad (same net → clears its own copper;
    // at ≥0.8 mm pitch the via barrel clears the neighbour balls — verified geometry)
    // and seeds the routed tree on the assigned inner layer, where the field is open.
    // The maze then routes radially out THERE, restricted (by `layer_mask`) to this
    // net's three layers. Tried before the surface stub: a truly-enclosed ball (free
    // region 1) has no surface axis to escape along, only a vertical one.
    if let Some(out) = escape_layer
        .and_then(|el| via_in_pad_escape(grid, problem, conn_idx, pt, pad_cell, el as usize, halo, vias))
    {
        cells.push(out);
        return cells;
    }
    // Find the pad obstacle at this point: an owned obstacle covering pt. Need its
    // long axis to know the escape direction.
    let Some(pad) = problem.obstacles.iter().find(|ob| {
        ob.connected_to.iter().any(|n| grid.connection_index(n) == Some(conn_idx))
            && (pt.x - ob.center.x).abs() <= ob.width / 2.0 + 1e-6
            && (pt.y - ob.center.y).abs() <= ob.height / 2.0 + 1e-6
    }) else {
        return cells;
    };
    // Elongated pads only (a square BGA ball has no escape axis — it needs a via,
    // handled elsewhere). Require a clear long/short ratio so the axis is unambiguous.
    let (long_is_x, long_half) = if pad.width >= pad.height * 1.5 {
        (true, pad.width / 2.0)
    } else if pad.height >= pad.width * 1.5 {
        (false, pad.height / 2.0)
    } else {
        return cells;
    };

    // The stub runs out to the pad tip + a clearance margin so its end sits clear of
    // the pad and into board. Try BOTH directions along the long axis; keep the one
    // whose tip reaches the larger open region, tie-broken toward the nearer board
    // edge (a peripheral pad escapes OUTWARD, away from the part body — which sits on
    // the inner side and shows up as the smaller region).
    let reach = long_half + problem.clearance + problem.min_trace_width / 2.0 + grid.pitch;
    let mut best: Option<(State, usize, f64)> = None;
    for sign in [1.0_f64, -1.0] {
        let (tx, ty) = if long_is_x {
            (pt.x + sign * reach, pt.y)
        } else {
            (pt.x, pt.y + sign * reach)
        };
        if tx < problem.bounds.min_x || tx > problem.bounds.max_x
            || ty < problem.bounds.min_y || ty > problem.bounds.max_y
        {
            continue;
        }
        let (ix, iy) = grid.cell_of(tx, ty);
        let tip = State { layer: pad_cell.layer, ix, iy };
        if !grid.is_free_for(tip.layer, tip.ix, tip.iy, conn_idx) {
            continue;
        }
        let region = local_region(grid, conn_idx, tip, 200);
        // Distance from the tip to the NEAREST board edge along the escape axis:
        // smaller = closer to the edge = the true outward direction.
        let edge_dist = if long_is_x {
            (tx - problem.bounds.min_x).min(problem.bounds.max_x - tx)
        } else {
            (ty - problem.bounds.min_y).min(problem.bounds.max_y - ty)
        };
        let better = match best {
            None => true,
            Some((_, r, e)) => region > r || (region == r && edge_dist < e),
        };
        if better {
            best = Some((tip, region, edge_dist));
        }
    }
    let Some((tip, region, _)) = best else {
        return cells;
    };
    // The tip must actually reach open board (else the stub buys nothing).
    if region < 32 {
        return cells;
    }
    // Author the stub from the pad's exact centre (sub-grid, so the near-pad run is
    // collinear with the pad and clears the perpendicular neighbours by construction)
    // to the tip CELL centre, where the maze router takes over (so the stub end meets
    // the routed path byte-exactly). The tip sits ~1 pad-length out in open board, so
    // the half-pitch perpendicular snap there is harmless; `drop_violating_copper`
    // gates the emitted mm regardless.
    let start_mm = Point2 { x: pt.x, y: pt.y };
    let tip_mm = Point2 { x: grid.cell_center_x(tip.ix), y: grid.cell_center_y(tip.iy) };
    traces.push(Trace {
        connection: problem.connections[conn_idx].name.clone(),
        layer: layer_ref(pad_cell.layer, problem.layer_count.max(1) as usize),
        width: problem.net_width(&problem.connections[conn_idx].name),
        path: vec![start_mm, tip_mm],
    });
    mark_segment(grid, conn_idx, pad_cell, tip, halo);
    cells.push(tip);
    cells
}

/// Drop a through-via centred ON an enclosed ball's pad and seed the routed tree on the
/// assigned inner signal layer `el`. Returns the inner-layer landing [`State`] (added to
/// the tree) when the escape is placed, else `None` (the inner cell is not free for this
/// net — another escape already claimed it; the caller falls back to the surface stub /
/// reports the net unrouted).
///
/// The via sits at the pad CENTRE (sub-grid mm), same net as the pad, so it never shorts
/// its own copper; at ≥0.8 mm pitch the 0.6 mm barrel clears the orthogonal neighbour
/// balls by ≥0.05 mm beyond clearance (the pitch gate is the caller's `escape_layers`
/// assignment — only assigned where the geometry fits). The inner-layer landing cell and
/// its clearance halo are marked so the next ball's escape keeps the full via spacing,
/// and `drop_violating_copper` is the final authority: a via that still grazed a
/// neighbour is dropped and the net reported unrouted, never shipped failing.
#[allow(clippy::too_many_arguments)]
fn via_in_pad_escape(
    grid: &mut RouteGrid,
    problem: &RouteProblem,
    conn_idx: usize,
    pt: &crate::problem::RoutePoint,
    pad_cell: State,
    el: usize,
    halo: usize,
    vias: &mut Vec<Via>,
) -> Option<State> {
    let layer_count = problem.layer_count.max(1) as usize;
    if el == 0 || el >= layer_count {
        return None;
    }
    let landing = State { layer: el, ix: pad_cell.ix, iy: pad_cell.iy };
    // The inner-layer landing must be open for this net (it is empty unless another
    // escape's via halo already claimed it).
    if !grid.is_free_for(landing.layer, landing.ix, landing.iy, conn_idx) {
        return None;
    }
    // The via sits ON the pad (its own copper), so its TOP-layer neighbour clearance is
    // the pitch gate's guarantee (the caller assigns an escape layer only where the via
    // fits), NOT a grid scan — the enclosed ball's top neighbourhood is legitimately
    // BlockedAll from neighbour halos, which a top scan would wrongly reject. The barrel
    // must, however, clear foreign copper on every INNER / bottom layer it pierces (a
    // foreign via or a through-hole barrel would collide there). `drop_violating_copper`
    // is the final authority regardless: a via that still grazed a neighbour is dropped
    // and the net reported unrouted, never shipped failing.
    let via_extra = (problem.via_diameter / 2.0 - problem.min_trace_width / 2.0).max(0.0);
    let r = (via_extra / grid.pitch).ceil() as usize;
    if !via_barrel_clear_below_top(grid, conn_idx, pad_cell.ix, pad_cell.iy, r) {
        return None;
    }
    vias.push(Via {
        connection: problem.connections[conn_idx].name.clone(),
        at: Point2 { x: pt.x, y: pt.y },
        diameter: problem.via_diameter,
        drill: problem.via_drill,
        span: ViaSpan::Through,
    });
    // Reserve the inner-layer landing + its via clearance halo for this net so a
    // neighbouring escape's via keeps the full barrel spacing.
    let via_halo = (((problem.via_diameter / 2.0 + problem.clearance + problem.min_trace_width / 2.0)
        / grid.pitch)
        .ceil() as usize)
        .max(halo);
    grid.mark_net_halo(landing.layer, landing.ix, landing.iy, conn_idx, via_halo);
    Some(landing)
}

/// Is every cell within `radius_cells` (Euclidean) of `(ix,iy)` free for `conn` on every
/// layer BELOW the top (`1..layer_count`)? The via-in-pad barrel's top-layer clearance is
/// the pitch gate's guarantee (the via sits on the ball's own pad, and the enclosed ball's
/// top neighbourhood is legitimately BlockedAll from neighbour halos), so the top layer is
/// excluded; the inner/bottom layers it pierces must be free of any foreign barrel.
fn via_barrel_clear_below_top(grid: &RouteGrid, conn: usize, ix: usize, iy: usize, radius_cells: usize) -> bool {
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
            if !(1..grid.layer_count).all(|l| grid.is_free_for(l, hx as usize, hy as usize, conn)) {
                return false;
            }
        }
    }
    true
}

/// Size of the connected free-for-`conn` region around `from` on its OWN layer
/// (4-connectivity, no vias), capped at `cap`. A small count ⇒ the cell is
/// enclosed. Cheap: a bounded flood that stops at `cap`.
fn local_region(grid: &RouteGrid, conn: usize, from: State, cap: usize) -> usize {
    if !grid.is_free_for(from.layer, from.ix, from.iy, conn) {
        return 0;
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut stack = vec![(from.ix, from.iy)];
    seen.insert((from.ix, from.iy));
    while let Some((ix, iy)) = stack.pop() {
        if seen.len() >= cap {
            break;
        }
        for (dx, dy) in [(1isize, 0isize), (-1, 0), (0, 1), (0, -1)] {
            let nx = ix as isize + dx;
            let ny = iy as isize + dy;
            if nx < 0 || ny < 0 || nx >= grid.nx as isize || ny >= grid.ny as isize {
                continue;
            }
            let (nx, ny) = (nx as usize, ny as usize);
            if seen.contains(&(nx, ny)) || !grid.is_free_for(from.layer, nx, ny, conn) {
                continue;
            }
            seen.insert((nx, ny));
            stack.push((nx, ny));
        }
    }
    seen.len()
}

/// Open the stub corridor `a`→`b` (same layer): FORCE each centre-line cell to
/// `conn` (overriding the conservative halo collapse, since the stub centre-line is
/// DRC-legal by construction) and mark the surrounding clearance `halo` so later
/// foreign nets keep their distance. Bresenham line for the centre-line.
fn mark_segment(grid: &mut RouteGrid, conn: usize, a: State, b: State, halo: usize) {
    let (mut ix, mut iy) = (a.ix as isize, a.iy as isize);
    let (tx, ty) = (b.ix as isize, b.iy as isize);
    let (dx, dy) = ((tx - ix).abs(), (ty - iy).abs());
    let (sx, sy) = (if tx >= ix { 1 } else { -1 }, if ty >= iy { 1 } else { -1 });
    let mut err = dx - dy;
    loop {
        // Halo first (won't override BlockedAll), then force the centre-line cell
        // so the corridor is genuinely open for this net.
        grid.mark_net_halo(a.layer, ix as usize, iy as usize, conn, halo);
        grid.force_mark_net(a.layer, ix as usize, iy as usize, conn);
        if ix == tx && iy == ty {
            break;
        }
        let e2 = 2 * err;
        if e2 > -dy {
            err -= dy;
            ix += sx;
        }
        if e2 < dx {
            err += dx;
            iy += sy;
        }
    }
}

/// The grid cell + layer of a route point.
fn point_cell(grid: &RouteGrid, pt: &crate::problem::RoutePoint, layer_count: usize) -> State {
    let layer = pt
        .layer
        .index(layer_count as u32)
        .unwrap_or(0)
        .min(layer_count as u32 - 1) as usize;
    let (ix, iy) = grid.cell_of(pt.x, pt.y);
    State { layer, ix, iy }
}

/// Convert one cell path into mm copper: per-layer polylines (collinear runs
/// merged) plus a via at each layer transition.
fn emit_path(
    problem: &RouteProblem,
    grid: &RouteGrid,
    connection: &str,
    path: &[State],
    traces: &mut Vec<Trace>,
    vias: &mut Vec<Via>,
) {
    if path.is_empty() {
        return;
    }
    let width = problem.net_width(connection); // per-net: fat power, thin signals
    let layer_count = problem.layer_count.max(1) as usize;

    // Walk the path, accumulating same-layer runs; a layer change closes the
    // current run (emit a trace), drops a via at the transition cell, and opens
    // a new run on the next layer at the same mm position.
    let mm = |s: &State| Point2 {
        x: grid.cell_center_x(s.ix),
        y: grid.cell_center_y(s.iy),
    };

    let mut run: Vec<Point2> = vec![mm(&path[0])];
    let mut run_layer = path[0].layer;

    for w in path.windows(2) {
        let (prev, cur) = (w[0], w[1]);
        if cur.layer == prev.layer {
            run.push(mm(&cur));
        } else {
            // Layer change: the via sits at the shared (ix,iy) of prev==cur.
            let at = mm(&prev);
            // Close the current run.
            push_trace(traces, connection, run_layer, layer_count, width, std::mem::take(&mut run));
            vias.push(Via {
                connection: connection.to_owned(),
                at: Point2 { x: at.x, y: at.y },
                diameter: problem.via_diameter,
                drill: problem.via_drill,
                span: ViaSpan::Through,
            });
            // Start the next run on the new layer at the same point.
            run = vec![Point2 { x: at.x, y: at.y }];
            run_layer = cur.layer;
        }
    }
    push_trace(traces, connection, run_layer, layer_count, width, run);
}

/// Push a simplified (collinear-merged) trace if it has ≥ 2 distinct points.
fn push_trace(
    traces: &mut Vec<Trace>,
    connection: &str,
    layer: usize,
    layer_count: usize,
    width: f64,
    path: Vec<Point2>,
) {
    let simplified = simplify(path);
    if simplified.len() < 2 {
        return;
    }
    traces.push(Trace {
        connection: connection.to_owned(),
        layer: layer_ref(layer, layer_count),
        width,
        path: simplified,
    });
}

/// The [`LayerRef`] for a numeric copper layer index (0 = top, last index =
/// bottom, anything between as `inner{n}`) — the inverse of
/// [`LayerRef::index`].
fn layer_ref(layer: usize, layer_count: usize) -> LayerRef {
    if layer == 0 {
        LayerRef::top()
    } else if layer + 1 == layer_count {
        LayerRef::bottom()
    } else {
        LayerRef(format!("inner{layer}"))
    }
}

/// Drop near-duplicate points and merge collinear runs. Fresh copy of the
/// simplify idea in `sch-io/src/wire.rs`, adapted to [`Point2`] (the crates
/// stay decoupled — no dependency between them).
fn simplify(path: Vec<Point2>) -> Vec<Point2> {
    const EPS: f64 = 1e-9;
    let mut deduped: Vec<Point2> = Vec::with_capacity(path.len());
    for p in path {
        match deduped.last() {
            Some(last) if (last.x - p.x).abs() < EPS && (last.y - p.y).abs() < EPS => {}
            _ => deduped.push(p),
        }
    }
    let mut out: Vec<Point2> = Vec::with_capacity(deduped.len());
    for p in deduped {
        if out.len() >= 2 {
            let a = &out[out.len() - 2];
            let b = &out[out.len() - 1];
            let collinear_x = (a.x - b.x).abs() < EPS && (b.x - p.x).abs() < EPS;
            let collinear_y = (a.y - b.y).abs() < EPS && (b.y - p.y).abs() < EPS;
            if collinear_x || collinear_y {
                *out.last_mut().unwrap() = p;
                continue;
            }
        }
        out.push(p);
    }
    out
}

// ── GridAStarRouter (the SDK Router impl) ───────────────────────────────────────

/// The free-tier grid-A* [`Router`]: a sequential shortest-net-first A* per net
/// with a rip-up retry, run in both a STRICT (via-barrel clearance scan) and a
/// LENIENT (no scan) variant — the better of the two is returned as the engine's
/// single result.
///
/// Both variants reconcile their copper through the DRC oracle before scoring, so
/// the result is geometry-clean; the better variant is the one that leaves fewer
/// unconnected pads (the lenient variant routes more on a board with room; the
/// strict variant is needed where a via would overhang a foreign pad). Strict
/// wins exact ties as the more conservative path.
#[derive(Debug, Clone, Copy, Default)]
pub struct GridAStarRouter;

impl Router for GridAStarRouter {
    fn name(&self) -> &'static str {
        ENGINE
    }

    /// The always-correct baseline supports any board: it honours per-net widths,
    /// custom outlines, inner-layer escape assignments, and any layer count.
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            max_layers: u32::MAX,
            honors_escape_layers: true,
            honors_net_widths: true,
            honors_outline: true,
        }
    }

    fn route(&self, problem: &RouteProblem) -> RouteResult {
        let strict = route(problem);
        let lenient = route_lenient(problem);
        if score(problem, &lenient) < score(problem, &strict) {
            lenient
        } else {
            strict
        }
    }
}

/// `(unconnected-pad weight + geometry violations, geometry violations)` for a
/// grid-router result — the strict-vs-lenient tiebreak key. Both variants are
/// reconciled to geometry-clean copper, so `geom` is 0 in practice; it is kept in
/// the key as the same robustness guard the cross-engine selector uses.
fn score(problem: &RouteProblem, r: &RouteResult) -> (usize, usize) {
    let geom = geometry_violations(problem, &r.solution);
    (crate::problem::failed_pad_weight(problem, &r.failed) + geom, geom)
}

/// Count the GEOMETRY DRC violations of a solution (clearance / width / via /
/// bounds / invalid layer) — excluding connectivity, which already correlates
/// with the failed-net count. The router's own DRC authority.
pub fn geometry_violations(problem: &RouteProblem, solution: &RouteSolution) -> usize {
    crate::lint::lint(problem, solution)
        .iter()
        .filter(|v| !matches!(v, crate::lint::DrcViolation::Connectivity { .. }))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectivity;
    use crate::problem::{Bounds, Connection, Obstacle, RoutePoint};
    use std::path::Path;

    /// A fine-pitch peripheral pad (a QFP pin) whose own grid cell is enclosed by its
    /// neighbours' clearance halos must still escape via its radial stub: with the box
    /// inflation alone the pin is walled in (own cell + every step out is BlockedAll),
    /// and only the pad-copper relief + escape stub route it. Regression guard for the
    /// LQFP-144 fix.
    #[test]
    fn enclosed_qfp_pin_escapes_via_its_radial_stub() {
        // A left-edge QFP row: pads 1.475 (x) × 0.3 (y), pitch 0.5 in y, centres x=5.0.
        // A package-body keepout sits to the +x (interior) side, so the only escape is
        // -x toward open board. Each pin connects to a spread-out cap far to the left.
        let mk_pad = |net: &str, cx: f64, cy: f64, w: f64, h: f64, owned: bool| Obstacle {
            kind: "rect".into(),
            layers: vec![LayerRef::top()],
            center: Point2 { x: cx, y: cy },
            width: w,
            height: h,
            connected_to: if owned { vec![net.into()] } else { vec![] },
        };
        let mut obstacles = vec![mk_pad("", 8.0, 7.0, 4.0, 6.0, false)]; // body keepout
        let mut connections = Vec::new();
        for i in 0..5 {
            let cy = 5.5 + i as f64 * 0.5;
            let net = format!("S{i}");
            obstacles.push(mk_pad(&net, 5.0, cy, 1.475, 0.3, true));
            let dest = (1.5, 1.0 + i as f64 * 1.5);
            obstacles.push(mk_pad(&net, dest.0, dest.1, 0.5, 0.5, true));
            connections.push(Connection {
                name: net,
                points_to_connect: vec![
                    RoutePoint { x: 5.0, y: cy, layer: LayerRef::top() },
                    RoutePoint { x: dest.0, y: dest.1, layer: LayerRef::top() },
                ],
            });
        }
        let problem = RouteProblem {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles,
            connections,
            bounds: Bounds { min_x: 0.0, max_x: 10.0, min_y: 0.0, max_y: 12.0 },
            clearance: 0.2,
            via_diameter: 0.6,
            via_drill: 0.3,
            net_widths: Default::default(),
            outline: None,
            escape_layers: Default::default(),
        };

        // Without the stub these pins are unroutable (their own cell is BlockedAll);
        // with it, at least one escapes — and the emitted copper is GEOMETRY-clean
        // (reconcile drops any clearance/width/via/short violator before it ships).
        let result = route(&problem);
        let routed = problem.connections.len() - result.failed.len();
        assert!(routed >= 1, "the escape stub must route at least one enclosed QFP pin");
        let geom: Vec<_> = crate::lint::lint(&problem, &result.solution)
            .into_iter()
            .filter(|v| !matches!(v, crate::lint::DrcViolation::Connectivity { .. }))
            .collect();
        assert!(geom.is_empty(), "QFP escape copper must be geometry-clean: {geom:?}");
        // The partial-net rule: a failed net contributes no copper.
        let failed_names: std::collections::BTreeSet<&str> =
            result.failed.iter().map(|f| f.connection.as_str()).collect();
        assert!(
            result.solution.traces.iter().all(|t| !failed_names.contains(t.connection.as_str())),
            "a failed net must not ship a stub/trace"
        );
    }

    /// A dense BGA inner ball, walled in on F.Cu by its 8 neighbour balls (a 0.8 mm
    /// array), escapes to its ASSIGNED inner signal layer via a via-in-pad and routes out
    /// there to a far header — where a surface-only (F/B) router leaves it unrouted. The
    /// emitted copper is geometry-clean. Regression guard for the structured inner-layer
    /// escape (`escape_layers` → `via_in_pad_escape`).
    #[test]
    fn enclosed_bga_inner_ball_escapes_to_its_assigned_inner_layer() {
        // 3×3 of 0.5 mm balls at 0.8 mm pitch, centred at (5,5). The centre ball is the
        // SIGNAL net S; the 8 neighbours are foreign (GND/VCC) so S is fully enclosed on
        // F.Cu. 8-layer board → planes at {3,4}, inner signal layers {1,2,5,6}. S also
        // connects to a header pad far away in open board.
        let ball = |net: &str, cx: f64, cy: f64, owned: bool| Obstacle {
            kind: "rect".into(),
            layers: vec![LayerRef::top()],
            center: Point2 { x: cx, y: cy },
            width: 0.5,
            height: 0.5,
            connected_to: if owned { vec![net.into()] } else { vec![] },
        };
        let mut obstacles = Vec::new();
        let mut k = 0;
        for (i, dy) in [-0.8f64, 0.0, 0.8].into_iter().enumerate() {
            for (j, dx) in [-0.8f64, 0.0, 0.8].into_iter().enumerate() {
                if i == 1 && j == 1 {
                    continue; // centre is the signal, added below
                }
                k += 1;
                // Neighbour balls own a foreign net each so they wall the centre in.
                obstacles.push(ball(&format!("P{k}"), 5.0 + dx, 5.0 + dy, true));
            }
        }
        obstacles.push(ball("S", 5.0, 5.0, true)); // the enclosed inner signal
        obstacles.push(ball("S", 12.0, 12.0, true)); // a far header pad in open board
        let connections = vec![Connection {
            name: "S".into(),
            points_to_connect: vec![
                RoutePoint { x: 5.0, y: 5.0, layer: LayerRef::top() },
                RoutePoint { x: 12.0, y: 12.0, layer: LayerRef::top() },
            ],
        }];
        let mut escape_layers = std::collections::BTreeMap::new();
        escape_layers.insert("S".to_string(), 2u32); // an inner SIGNAL layer
        let problem = RouteProblem {
            layer_count: 8,
            min_trace_width: 0.2,
            obstacles,
            connections,
            bounds: Bounds { min_x: 0.0, max_x: 16.0, min_y: 0.0, max_y: 16.0 },
            clearance: 0.2,
            via_diameter: 0.6,
            via_drill: 0.3,
            net_widths: Default::default(),
            outline: None,
            escape_layers,
        };

        let result = route(&problem);
        assert!(
            result.failed.is_empty(),
            "the enclosed inner ball must escape to its inner layer, failed: {:?}",
            result.failed
        );
        // It dropped a via (the via-in-pad escape).
        assert!(!result.solution.vias.is_empty(), "escape must place a via-in-pad");
        // Some copper lands on the assigned inner signal layer (inner2).
        assert!(
            result.solution.traces.iter().any(|t| t.layer == LayerRef("inner2".into())),
            "the escape must route on the assigned inner signal layer"
        );
        // The emitted copper is geometry-clean (the lint is the authority).
        let geom: Vec<_> = crate::lint::lint(&problem, &result.solution)
            .into_iter()
            .filter(|v| !matches!(v, crate::lint::DrcViolation::Connectivity { .. }))
            .collect();
        assert!(geom.is_empty(), "inner-layer escape copper must be geometry-clean: {geom:?}");
    }

    /// Without the escape assignment the SAME enclosed inner ball is unroutable (F/B are
    /// walled in) — confirming the escape is what routes it, not the open board.
    #[test]
    fn enclosed_bga_inner_ball_is_unroutable_without_an_escape_layer() {
        let ball = |net: &str, cx: f64, cy: f64, owned: bool| Obstacle {
            kind: "rect".into(),
            layers: vec![LayerRef::top()],
            center: Point2 { x: cx, y: cy },
            width: 0.5,
            height: 0.5,
            connected_to: if owned { vec![net.into()] } else { vec![] },
        };
        let mut obstacles = Vec::new();
        let mut k = 0;
        for (i, dy) in [-0.8f64, 0.0, 0.8].into_iter().enumerate() {
            for (j, dx) in [-0.8f64, 0.0, 0.8].into_iter().enumerate() {
                if i == 1 && j == 1 {
                    continue;
                }
                k += 1;
                obstacles.push(ball(&format!("P{k}"), 5.0 + dx, 5.0 + dy, true));
            }
        }
        obstacles.push(ball("S", 5.0, 5.0, true));
        obstacles.push(ball("S", 12.0, 12.0, true));
        let connections = vec![Connection {
            name: "S".into(),
            points_to_connect: vec![
                RoutePoint { x: 5.0, y: 5.0, layer: LayerRef::top() },
                RoutePoint { x: 12.0, y: 12.0, layer: LayerRef::top() },
            ],
        }];
        let problem = RouteProblem {
            layer_count: 8,
            min_trace_width: 0.2,
            obstacles,
            connections,
            bounds: Bounds { min_x: 0.0, max_x: 16.0, min_y: 0.0, max_y: 16.0 },
            clearance: 0.2,
            via_diameter: 0.6,
            via_drill: 0.3,
            net_widths: Default::default(),
            outline: None,
            escape_layers: Default::default(), // NO escape assignment
        };
        let result = route(&problem);
        assert_eq!(result.failed.len(), 1, "with no escape, the walled-in ball cannot route");
    }

    #[test]
    fn plane_layers_are_the_centred_pair_for_every_even_stackup() {
        // 2-layer carries no plane; even ≥4 gets the two CENTRED inner layers, leaving
        // the rest as signal. Regression guard for the generalized formula.
        assert_eq!(plane_layers(2), Vec::<u32>::new());
        assert_eq!(plane_layers(4), vec![1, 2]);
        assert_eq!(plane_layers(6), vec![2, 3]);
        assert_eq!(plane_layers(8), vec![3, 4]);
        assert_eq!(plane_layers(10), vec![4, 5]);
        // Odd counts are malformed → no plane (never artificially place one).
        assert_eq!(plane_layers(5), Vec::<u32>::new());
    }

    fn load(name: &str) -> RouteProblem {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures")
            .join(name);
        let json = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        serde_json::from_str(&json).unwrap_or_else(|e| panic!("parse {name}: {e}"))
    }

    #[test]
    fn led_r_routes_fully_and_is_connectivity_clean() {
        let p = load("led-r.json");
        let result = route(&p);
        assert!(
            result.failed.is_empty(),
            "led-r.json should route fully, failed: {:?}",
            result.failed
        );
        let v = connectivity::check(&p, &result.solution);
        assert!(v.is_empty(), "connectivity oracle should be clean: {v:?}");
    }

    #[test]
    fn quad_routes_fully_with_a_via_and_is_connectivity_clean() {
        let p = load("quad.json");
        let result = route(&p);
        assert!(
            result.failed.is_empty(),
            "quad.json should route fully, failed: {:?}",
            result.failed
        );
        assert!(
            !result.solution.vias.is_empty(),
            "quad.json's crossing nets should force at least one via"
        );
        let v = connectivity::check(&p, &result.solution);
        assert!(v.is_empty(), "connectivity oracle should be clean: {v:?}");
    }

    #[test]
    fn solution_serialization_is_deterministic() {
        let p = load("quad.json");
        let a = route(&p);
        let b = route(&p);
        let ja = serde_json::to_string(&a.solution).unwrap();
        let jb = serde_json::to_string(&b.solution).unwrap();
        assert_eq!(ja, jb, "two routes must serialize byte-equal");
    }

    #[test]
    fn net_order_is_shortest_half_perimeter_first_then_name() {
        let p = load("quad.json");
        let order = net_order(&p, &std::collections::BTreeSet::new());
        // Order indices must be a permutation of all connections.
        let mut sorted = order.clone();
        sorted.sort();
        assert_eq!(sorted, (0..p.connections.len()).collect::<Vec<_>>());
        // Half-perimeters must be non-decreasing along the order.
        let mut prev = f64::NEG_INFINITY;
        for &i in &order {
            let hp = half_perimeter(&p.connections[i]);
            assert!(hp >= prev - 1e-12, "net order not non-decreasing by half-perimeter");
            prev = hp;
        }
    }

    #[test]
    fn routed_traces_use_layers_valid_for_the_board() {
        // Regression: grid layer 1 on a 2-layer board is "bottom", not
        // "inner1" (which LayerRef::index rejects for layer_count = 2).
        let p = load("quad.json");
        let r = route(&p);
        assert!(r.failed.is_empty());
        let mut saw_bottom = false;
        for t in &r.solution.traces {
            assert!(
                t.layer.index(p.layer_count).is_some(),
                "trace on layer {:?} invalid for a {}-layer board",
                t.layer,
                p.layer_count
            );
            saw_bottom |= t.layer == LayerRef::bottom();
        }
        assert!(saw_bottom, "quad must use the bottom layer (it has vias)");
    }

    #[test]
    fn simplify_merges_collinear_and_drops_dupes() {
        let path = vec![
            Point2 { x: 0.0, y: 0.0 },
            Point2 { x: 1.0, y: 0.0 },
            Point2 { x: 2.0, y: 0.0 }, // collinear with the previous two
            Point2 { x: 2.0, y: 0.0 }, // duplicate
            Point2 { x: 2.0, y: 3.0 },
        ];
        let out = simplify(path);
        assert_eq!(
            out,
            vec![
                Point2 { x: 0.0, y: 0.0 },
                Point2 { x: 2.0, y: 0.0 },
                Point2 { x: 2.0, y: 3.0 },
            ]
        );
    }
}
