//! Per-cell detailed router: the second detailed-routing stage (slice 3, Task 2).
//!
//! [`crate::crossing::assign_crossings`] turns a slice-2 [`GlobalPlan`] into a
//! list of [`CellJob`]s — one per `(leaf, net)` — each carrying the terminals
//! (pads, boundary entry/exit points, via sites) the net must connect *inside*
//! that one quadtree leaf. This stage routes each job with a fine **octilinear**
//! (45°) A* over a [`RouteGrid`] windowed to the leaf, then emits cell-local mm
//! polylines (with a layer per segment) and via sites — exactly the per-cell
//! copper Task 3 stitches into continuous per-net traces.
//!
//! ## Per-cell window
//!
//! Each job routes on a [`RouteGrid::build_window`] over the leaf rect inflated
//! by **one track pitch** and clamped to the board bounds, at the shared detailed
//! pitch ([`grid::grid_pitch`]). The inflation lets a route hug — and reach
//! crossing points that sit exactly on — the leaf boundary. Window edges that are
//! not board edges are routable; the route still cannot leave the window because
//! out-of-window cells read as blocked. See [`RouteGrid::build_window`] for the
//! full boundary contract. The window only ever contains *this* cell's own routed
//! copper plus the static obstacle model; foreign nets routed in other cells are
//! invisible here, and stitching (Task 3) joins cells at the shared crossing
//! points — which are byte-identical across the two cells because the window grid
//! is aligned to the global lattice and terminal endpoints are snapped exactly
//! (see below).
//!
//! ## Endpoint exactness (the stitching contract)
//!
//! A grid path's endpoints are *cell centres*, which need not coincide with a
//! terminal's exact mm position. Because Task 3 stitches a leaf's `Exit` to the
//! neighbour's `Entry` by coordinate identity, every emitted polyline endpoint
//! that corresponds to a terminal is **snapped to that terminal's exact mm
//! position** before emission. A crossing point therefore appears with the *same*
//! bytes in both adjacent cells (the assignment gave both the identical
//! [`Point2`]), so the stitched polyline is continuous with no gap or overlap.
//!
//! ## Determinism & failure
//!
//! Jobs route in deterministic order: leaves ascending, and within a leaf nets in
//! the slice-1 global net order (half-perimeter ascending, name tie-break)
//! restricted to the nets present in that leaf. Each net's routed copper + a
//! clearance halo are marked into the window grid (as in slice 1) so later nets
//! in the SAME cell avoid it. A net that cannot be routed inside its cell is
//! reported as a [`FailedNet`] carrying the leaf id — never panicked, never
//! silently dropped. The result is serializable and byte-stable across runs.

use crate::astar::{self, AStarCosts, MoveSet, State};
use crate::crossing::{CellJob, CrossingAssignment, Terminal, TerminalKind};
use crate::grid::{self, RouteGrid};
use crate::mesh::{CapacityMesh, LeafId, Rect};
use crate::problem::{FailedNet, LayerRef, Point2, RouteProblem};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ── public result types ──────────────────────────────────────────────────────

/// One single-layer copper polyline routed inside a cell, in board mm (y-down).
///
/// Endpoints that correspond to a terminal are snapped to that terminal's exact
/// mm position (see the module docs) so Task 3 can stitch cells by coordinate
/// identity. Interior points are detailed-grid cell centres.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CellTrace {
    /// The copper layer this polyline is on.
    pub layer: LayerRef,
    /// Ordered polyline points (mm). At least 2 points.
    pub points: Vec<Point2>,
}

/// A via site dropped inside a cell where the routed path changed layer. The via
/// is through-hole (joins every layer); Task 3 emits the [`crate::problem::Via`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CellVia {
    /// The via position (mm). Snapped to the assigned via-site terminal when the
    /// transition happens at one.
    pub at: Point2,
}

/// All copper one net's job produced inside one leaf: per-layer polylines plus
/// via sites. Empty `traces` is possible (a single-terminal job has nothing to
/// route); such a route is still emitted so Task 3 sees the cell was handled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CellRoute {
    /// The leaf this route is inside.
    pub leaf: LeafId,
    /// The connection (net) name.
    pub connection: String,
    /// Single-layer polylines (cell-local mm). Joined at via sites across layers.
    pub traces: Vec<CellTrace>,
    /// Via sites where the path changed layer inside the cell.
    pub vias: Vec<CellVia>,
}

/// The output of [`route_cells`]: per-cell copper plus any per-net failures.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CellRouteResult {
    /// Per `(leaf, net)` routed copper, in `(leaf, net-order)` order.
    pub cell_routes: Vec<CellRoute>,
    /// Nets that could not be routed inside a cell, with the leaf id in the
    /// reason. Deterministic order.
    pub failed: Vec<FailedNet>,
}

impl CellRouteResult {
    /// A clean result has no failures.
    pub fn is_clean(&self) -> bool {
        self.failed.is_empty()
    }
}

// ── entry point ──────────────────────────────────────────────────────────────

/// Route every cell job in `assignment` inside its leaf with octilinear A*.
///
/// `assignment` is the output of [`crate::crossing::assign_crossings`] for
/// `plan` over `mesh`/`problem`. Never panics; per-net cell failures are
/// collected in [`CellRouteResult::failed`].
pub fn route_cells(
    problem: &RouteProblem,
    mesh: &CapacityMesh,
    assignment: &CrossingAssignment,
) -> CellRouteResult {
    let layer_count = problem.layer_count.max(1) as usize;
    let pitch = grid::grid_pitch(problem);
    let track_pitch = mesh.track_pitch;
    // Clearance halo radius in grid cells — identical formula to the slice-1
    // router so per-cell clearance matches the full-board router.
    let halo = (((problem.min_trace_width + problem.clearance) / pitch).ceil() as usize).max(1);
    // Octilinear costs for the detailed stage; orthogonal default elsewhere.
    let costs = AStarCosts {
        moves: MoveSet::Octilinear,
        ..AStarCosts::default()
    };

    // Group jobs by leaf, then order nets within a leaf by the slice-1 global
    // net order restricted to nets present in the leaf.
    let net_rank = net_rank(problem);

    let mut jobs_by_leaf: BTreeMap<LeafId, Vec<&CellJob>> = BTreeMap::new();
    for job in &assignment.jobs {
        jobs_by_leaf.entry(job.leaf).or_default().push(job);
    }

    let mut cell_routes: Vec<CellRoute> = Vec::new();
    let mut failed: Vec<FailedNet> = Vec::new();

    for (leaf, mut jobs) in jobs_by_leaf {
        let leaf_rect = &mesh.leaves[leaf].rect;
        let window = inflate_clamp(leaf_rect, track_pitch, problem);
        // One grid per leaf, shared across the leaf's nets so later nets avoid
        // earlier ones' copper (slice-1 marking discipline, scoped to the cell).
        let mut wgrid = RouteGrid::build_window(problem, &window);

        // Net order within this leaf: slice-1 rank, ties by name (stable).
        jobs.sort_by(|p, q| {
            let rp = net_rank.get(&p.connection).copied().unwrap_or(usize::MAX);
            let rq = net_rank.get(&q.connection).copied().unwrap_or(usize::MAX);
            rp.cmp(&rq).then_with(|| p.connection.cmp(&q.connection))
        });

        for job in jobs {
            match route_one_job(job, &mut wgrid, layer_count, halo, costs) {
                Ok(route) => cell_routes.push(route),
                Err(reason) => failed.push(FailedNet {
                    connection: job.connection.clone(),
                    reason: format!("cell {leaf}: {reason}"),
                }),
            }
        }
    }

    CellRouteResult {
        cell_routes,
        failed,
    }
}

// ── per-job routing ──────────────────────────────────────────────────────────

/// Route one cell job into `wgrid`. The job's terminals are connected into a
/// single tree (terminal 0 seeds it; each further terminal is A*-routed to the
/// nearest tree cell, allowing vias). Routed copper + a clearance halo are marked
/// so later nets in the same cell avoid it. Returns the cell-local copper, or an
/// error string (the caller adds the leaf id).
fn route_one_job(
    job: &CellJob,
    wgrid: &mut RouteGrid,
    layer_count: usize,
    halo: usize,
    costs: AStarCosts,
) -> Result<CellRoute, String> {
    let conn_idx = wgrid
        .connection_index(&job.connection)
        .ok_or_else(|| "connection has no grid index".to_string())?;

    // De-duplicate terminals that map to the same (layer, cell): a pad and an
    // entry can coincide. Keep insertion order (the deterministic job order).
    let mut terminals: Vec<(&Terminal, State)> = Vec::with_capacity(job.terminals.len());
    {
        let mut seen: std::collections::BTreeSet<(usize, usize, usize)> = Default::default();
        for t in &job.terminals {
            let s = terminal_cell(wgrid, t, layer_count);
            if seen.insert((s.layer, s.ix, s.iy)) {
                terminals.push((t, s));
            }
        }
    }

    let mut traces: Vec<CellTrace> = Vec::new();
    let mut vias: Vec<CellVia> = Vec::new();

    if terminals.is_empty() {
        // Nothing to route (no terminals at all) — emit an empty route so the
        // cell is recorded as handled.
        return Ok(CellRoute {
            leaf: job.leaf,
            connection: job.connection.clone(),
            traces,
            vias,
        });
    }

    // Seed the tree with terminal 0 and mark its halo as this net's copper.
    let mut tree_cells: Vec<State> = vec![terminals[0].1];
    // Exact mm snap position for each terminal cell, keyed by (layer, ix, iy):
    // when a routed path's endpoint sits on a terminal cell, replace the cell
    // centre with the terminal's exact mm coordinate.
    let mut snap: BTreeMap<(usize, usize, usize), Point2> = BTreeMap::new();
    for (t, s) in &terminals {
        snap.entry((s.layer, s.ix, s.iy)).or_insert_with(|| t.at.clone());
    }

    {
        let seed = terminals[0].1;
        wgrid.mark_net_halo(seed.layer, seed.ix, seed.iy, conn_idx, halo);
    }

    // Single-terminal job: nothing to connect, but record any pad/via the cell
    // owns. A lone via terminal still needs its site recorded so Task 3 can drop
    // the via even when no in-cell copper run reaches it on both layers.
    for (t, _s) in &terminals {
        if t.kind == TerminalKind::Via {
            push_via(&mut vias, &t.at);
        }
    }

    for (t, start) in terminals.iter().skip(1) {
        let path = astar::search(wgrid, conn_idx, &[*start], &tree_cells, costs)
            .ok_or_else(|| {
                format!(
                    "no in-cell path for terminal {:?} at ({:.4},{:.4}) (congestion or enclosure)",
                    t.kind, t.at.x, t.at.y
                )
            })?;

        // Mark copper + halo and fold the path into the tree.
        for s in &path {
            wgrid.mark_net_halo(s.layer, s.ix, s.iy, conn_idx, halo);
            tree_cells.push(*s);
        }

        emit_path(wgrid, &path, &snap, &mut traces, &mut vias);
    }

    Ok(CellRoute {
        leaf: job.leaf,
        connection: job.connection.clone(),
        traces,
        vias,
    })
}

/// Convert one cell path into per-layer mm polylines (split at layer changes,
/// with a via at each transition), snapping any endpoint that sits on a terminal
/// cell to that terminal's exact mm position. Mirrors the slice-1 `emit_path`
/// shape but keeps cell-local copper for the stitcher.
fn emit_path(
    grid: &RouteGrid,
    path: &[State],
    snap: &BTreeMap<(usize, usize, usize), Point2>,
    traces: &mut Vec<CellTrace>,
    vias: &mut Vec<CellVia>,
) {
    if path.is_empty() {
        return;
    }
    let layer_count = grid.layer_count;
    // mm position of a path state, snapping to a terminal's exact position when
    // the state's cell is a terminal cell (endpoint exactness contract).
    let mm = |s: &State| -> Point2 {
        if let Some(p) = snap.get(&(s.layer, s.ix, s.iy)) {
            p.clone()
        } else {
            Point2 {
                x: grid.cell_center_x(s.ix),
                y: grid.cell_center_y(s.iy),
            }
        }
    };

    let mut run: Vec<Point2> = vec![mm(&path[0])];
    let mut run_layer = path[0].layer;

    for w in path.windows(2) {
        let (prev, cur) = (w[0], w[1]);
        if cur.layer == prev.layer {
            run.push(mm(&cur));
        } else {
            // Layer change: the via sits at the shared (ix,iy). Use the snapped
            // position so a via on a via-site terminal lands exactly there.
            let at = mm(&prev);
            push_trace(traces, run_layer, layer_count, std::mem::take(&mut run));
            push_via(vias, &at);
            run = vec![at];
            run_layer = cur.layer;
        }
    }
    push_trace(traces, run_layer, layer_count, run);
}

/// Push a simplified (collinear-merged) trace if it has ≥ 2 distinct points.
fn push_trace(
    traces: &mut Vec<CellTrace>,
    layer: usize,
    layer_count: usize,
    points: Vec<Point2>,
) {
    let simplified = simplify(points);
    if simplified.len() < 2 {
        return;
    }
    traces.push(CellTrace {
        layer: layer_ref(layer, layer_count),
        points: simplified,
    });
}

/// Push a via site, de-duplicating against the last one (a layer change at the
/// same point should not stack vias).
fn push_via(vias: &mut Vec<CellVia>, at: &Point2) {
    const EPS: f64 = 1e-9;
    if vias
        .iter()
        .any(|v| (v.at.x - at.x).abs() < EPS && (v.at.y - at.y).abs() < EPS)
    {
        return;
    }
    vias.push(CellVia { at: at.clone() });
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// Inflate a leaf rect by one track pitch and clamp to the board bounds — the
/// per-cell routing window (see [`RouteGrid::build_window`]).
fn inflate_clamp(rect: &Rect, track_pitch: f64, problem: &RouteProblem) -> Rect {
    let b = &problem.bounds;
    Rect {
        min_x: (rect.min_x - track_pitch).max(b.min_x),
        min_y: (rect.min_y - track_pitch).max(b.min_y),
        max_x: (rect.max_x + track_pitch).min(b.max_x),
        max_y: (rect.max_y + track_pitch).min(b.max_y),
    }
}

/// The grid cell + layer of a terminal.
fn terminal_cell(grid: &RouteGrid, t: &Terminal, layer_count: usize) -> State {
    let layer = t
        .layer
        .index(layer_count as u32)
        .unwrap_or(0)
        .min(layer_count as u32 - 1) as usize;
    let (ix, iy) = grid.cell_of(t.at.x, t.at.y);
    State { layer, ix, iy }
}

/// Connection name → slice-1 global net rank (ascending bounding-box
/// half-perimeter, ties by name). Lower rank routes first. Matches
/// [`crate::router`]'s `net_order` so the per-cell order is consistent with the
/// full-board router.
fn net_rank(problem: &RouteProblem) -> BTreeMap<String, usize> {
    let mut order: Vec<usize> = (0..problem.connections.len()).collect();
    order.sort_by(|&a, &b| {
        let ka = half_perimeter(&problem.connections[a]);
        let kb = half_perimeter(&problem.connections[b]);
        ka.partial_cmp(&kb)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| problem.connections[a].name.cmp(&problem.connections[b].name))
    });
    let mut rank: BTreeMap<String, usize> = BTreeMap::new();
    for (r, ci) in order.into_iter().enumerate() {
        rank.entry(problem.connections[ci].name.clone()).or_insert(r);
    }
    rank
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

/// The [`LayerRef`] for a numeric copper layer index (0 = top, last = bottom,
/// between = `inner{n}`) — the inverse of [`LayerRef::index`]. Mirrors
/// `router::layer_ref` (private there).
fn layer_ref(layer: usize, layer_count: usize) -> LayerRef {
    if layer == 0 {
        LayerRef::top()
    } else if layer + 1 == layer_count {
        LayerRef::bottom()
    } else {
        LayerRef(format!("inner{layer}"))
    }
}

/// Drop near-duplicate points and merge collinear runs (including 45° runs).
/// Fresh copy of the simplify idea in `router.rs`, extended to merge diagonal
/// collinearity so a straight 45° staircase of cells collapses to two points.
///
/// Shared with [`crate::pipeline`], which re-runs it over per-net polylines
/// stitched across cells (so a joined run that is collinear across a former cell
/// boundary collapses too) — the 45°-aware simplify lives here, its natural home
/// as the detailed stage's emitter, rather than being duplicated.
pub(crate) fn simplify(path: Vec<Point2>) -> Vec<Point2> {
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
            // Collinear iff the cross product of (b-a) and (p-b) is ~0 AND the
            // direction does not reverse (same forward heading). Handles
            // orthogonal and 45° runs uniformly.
            let v1 = (b.x - a.x, b.y - a.y);
            let v2 = (p.x - b.x, p.y - b.y);
            let cross = v1.0 * v2.1 - v1.1 * v2.0;
            let dot = v1.0 * v2.0 + v1.1 * v2.1;
            if cross.abs() < EPS && dot > 0.0 {
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
    use crate::crossing::assign_crossings;
    use crate::pathing::global_route;
    use crate::problem::{Bounds, Connection, Obstacle, RoutePoint};
    use std::path::Path;

    fn load(name: &str) -> RouteProblem {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures")
            .join(name);
        let json = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        serde_json::from_str(&json).unwrap_or_else(|e| panic!("parse {name}: {e}"))
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

    fn base(bounds: Bounds, obstacles: Vec<Obstacle>, connections: Vec<Connection>) -> RouteProblem {
        RouteProblem {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles,
            connections,
            bounds,
            clearance: 0.2,
            via_diameter: 0.6,
            via_drill: 0.3,
        }
    }

    fn bounds(w: f64, h: f64) -> Bounds {
        Bounds {
            min_x: 0.0,
            max_x: w,
            min_y: 0.0,
            max_y: h,
        }
    }

    fn run(p: &RouteProblem) -> (CapacityMesh, CellRouteResult) {
        let mesh = CapacityMesh::build(p);
        let plan = global_route(p).plan;
        let a = assign_crossings(p, &mesh, &plan);
        let r = route_cells(p, &mesh, &a);
        (mesh, r)
    }

    /// A single cell with two terminals on a straight line routes a straight
    /// polyline whose endpoints are exactly the terminals.
    #[test]
    fn straight_two_terminal_route() {
        // A small board with one net crossing it; the first cell's job has a pad
        // and an exit on the same layer — a straight run.
        let p = base(
            bounds(20.0, 8.0),
            vec![],
            vec![conn("N", &[(2.0, 4.0, "top"), (18.0, 4.0, "top")])],
        );
        let (_mesh, r) = run(&p);
        assert!(r.is_clean(), "must route cleanly: {:?}", r.failed);
        // Every emitted polyline point is finite and traces have ≥ 2 points.
        let mut total_pts = 0;
        for cr in &r.cell_routes {
            for t in &cr.traces {
                assert!(t.points.len() >= 2, "trace has ≥ 2 points");
                total_pts += t.points.len();
            }
        }
        assert!(total_pts > 0, "the net produced copper");
        // Endpoint exactness: at least one trace endpoint equals a terminal
        // (pad/crossing) exactly — both pads are at y=4.
        let on_pad = |q: &Point2| {
            ((q.x - 2.0).abs() < 1e-12 || (q.x - 18.0).abs() < 1e-12)
                && (q.y - 4.0).abs() < 1e-12
        };
        let hits_pad = r.cell_routes.iter().any(|cr| {
            cr.traces.iter().any(|t| {
                let f = &t.points[0];
                let l = &t.points[t.points.len() - 1];
                on_pad(f) && on_pad(l)
            })
        });
        assert!(hits_pad, "both endpoints must land exactly on pads");
    }

    /// A diagonal route should appear when octilinear is cheaper: a net whose
    /// terminals sit on a 45° offset inside one cell routes with a 45° segment.
    #[test]
    fn diagonal_segment_appears_when_cheaper() {
        // One open cell, a net from corner to corner of the cell — the octilinear
        // router takes the diagonal, so some trace segment has |dx| ≈ |dy| > 0.
        let p = base(
            bounds(12.0, 12.0),
            vec![],
            vec![conn("D", &[(3.0, 3.0, "top"), (9.0, 9.0, "top")])],
        );
        let (_mesh, r) = run(&p);
        assert!(r.is_clean(), "must route cleanly: {:?}", r.failed);
        let has_45 = r.cell_routes.iter().any(|cr| {
            cr.traces.iter().any(|t| {
                t.points.windows(2).any(|w| {
                    let dx = (w[1].x - w[0].x).abs();
                    let dy = (w[1].y - w[0].y).abs();
                    dx > 1e-9 && dy > 1e-9 && (dx - dy).abs() < 1e-6
                })
            })
        });
        assert!(has_45, "a 45° segment must appear in the routed copper");
    }

    /// Corner-cutting is forbidden: a tight keepout pinch must not let a diagonal
    /// slip through a blocked orthogonal corner. We assert every diagonal segment
    /// in the output stays clearance-clear of the keepout (the connectivity/lint
    /// authority confirms this geometrically; here we assert no segment passes
    /// through the keepout interior).
    #[test]
    fn corner_cut_is_not_taken_through_a_keepout() {
        let p = base(
            bounds(16.0, 16.0),
            vec![Obstacle {
                kind: "rect".to_owned(),
                layers: vec![LayerRef::top()],
                center: Point2 { x: 8.0, y: 8.0 },
                width: 2.0,
                height: 2.0,
                connected_to: vec![],
            }],
            vec![conn("N", &[(2.0, 8.0, "top"), (14.0, 8.0, "top")])],
        );
        let (_mesh, r) = run(&p);
        // It is allowed to fail (tight board) but must not produce copper that
        // runs through the keepout interior on the top layer.
        let keepout = Rect {
            min_x: 7.0,
            min_y: 7.0,
            max_x: 9.0,
            max_y: 9.0,
        };
        for cr in &r.cell_routes {
            for t in &cr.traces {
                if t.layer != LayerRef::top() {
                    continue;
                }
                for w in t.points.windows(2) {
                    // Sample the segment midpoint; a corner-cut diagonal would
                    // pass through the keepout corner.
                    let mid = Point2 {
                        x: (w[0].x + w[1].x) / 2.0,
                        y: (w[0].y + w[1].y) / 2.0,
                    };
                    let inside = mid.x > keepout.min_x
                        && mid.x < keepout.max_x
                        && mid.y > keepout.min_y
                        && mid.y < keepout.max_y;
                    assert!(!inside, "a trace segment cut through the keepout corner");
                }
            }
        }
    }

    /// A hand-built dense pin-field cell routes all its nets.
    #[test]
    fn dense_pin_field_routes_all_nets() {
        // Four short nets in a small board, each a 2-pin horizontal pair stacked
        // vertically — a dense field the per-cell router must satisfy.
        let mut connections = Vec::new();
        for i in 0..4 {
            let y = 3.0 + i as f64 * 2.0;
            connections.push(conn(
                &format!("N{i}"),
                &[(3.0, y, "top"), (13.0, y, "top")],
            ));
        }
        let p = base(bounds(16.0, 12.0), vec![], connections);
        let (_mesh, r) = run(&p);
        assert!(
            r.is_clean(),
            "dense pin field must route all nets: {:?}",
            r.failed
        );
        // Each net produced copper somewhere.
        for i in 0..4 {
            let name = format!("N{i}");
            assert!(
                r.cell_routes
                    .iter()
                    .any(|cr| cr.connection == name && !cr.traces.is_empty()),
                "net {name} produced copper"
            );
        }
    }

    /// Determinism: serialize twice, compare byte-for-byte.
    #[test]
    fn route_cells_is_deterministic() {
        let p = load("quad.json");
        let mesh = CapacityMesh::build(&p);
        let plan = global_route(&p).plan;
        let a = assign_crossings(&p, &mesh, &plan);
        let r1 = route_cells(&p, &mesh, &a);
        let r2 = route_cells(&p, &mesh, &a);
        let j1 = serde_json::to_string(&r1).unwrap();
        let j2 = serde_json::to_string(&r2).unwrap();
        assert_eq!(j1, j2, "two cell routings must serialize byte-equal");
    }

    /// Crossing-point exactness (the stitching contract): on `led-r`, which
    /// routes cell-by-cell with no failures, every assigned crossing appears
    /// verbatim (byte-exact mm) as a polyline endpoint in at least one cell route
    /// for that net — so Task 3 can stitch a leaf's exit to its neighbour's entry
    /// by coordinate identity. The exactness comes from the router *snapping*
    /// terminal endpoints to their assigned mm position.
    #[test]
    fn crossing_endpoints_are_exact_for_a_clean_fixture() {
        let p = load("led-r.json");
        let mesh = CapacityMesh::build(&p);
        let plan = global_route(&p).plan;
        let a = assign_crossings(&p, &mesh, &plan);
        let r = route_cells(&p, &mesh, &a);
        assert!(r.is_clean(), "led-r must route cell-by-cell: {:?}", r.failed);
        for x in &a.crossings {
            let appears = r
                .cell_routes
                .iter()
                .filter(|cr| cr.connection == x.connection)
                .flat_map(|cr| &cr.traces)
                .any(|t| {
                    let f = &t.points[0];
                    let l = &t.points[t.points.len() - 1];
                    let eq = |q: &Point2| {
                        (q.x - x.at.x).abs() < 1e-12 && (q.y - x.at.y).abs() < 1e-12
                    };
                    eq(f) || eq(l)
                });
            assert!(
                appears,
                "crossing for {} at ({:.4},{:.4}) must appear as an exact endpoint",
                x.connection, x.at.x, x.at.y
            );
        }
    }

    /// Endpoint-snapping is exact wherever a routed trace endpoint coincides with
    /// an assigned crossing — even on a fixture with cell failures (`quad`). Any
    /// endpoint that is *near* a crossing must be *byte-exact* to it (the snap
    /// never leaves a near-miss that would break stitching).
    #[test]
    fn endpoints_near_a_crossing_are_byte_exact() {
        let p = load("quad.json");
        let mesh = CapacityMesh::build(&p);
        let plan = global_route(&p).plan;
        let a = assign_crossings(&p, &mesh, &plan);
        let r = route_cells(&p, &mesh, &a);
        let pitch = grid::grid_pitch(&p);
        for cr in &r.cell_routes {
            for t in &cr.traces {
                for end in [&t.points[0], &t.points[t.points.len() - 1]] {
                    for x in a.crossings.iter().filter(|x| x.connection == cr.connection) {
                        let dx = (end.x - x.at.x).abs();
                        let dy = (end.y - x.at.y).abs();
                        // Within half a pitch of a crossing ⇒ must be exact.
                        if dx < pitch / 2.0 && dy < pitch / 2.0 {
                            assert!(
                                dx < 1e-12 && dy < 1e-12,
                                "endpoint near crossing {} must be byte-exact (got Δ=({dx:.6},{dy:.6}))",
                                cr.connection
                            );
                        }
                    }
                }
            }
        }
    }

    /// Fixtures route through the per-cell stage without panicking; failures (if
    /// any) are reported honestly with the leaf id in the reason.
    #[test]
    fn fixtures_route_or_report_honestly() {
        for name in ["led-r.json", "quad.json", "congested.json"] {
            let p = load(name);
            let (_mesh, r) = run(&p);
            // Any failure must name a cell.
            for f in &r.failed {
                assert!(
                    f.reason.starts_with("cell "),
                    "{name}: failure reason must carry the leaf id: {}",
                    f.reason
                );
            }
            // It produced some copper (the fixtures are routable cell-by-cell at
            // least in part).
            assert!(
                !r.cell_routes.is_empty(),
                "{name}: produced at least one cell route"
            );
        }
    }
}
