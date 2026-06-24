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
//! of the simplify idea from `sch-engine/route.rs` — the crates stay
//! decoupled). The result is a [`RouteResult`]: the [`RouteSolution`] plus a
//! list of [`FailedNet`]s. A net that cannot be routed is reported, never
//! silently dropped, and the router never panics.
//!
//! ## Design constants
//!
//! All tunables live here ([`DesignConstants`]) as the single tuning surface:
//! the grid pitch and obstacle-inflation formulas (delegated to [`crate::grid`]
//! so the grid and router agree) and the A* bend/via costs. Slice 1 keeps the
//! spec defaults; later slices retune here.

use crate::astar::{self, AStarCosts, State};
use crate::grid::{self, RouteGrid};
use crate::problem::{LayerRef, Point2, RouteProblem, RouteSolution, Trace, Via, ViaSpan};

#[doc(inline)]
pub use crate::problem::FailedNet;

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

/// The outcome of [`route`]: the emitted copper plus any nets that failed.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteResult {
    /// Emitted traces and vias for the nets that routed.
    pub solution: RouteSolution,
    /// Nets that could not be fully routed (deterministic order).
    pub failed: Vec<FailedNet>,
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
    let mut best_w = failed_pad_weight(problem, &best);
    for _ in 0..3 {
        if best.failed.is_empty() {
            break;
        }
        let pri: std::collections::BTreeSet<String> =
            best.failed.iter().map(|f| f.connection.clone()).collect();
        let mut cand = route_with(problem, design, &pri);
        reconcile(problem, &mut cand);
        let cn = cand.failed.len();
        let cw = failed_pad_weight(problem, &cand);
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

/// Connectivity cost of a result: the number of PADS left unconnected (sum over failed
/// nets of their pin count), not the net count — failing one 8-pin power net is worse
/// than failing two 2-pin signals.
fn failed_pad_weight(problem: &RouteProblem, result: &RouteResult) -> usize {
    result
        .failed
        .iter()
        .map(|f| {
            problem
                .connections
                .iter()
                .find(|c| c.name == f.connection)
                .map(|c| c.points_to_connect.len().max(1))
                .unwrap_or(1)
        })
        .sum()
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
        let costs = AStarCosts {
            trace_clear_radius_cells: (((nw - min_w) / 2.0 / pitch).ceil() as usize),
            ..design.costs
        };

        // The routed tree starts as point 0's cell; each further point is
        // routed to the nearest cell already in the tree.
        let mut tree_cells: Vec<State> =
            vec![point_cell(&grid, &conn.points_to_connect[0], layer_count)];
        // Mark the seed cell (with its clearance halo) as this net's copper.
        let seed = tree_cells[0];
        grid.mark_net_halo(seed.layer, seed.ix, seed.iy, conn_idx, halo);

        let mut net_failed: Option<String> = None;

        for (pi, pt) in conn.points_to_connect.iter().enumerate().skip(1) {
            let start = point_cell(&grid, pt, layer_count);
            // A* from the new point's cell to the nearest cell of the tree.
            let path = astar::search(&grid, conn_idx, &[start], &tree_cells, costs);
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
/// simplify idea in `sch-engine/route.rs`, adapted to [`Point2`] (the crates
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectivity;
    use std::path::Path;

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
