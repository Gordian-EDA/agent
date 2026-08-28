//! Per-cell detailed router: the second detailed-routing stage (slice 3, Task 2).
//!
//! [`crate::crossing::assign_crossings`] turns a slice-2 [`crate::pathing::GlobalPlan`] into a
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

use crate::astar::{self, AStarCosts, DIAG_COST, State};
use crate::crossing::{CellJob, CrossingAssignment, Terminal, TerminalKind};
use crate::grid::{self, Cell, RouteGrid};
use crate::heuristics::{
    connection_crossing_pressures, connection_obstacle_pressure_um,
    connection_segment_obstacle_pressure_um, connection_span_um,
};
use crate::mesh::{CapacityMesh, LeafId};
use crate::problem::Rect;
use crate::problem::{
    FailedNet, LayerRef, Point2, RouteProblem, RouteSolution, Trace, Via, ViaSpan,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

const FINISHER_MAX_MULTILAYER_CONNECTIONS: usize = 10;

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

/// One detailed-route pass candidate considered before the optional finisher.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DetailPassDiagnostic {
    /// Candidate index in the deterministic retry portfolio; zero is the ranked baseline.
    pub index: usize,
    /// Net order for this candidate, with duplicate per-cell jobs collapsed.
    pub net_order: Vec<String>,
    /// Geometry violations in the stitched successful-cell candidate.
    pub geometry: usize,
    /// Number of cell failures before any full-board finisher.
    pub fail_count: usize,
    /// Failed-pad weight, so multi-pin failures are visible.
    pub failed_pad_weight: usize,
    /// Failed net names for this candidate.
    pub failed: Vec<String>,
    /// Whether this candidate became the retained pre-finisher cell-route pass.
    pub selected: bool,
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
    route_cells_impl(problem, mesh, assignment, None)
}

/// As [`route_cells`], plus pre-finisher retry candidate diagnostics for tooling.
pub fn route_cells_with_diagnostics(
    problem: &RouteProblem,
    mesh: &CapacityMesh,
    assignment: &CrossingAssignment,
) -> (CellRouteResult, Vec<DetailPassDiagnostic>) {
    let mut diagnostics = Vec::new();
    let result = route_cells_impl(problem, mesh, assignment, Some(&mut diagnostics));
    (result, diagnostics)
}

fn route_cells_impl(
    problem: &RouteProblem,
    mesh: &CapacityMesh,
    assignment: &CrossingAssignment,
    mut diagnostics: Option<&mut Vec<DetailPassDiagnostic>>,
) -> CellRouteResult {
    let layer_count = problem.layer_count.max(1) as usize;
    let track_pitch = mesh.track_pitch;
    // The detailed stage routes on a grid FINER than the slice-1 design pitch: a
    // half-pitch lattice. The finer the grid, the closer an emitted cell-centre
    // point sits to the ideal centreline, so the grid-snap distortion the exact-
    // geometry lint measures at dense crossings shrinks (here to ≤ a quarter of the
    // design pitch). Slice-1 keeps the design pitch and is untouched.
    let pitch = grid::grid_pitch(problem) / 2.0;
    // Exact-geometry clearance halo: the legal centre-to-centre spacing between a
    // foreign trace and this net's copper is `min_trace_width + clearance` (mm,
    // Euclidean). The detailed router blocks foreign cells strictly inside that
    // distance — a foreign centreline at exactly the spacing is legal (its edge gap
    // equals `clearance`). We widen it by one cell-centre snap displacement (≤ half
    // a cell diagonal) so even a crossing endpoint snapped to its exact mm — off
    // its cell centre — keeps the legal spacing from a foreign interior cell. The
    // slice-1 full-board router keeps its Chebyshev halo; this is detail-only.
    let snap_disp = pitch * std::f64::consts::SQRT_2 / 2.0;
    let halo = problem.min_trace_width + problem.clearance;
    // A via is a through-hole disc: the legal centre-to-centre spacing from a via
    // to a foreign trace's centreline is `via_radius + clearance + trace
    // half-width`, again widened by the snap displacement and marked on every layer
    // (the barrel is through-hole).
    let via_halo =
        problem.via_diameter / 2.0 + problem.clearance + problem.min_trace_width / 2.0 + snap_disp;
    // A spontaneous mid-path via must keep its barrel clear of foreign copper by
    // the via clearance, so the search checks a Chebyshev halo of this many cells
    // on every layer before placing a via (conservatively covers the Euclidean disc
    // the lint measures).
    let via_clear_radius_cells = (via_halo / pitch).ceil() as usize;
    // Per-cell jobs route octilinearly (diagonals enabled): each job is confined
    // to its leaf window and the corner guard + per-cell occupancy keep 45° runs
    // legal.
    let costs = AStarCosts {
        diag: DIAG_COST,
        via_clear_radius_cells,
        via: 60,
        ..AStarCosts::default()
    };

    // Route jobs in global net-rank order first (slice-1 half-perimeter rank,
    // ties by name), and within a net by leaf id. If that first pass leaves
    // failures, retry a bounded portfolio of pressure-aware net orders on fresh
    // occupancy grids and keep the best. This mirrors the rip-up/order-variation
    // trick used by mature autorouters: a net that succeeds early can still claim
    // the only corridor a harder later net needed, and the finisher only repairs
    // failed nets rather than re-routing the blocker.
    let net_rank = net_rank(problem);
    let ranked_job_order = detail_ranked_job_order(assignment, &net_rank);
    let (mut cell_routes, mut failed, blocker_edges) = route_detail_pass(
        problem,
        mesh,
        assignment,
        &ranked_job_order,
        pitch,
        layer_count,
        track_pitch,
        halo,
        via_halo,
        costs,
    );
    let mut best_key = cell_route_candidate_key(problem, &cell_routes, &failed);
    let mut best_idx = 0usize;
    record_detail_pass_diagnostic(
        &mut diagnostics,
        0,
        &ranked_job_order,
        assignment,
        best_key,
        &failed,
    );
    if !failed.is_empty() {
        let mut retry_orders = detail_retry_job_orders(problem, assignment, &net_rank);
        if let Some(order) = detail_blocker_retry_job_order(assignment, &net_rank, &blocker_edges) {
            retry_orders.push(order);
            retry_orders.dedup();
        }
        for (idx, order) in retry_orders.iter().enumerate().skip(1) {
            let (candidate_routes, candidate_failed, _) = route_detail_pass(
                problem,
                mesh,
                assignment,
                order,
                pitch,
                layer_count,
                track_pitch,
                halo,
                via_halo,
                costs,
            );
            let candidate_key =
                cell_route_candidate_key(problem, &candidate_routes, &candidate_failed);
            record_detail_pass_diagnostic(
                &mut diagnostics,
                idx,
                order,
                assignment,
                candidate_key,
                &candidate_failed,
            );
            if keep_cell_route_candidate(candidate_key, idx, best_key, best_idx) {
                best_key = candidate_key;
                best_idx = idx;
                cell_routes = candidate_routes;
                failed = candidate_failed;
            }
        }
    }
    mark_selected_detail_pass_diagnostic(&mut diagnostics, best_idx);

    // ── Hotspot repair: per-net full-board finisher (slice 3, Task 3.5) ──────────
    //
    // The per-cell pass routes each net's traversal of one leaf, confined to that
    // leaf's window and obeying the global plan's crossing slots. Where a net's
    // crossings funnel through a saturated boundary or over-converge in one tiny
    // central leaf, a cell's A* runs out of room and the net fails — a routing-
    // *completeness* gap, not a geometry defect.
    //
    // For each failed net (deterministic slice-1 net order) we drop its partial
    // in-cell copper and re-route it **pad-to-pad on a fresh full-board grid** that
    // carries only the copper that survives into the solution (every successful
    // net's traces/vias + every net's pad anchor). The finisher routes
    // OCTILINEARLY (8-way); its diagonals are DRC-safe because every routed run is
    // marked as a swept-clearance CAPSULE ([`mark_segment_capsule`]), so a later net
    // keeps full clearance from the diagonal body (see [`run_finisher_pass`]). It is
    // bounded to the net's own corridor for speed, trying free pad-to-pad first and
    // a plan-guided wall-gap waypoint only as a fallback. Each repaired net's copper
    // is marked before the next runs, so repairs keep clearance from each other.
    // Nets that still cannot route stay honest `finisher: …` failures.
    let failed_names: std::collections::BTreeSet<String> =
        failed.iter().map(|f| f.connection.clone()).collect();
    if !failed_names.is_empty() && should_try_finisher(problem) {
        // A failed net's partial in-cell copper must not survive into the stitched
        // solution: per the partial-net rule it was already dropped (Task 3 skips
        // `failed` nets), and the finisher re-routes the whole net from scratch, so
        // its partial cell routes are removed here. Were they kept, a repaired net
        // (no longer in `failed`) would stitch its dangling per-cell stubs together
        // with the finisher copper into a disconnected tangle.
        cell_routes.retain(|cr| !failed_names.contains(&cr.connection));

        // Build a CLEAN finisher grid carrying only copper that survives into the
        // solution. The per-cell pass polluted the shared grid with two phantom
        // obstacles: the dropped partial copper of failed nets, and the up-front via
        // barrels reserved for failed nets' assigned (never-realised) via sites.
        // Both block foreign nets — including the very nets the finisher must repair
        // — so a finisher could fail against geometry that will not exist. Re-stamp
        // only the successful nets' emitted copper + vias into a fresh grid so the
        // finisher negotiates the real residual occupancy. (Rebuilding once for the
        // whole repair pass is cheap relative to the per-cell A* the pass replaced.)
        let finish_halo = halo;
        // The finisher routes on the per-cell stage's half-design pitch: fine enough
        // that a bare cell-centre endpoint sits within half a trace width of its pad
        // (so connectivity holds without snapping the endpoint off-centre — which a
        // coarser pitch would force, and which the exact lint then sees as a
        // sub-clearance near-miss against a neighbour's centre-aligned copper).
        let finish_pitch = pitch;
        let mut base_grid = RouteGrid::build_with_pitch(problem, finish_pitch);
        // Every net's route points (pad anchors) are copper that exists regardless
        // of whether the net routed. Stamp each as its net's owned cell + clearance
        // halo so a foreign finisher route keeps clear of it — without this, a
        // finisher net could run straight through an *unrouted* net's pad and short
        // it (the per-cell pass never does this because each job stays near its own
        // pads; the full-board finisher can wander anywhere). A net never blocks its
        // own pad, so this does not impede the owner's finisher.
        for conn in &problem.connections {
            let Some(ci) = base_grid.connection_index(&conn.name) else {
                continue;
            };
            for pt in &conn.points_to_connect {
                let s = route_point_cell(&base_grid, pt, layer_count);
                base_grid.mark_net_halo_euclid(s.layer, s.ix, s.iy, ci, finish_halo);
            }
        }
        for cr in &cell_routes {
            stamp_route(&mut base_grid, cr, finish_halo, via_halo);
        }

        // Per-net guidance waypoint: the SINGLE crossing nearest the board's centre
        // x — the saturated wall a hard board funnels every net through. Guiding only
        // this one waypoint (not all 10–17 internal cell-boundary crossings a net
        // accumulates) keeps the net in the negotiated wall gap while costing one
        // extra A* leg, not seventeen — the interior crossings are routed fine by the
        // free pad-to-pad search and need no guidance. A net whose plan never crosses
        // the wall gets no waypoint (empty lane ⇒ pure pad-to-pad).
        let cx_board = (problem.bounds.min_x + problem.bounds.max_x) / 2.0;
        let mut lanes: BTreeMap<String, Vec<Waypoint>> = BTreeMap::new();
        for x in &assignment.crossings {
            let w = Waypoint {
                at: x.at,
                layer: x.layer.min(layer_count - 1),
            };
            let slot = lanes.entry(x.connection.clone()).or_default();
            match slot.first() {
                Some(cur) if (cur.at.x - cx_board).abs() <= (w.at.x - cx_board).abs() => {}
                _ => {
                    slot.clear();
                    slot.push(w);
                }
            }
        }

        // The finisher routes failed nets one at a time, each marking copper the
        // next must clear — so the ORDER decides whether saturated hotspots (e.g.
        // congested's exactly-full wall) stay feasible: a bad order grabs a gap a
        // later net needed. We try a few deterministic candidate orderings and keep
        // the pass that repairs the most nets (tie-break: fewest, then the earliest
        // candidate, for stability). Each pass runs on its own clone of the residual
        // grid, so the trials are independent and the result is reproducible.
        // The finisher routes OCTILINEARLY (8-way), inheriting the per-cell
        // `DIAG_COST` via `..costs`. A diagonal finisher run is DRC-safe because its
        // copper is marked as a swept-clearance CAPSULE ([`mark_segment_capsule`]):
        // the exact Minkowski dilation of the centreline, not just a disc at each
        // vertex cell. So a later net is kept the full clearance from the diagonal
        // *body* — closing the cell-centre-halo notch around a 45° segment's midpoint
        // that made vertex-only marking unsafe (two adjacent 45° lanes dipping under
        // clearance). Diagonals give the finisher routing freedom (and neater copper)
        // on the hardest hotspot nets. The corner-cut guard in the A* core keeps a 45°
        // step from slipping through a blocked corner.
        let finish_costs = AStarCosts {
            diag: DIAG_COST,
            via_clear_radius_cells: (via_halo / finish_pitch).ceil() as usize,
            ..costs
        };
        // The diagonal-safe clearance is the capsule MARK, so the finisher may go
        // octilinear: assert diagonals are ON (and routable) rather than the old
        // orthogonal-only sentinel.
        debug_assert_ne!(finish_costs.diag, u32::MAX, "finisher routes octilinearly");
        debug_assert_eq!(
            finish_costs.diag, DIAG_COST,
            "finisher uses the octilinear diagonal cost"
        );
        // Repair the failed nets with a tiny deterministic order portfolio. This is the
        // detailed-router analog of Freerouting-style rip-up order variation: in a
        // saturated hotspot, the first repaired net claims the scarce corridor, so a
        // different order can make the difference between routing N and N-1 nets. Each
        // trial runs on an independent clone of the residual grid; the current rank order
        // is tried first and wins exact ties, preserving existing output unless another
        // order repairs more nets.
        let orders = finisher_orders(problem, &failed_names, &net_rank);
        let mut best_routes = Vec::new();
        let mut best_fail = Vec::new();
        let mut best_key = FinisherCandidateKey::worst();
        let mut best_idx = usize::MAX;
        for (idx, order) in orders.iter().enumerate() {
            let mut g = base_grid.clone();
            let (routes, finisher_fail) = run_finisher_pass(
                problem,
                &mut g,
                order,
                &lanes,
                layer_count,
                finish_halo,
                via_halo,
                finish_costs,
            );
            let key = finisher_candidate_key(problem, &routes, &finisher_fail);
            if keep_finisher_candidate(key, idx, best_key, best_idx) {
                best_key = key;
                best_idx = idx;
                best_routes = routes;
                best_fail = finisher_fail;
            }
        }

        cell_routes.extend(best_routes);
        // Drop every per-cell failure of a net the incremental finisher touched;
        // reinstate it as a single honest finisher failure if it could not complete.
        failed.retain(|f| !failed_names.contains(&f.connection));
        failed.extend(best_fail);
    } else if !failed_names.is_empty() {
        let reason = finisher_skip_reason(problem);
        for failure in &mut failed {
            failure.reason = format!("{}; {reason}", failure.reason);
        }
    }

    CellRouteResult {
        cell_routes,
        failed,
    }
}

fn detail_ranked_job_order(
    assignment: &CrossingAssignment,
    net_rank: &BTreeMap<String, usize>,
) -> Vec<usize> {
    let mut order: Vec<usize> = (0..assignment.jobs.len()).collect();
    order.sort_by(|&a, &b| {
        let ja = &assignment.jobs[a];
        let jb = &assignment.jobs[b];
        let ra = net_rank.get(&ja.connection).copied().unwrap_or(usize::MAX);
        let rb = net_rank.get(&jb.connection).copied().unwrap_or(usize::MAX);
        ra.cmp(&rb)
            .then_with(|| ja.connection.cmp(&jb.connection))
            .then_with(|| ja.leaf.cmp(&jb.leaf))
    });
    order
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct DetailBlockerEdge {
    blocked: String,
    blocker: String,
}

fn detail_retry_job_orders(
    problem: &RouteProblem,
    assignment: &CrossingAssignment,
    net_rank: &BTreeMap<String, usize>,
) -> Vec<Vec<usize>> {
    let names: std::collections::BTreeSet<String> = assignment
        .jobs
        .iter()
        .map(|job| job.connection.clone())
        .collect();
    let mut orders: Vec<Vec<usize>> = finisher_orders(problem, &names, net_rank)
        .into_iter()
        .map(|net_order| detail_job_order(assignment, &net_order))
        .collect();
    orders.dedup();
    if orders.is_empty() {
        orders.push(Vec::new());
    }
    orders
}

fn detail_blocker_retry_job_order(
    assignment: &CrossingAssignment,
    net_rank: &BTreeMap<String, usize>,
    blocker_edges: &[DetailBlockerEdge],
) -> Option<Vec<usize>> {
    if blocker_edges.is_empty() {
        return None;
    }
    let net_order = detail_blocker_retry_net_order(assignment, net_rank, blocker_edges);
    (!net_order.is_empty()).then(|| detail_job_order(assignment, &net_order))
}

fn detail_blocker_retry_net_order(
    assignment: &CrossingAssignment,
    net_rank: &BTreeMap<String, usize>,
    blocker_edges: &[DetailBlockerEdge],
) -> Vec<String> {
    let mut names: BTreeSet<String> = assignment
        .jobs
        .iter()
        .map(|job| job.connection.clone())
        .collect();
    let mut outgoing: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut indegree: BTreeMap<String, usize> =
        names.iter().map(|name| (name.clone(), 0)).collect();

    for edge in blocker_edges {
        if edge.blocked == edge.blocker
            || !names.contains(&edge.blocked)
            || !names.contains(&edge.blocker)
        {
            continue;
        }
        if outgoing
            .entry(edge.blocked.clone())
            .or_default()
            .insert(edge.blocker.clone())
        {
            *indegree.entry(edge.blocker.clone()).or_default() += 1;
        }
    }

    if outgoing.is_empty() {
        return Vec::new();
    }

    let mut order = Vec::with_capacity(names.len());
    while !names.is_empty() {
        let next = names
            .iter()
            .filter(|name| indegree.get(*name).copied().unwrap_or(0) == 0)
            .min_by(|a, b| compare_detail_net_order(a, b, net_rank))
            .cloned()
            .or_else(|| {
                names
                    .iter()
                    .min_by(|a, b| {
                        indegree
                            .get(*a)
                            .copied()
                            .unwrap_or(0)
                            .cmp(&indegree.get(*b).copied().unwrap_or(0))
                            .then_with(|| compare_detail_net_order(a, b, net_rank))
                    })
                    .cloned()
            })
            .expect("non-empty name set has a next item");
        names.remove(&next);
        if let Some(blockers) = outgoing.get(&next) {
            for blocker in blockers {
                if let Some(count) = indegree.get_mut(blocker) {
                    *count = count.saturating_sub(1);
                }
            }
        }
        order.push(next);
    }
    order
}

fn compare_detail_net_order(
    a: &str,
    b: &str,
    net_rank: &BTreeMap<String, usize>,
) -> std::cmp::Ordering {
    let ra = net_rank.get(a).copied().unwrap_or(usize::MAX);
    let rb = net_rank.get(b).copied().unwrap_or(usize::MAX);
    ra.cmp(&rb).then_with(|| a.cmp(b))
}

fn detail_job_order(assignment: &CrossingAssignment, net_order: &[String]) -> Vec<usize> {
    let rank: BTreeMap<&str, usize> = net_order
        .iter()
        .enumerate()
        .map(|(idx, name)| (name.as_str(), idx))
        .collect();
    let mut order: Vec<usize> = (0..assignment.jobs.len()).collect();
    order.sort_by(|&a, &b| {
        let ja = &assignment.jobs[a];
        let jb = &assignment.jobs[b];
        rank.get(ja.connection.as_str())
            .copied()
            .unwrap_or(usize::MAX)
            .cmp(
                &rank
                    .get(jb.connection.as_str())
                    .copied()
                    .unwrap_or(usize::MAX),
            )
            .then_with(|| ja.connection.cmp(&jb.connection))
            .then_with(|| ja.leaf.cmp(&jb.leaf))
    });
    order
}

fn record_detail_pass_diagnostic(
    diagnostics: &mut Option<&mut Vec<DetailPassDiagnostic>>,
    index: usize,
    job_order: &[usize],
    assignment: &CrossingAssignment,
    key: DetailPassCandidateKey,
    failures: &[FailedNet],
) {
    let Some(diagnostics) = diagnostics.as_mut() else {
        return;
    };
    diagnostics.push(DetailPassDiagnostic {
        index,
        net_order: diagnostic_net_order(assignment, job_order),
        geometry: key.geometry,
        fail_count: key.fail_count,
        failed_pad_weight: key.failed_pad_weight,
        failed: failures
            .iter()
            .map(|failure| failure.connection.clone())
            .collect(),
        selected: false,
    });
}

fn mark_selected_detail_pass_diagnostic(
    diagnostics: &mut Option<&mut Vec<DetailPassDiagnostic>>,
    selected_idx: usize,
) {
    if let Some(diagnostics) = diagnostics.as_mut() {
        for diagnostic in diagnostics.iter_mut() {
            diagnostic.selected = diagnostic.index == selected_idx;
        }
    }
}

fn diagnostic_net_order(assignment: &CrossingAssignment, job_order: &[usize]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for &idx in job_order {
        let Some(job) = assignment.jobs.get(idx) else {
            continue;
        };
        if seen.insert(job.connection.clone()) {
            out.push(job.connection.clone());
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn route_detail_pass(
    problem: &RouteProblem,
    mesh: &CapacityMesh,
    assignment: &CrossingAssignment,
    job_order: &[usize],
    pitch: f64,
    layer_count: usize,
    track_pitch: f64,
    halo: f64,
    via_halo: f64,
    costs: AStarCosts,
) -> (Vec<CellRoute>, Vec<FailedNet>, Vec<DetailBlockerEdge>) {
    let mut cell_routes: Vec<CellRoute> = Vec::new();
    let mut failed: Vec<FailedNet> = Vec::new();
    let mut blocker_edges: Vec<DetailBlockerEdge> = Vec::new();

    // One shared full-board grid (at the fine detailed pitch) for the whole pass.
    // Per-cell window grids are blind to each other's copper; this shared grid
    // makes all already-routed copper visible while each A* remains bounded to
    // the leaf window.
    let mut grid = RouteGrid::build_with_pitch(problem, pitch);

    let rollback_failed_nets = !should_try_finisher(problem);

    // Reserve every assigned via barrel up front so foreign traces in earlier
    // jobs cannot claim a future via site. The owning net can still pass its own
    // barrel because the occupancy is tagged by connection.
    for job in &assignment.jobs {
        reserve_job_vias(&mut grid, job, via_halo);
    }

    if rollback_failed_nets {
        route_detail_pass_with_net_rollback(
            problem,
            mesh,
            assignment,
            job_order,
            &mut grid,
            &mut cell_routes,
            &mut failed,
            &mut blocker_edges,
            track_pitch,
            layer_count,
            halo,
            via_halo,
            costs,
        );
    } else {
        for &job_idx in job_order {
            let Some(job) = assignment.jobs.get(job_idx) else {
                continue;
            };
            let leaf_rect = &mesh.leaves[job.leaf].rect;
            let window = leaf_rect.inflate_clamped_to(track_pitch, &problem.bounds);
            match route_one_job(
                problem,
                job,
                &mut grid,
                &window,
                layer_count,
                halo,
                via_halo,
                costs,
            ) {
                Ok(route) => {
                    cell_routes.push(route);
                }
                Err(err) => {
                    record_detail_failure(job, err, &mut failed, &mut blocker_edges);
                }
            }
        }
    }

    (cell_routes, failed, blocker_edges)
}

#[allow(clippy::too_many_arguments)]
fn route_detail_pass_with_net_rollback(
    problem: &RouteProblem,
    mesh: &CapacityMesh,
    assignment: &CrossingAssignment,
    job_order: &[usize],
    grid: &mut RouteGrid,
    cell_routes: &mut Vec<CellRoute>,
    failed: &mut Vec<FailedNet>,
    blocker_edges: &mut Vec<DetailBlockerEdge>,
    track_pitch: f64,
    layer_count: usize,
    halo: f64,
    via_halo: f64,
    costs: AStarCosts,
) {
    let mut pos = 0usize;
    while pos < job_order.len() {
        let Some(first_job) = assignment.jobs.get(job_order[pos]) else {
            pos += 1;
            continue;
        };
        let connection = first_job.connection.clone();
        let start = pos;
        pos += 1;
        while pos < job_order.len()
            && assignment
                .jobs
                .get(job_order[pos])
                .is_some_and(|job| job.connection == connection)
        {
            pos += 1;
        }

        let mut trial_grid = grid.clone();

        let mut trial_routes = Vec::new();
        let mut group_failed = false;
        for &job_idx in &job_order[start..pos] {
            let Some(job) = assignment.jobs.get(job_idx) else {
                continue;
            };
            let leaf_rect = &mesh.leaves[job.leaf].rect;
            let window = leaf_rect.inflate_clamped_to(track_pitch, &problem.bounds);
            match route_one_job(
                problem,
                job,
                &mut trial_grid,
                &window,
                layer_count,
                halo,
                via_halo,
                costs,
            ) {
                Ok(route) => trial_routes.push(route),
                Err(err) => {
                    record_detail_failure(job, err, failed, blocker_edges);
                    group_failed = true;
                    break;
                }
            }
        }

        if !group_failed {
            *grid = trial_grid;
            cell_routes.extend(trial_routes);
        }
    }
}

fn record_detail_failure(
    job: &CellJob,
    err: DetailRouteError,
    failed: &mut Vec<FailedNet>,
    blocker_edges: &mut Vec<DetailBlockerEdge>,
) {
    for blocker in err.foreign_owners {
        if blocker != job.connection {
            blocker_edges.push(DetailBlockerEdge {
                blocked: job.connection.clone(),
                blocker,
            });
        }
    }
    failed.push(FailedNet {
        connection: job.connection.clone(),
        reason: format!("cell {}: {}", job.leaf, err.reason),
    });
}

fn reserve_job_vias(grid: &mut RouteGrid, job: &CellJob, via_halo: f64) {
    let Some(conn_idx) = grid.connection_index(&job.connection) else {
        return;
    };
    for t in &job.terminals {
        if t.kind == TerminalKind::Via {
            let (vx, vy) = grid.cell_of(t.at.x, t.at.y);
            for l in 0..grid.layer_count {
                grid.mark_net_halo_euclid(l, vx, vy, conn_idx, via_halo);
            }
        }
    }
}

fn should_try_finisher(problem: &RouteProblem) -> bool {
    problem.layer_count <= 2 || problem.connections.len() <= FINISHER_MAX_MULTILAYER_CONNECTIONS
}

fn finisher_skip_reason(problem: &RouteProblem) -> String {
    format!(
        "finisher skipped for large multilayer board (layers={}, connections={} > cap={})",
        problem.layer_count,
        problem.connections.len(),
        FINISHER_MAX_MULTILAYER_CONNECTIONS
    )
}

fn keep_cell_route_candidate(
    candidate: DetailPassCandidateKey,
    candidate_idx: usize,
    incumbent: DetailPassCandidateKey,
    incumbent_idx: usize,
) -> bool {
    (candidate, candidate_idx) < (incumbent, incumbent_idx)
}

fn cell_route_candidate_key(
    problem: &RouteProblem,
    routes: &[CellRoute],
    failures: &[FailedNet],
) -> DetailPassCandidateKey {
    let failed_names: BTreeSet<String> = failures
        .iter()
        .map(|failure| failure.connection.clone())
        .collect();
    let solution = cell_routes_to_solution(problem, routes, &failed_names);
    DetailPassCandidateKey {
        geometry: crate::router::geometry_violations(problem, &solution),
        fail_count: failures.len(),
        failed_pad_weight: crate::problem::failed_pad_weight(problem, failures),
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
struct DetailPassCandidateKey {
    geometry: usize,
    fail_count: usize,
    failed_pad_weight: usize,
}

fn cell_routes_to_solution(
    problem: &RouteProblem,
    routes: &[CellRoute],
    failed_names: &BTreeSet<String>,
) -> RouteSolution {
    let mut solution = RouteSolution {
        traces: Vec::new(),
        vias: Vec::new(),
    };
    for route in routes {
        if failed_names.contains(&route.connection) {
            continue;
        }
        for trace in &route.traces {
            if trace.points.len() >= 2 {
                solution.traces.push(Trace {
                    connection: route.connection.clone(),
                    layer: trace.layer.clone(),
                    width: problem.min_trace_width,
                    path: trace.points.clone(),
                });
            }
        }
        for via in &route.vias {
            solution.vias.push(Via {
                connection: route.connection.clone(),
                at: via.at,
                diameter: problem.via_diameter,
                drill: problem.via_drill,
                span: ViaSpan::Through,
            });
        }
    }
    solution
}

/// The deterministic order the finisher repairs the failed nets in: ascending
/// slice-1 net rank (bounding-box half-perimeter, name tie-break) — the same order
/// the global stage and per-cell pass used, so a repaired net keeps clearance from
/// the copper laid before it and the whole detailed stage stays order-consistent.
fn finisher_order(
    names_set: &std::collections::BTreeSet<String>,
    net_rank: &BTreeMap<String, usize>,
) -> Vec<String> {
    let mut names: Vec<String> = names_set.iter().cloned().collect();
    names.sort_by(|a, b| {
        let ra = net_rank.get(a).copied().unwrap_or(usize::MAX);
        let rb = net_rank.get(b).copied().unwrap_or(usize::MAX);
        ra.cmp(&rb).then_with(|| a.cmp(b))
    });
    names
}

/// Deterministic finisher order portfolio. The first order is the slice-1 rank
/// order and remains the exact-tie winner. The alternatives target common
/// hotspot cases: long/hard nets first, reverse corridor claiming, and pure name
/// order to break rank ties differently while staying byte-stable.
fn finisher_orders(
    problem: &RouteProblem,
    names_set: &std::collections::BTreeSet<String>,
    net_rank: &BTreeMap<String, usize>,
) -> Vec<Vec<String>> {
    let mut orders: Vec<Vec<String>> = Vec::new();
    let metrics = finisher_order_metrics(problem, names_set);

    let rank = finisher_order(names_set, net_rank);
    orders.push(rank.clone());

    let mut reverse_rank = rank.clone();
    reverse_rank.reverse();
    orders.push(reverse_rank);

    let mut by_name: Vec<String> = names_set.iter().cloned().collect();
    by_name.sort();
    orders.push(by_name);

    let mut hardest_first: Vec<String> = names_set.iter().cloned().collect();
    hardest_first.sort_by(|a, b| {
        let ma = finisher_metric(&metrics, a);
        let mb = finisher_metric(&metrics, b);
        mb.pin_count
            .cmp(&ma.pin_count)
            .then_with(|| mb.crossing_pressure.cmp(&ma.crossing_pressure))
            .then_with(|| {
                mb.segment_obstacle_pressure_um
                    .cmp(&ma.segment_obstacle_pressure_um)
            })
            .then_with(|| mb.obstacle_pressure_um.cmp(&ma.obstacle_pressure_um))
            .then_with(|| mb.span_um.cmp(&ma.span_um))
            .then_with(|| a.cmp(b))
    });
    orders.push(hardest_first);

    let mut crossing_first: Vec<String> = names_set.iter().cloned().collect();
    crossing_first.sort_by(|a, b| {
        let ma = finisher_metric(&metrics, a);
        let mb = finisher_metric(&metrics, b);
        mb.crossing_pressure
            .cmp(&ma.crossing_pressure)
            .then_with(|| {
                mb.segment_obstacle_pressure_um
                    .cmp(&ma.segment_obstacle_pressure_um)
            })
            .then_with(|| mb.obstacle_pressure_um.cmp(&ma.obstacle_pressure_um))
            .then_with(|| mb.span_um.cmp(&ma.span_um))
            .then_with(|| mb.pin_count.cmp(&ma.pin_count))
            .then_with(|| a.cmp(b))
    });
    orders.push(crossing_first);

    let mut segment_crowded_first: Vec<String> = names_set.iter().cloned().collect();
    segment_crowded_first.sort_by(|a, b| {
        let ma = finisher_metric(&metrics, a);
        let mb = finisher_metric(&metrics, b);
        mb.segment_obstacle_pressure_um
            .cmp(&ma.segment_obstacle_pressure_um)
            .then_with(|| mb.obstacle_pressure_um.cmp(&ma.obstacle_pressure_um))
            .then_with(|| mb.crossing_pressure.cmp(&ma.crossing_pressure))
            .then_with(|| mb.span_um.cmp(&ma.span_um))
            .then_with(|| mb.pin_count.cmp(&ma.pin_count))
            .then_with(|| a.cmp(b))
    });
    orders.push(segment_crowded_first);

    let mut crowded_first: Vec<String> = names_set.iter().cloned().collect();
    crowded_first.sort_by(|a, b| {
        let ma = finisher_metric(&metrics, a);
        let mb = finisher_metric(&metrics, b);
        mb.obstacle_pressure_um
            .cmp(&ma.obstacle_pressure_um)
            .then_with(|| mb.crossing_pressure.cmp(&ma.crossing_pressure))
            .then_with(|| mb.span_um.cmp(&ma.span_um))
            .then_with(|| mb.pin_count.cmp(&ma.pin_count))
            .then_with(|| a.cmp(b))
    });
    orders.push(crowded_first);

    orders.dedup();
    orders
}

#[derive(Debug, Clone, Copy, Default)]
struct FinisherOrderMetric {
    pin_count: usize,
    span_um: u64,
    segment_obstacle_pressure_um: u64,
    obstacle_pressure_um: u64,
    crossing_pressure: usize,
}

fn finisher_order_metrics(
    problem: &RouteProblem,
    names_set: &std::collections::BTreeSet<String>,
) -> BTreeMap<String, FinisherOrderMetric> {
    let crossing_pressures = connection_crossing_pressures(problem);
    problem
        .connections
        .iter()
        .enumerate()
        .filter(|(_, conn)| names_set.contains(&conn.name))
        .map(|(idx, conn)| {
            (
                conn.name.clone(),
                FinisherOrderMetric {
                    pin_count: conn.points_to_connect.len(),
                    span_um: connection_span_um(conn),
                    segment_obstacle_pressure_um: connection_segment_obstacle_pressure_um(
                        problem, conn,
                    ),
                    obstacle_pressure_um: connection_obstacle_pressure_um(problem, conn),
                    crossing_pressure: crossing_pressures.get(idx).copied().unwrap_or(0),
                },
            )
        })
        .collect()
}

fn finisher_metric(
    metrics: &BTreeMap<String, FinisherOrderMetric>,
    name: &str,
) -> FinisherOrderMetric {
    metrics.get(name).copied().unwrap_or_default()
}

/// Run one finisher pass over `grid` (already a clone) routing the failed nets in
/// `order`. Returns the produced finisher routes and the honest per-net failures
/// for nets that could not be completed (free pad-to-pad first, plan-guided
/// fallback). The grid is mutated as nets are committed.
#[allow(clippy::too_many_arguments)]
fn run_finisher_pass(
    problem: &RouteProblem,
    grid: &mut RouteGrid,
    order: &[String],
    lanes: &BTreeMap<String, Vec<Waypoint>>,
    layer_count: usize,
    finish_halo: f64,
    via_halo: f64,
    costs: AStarCosts,
) -> (Vec<CellRoute>, Vec<FailedNet>) {
    let mut routes: Vec<CellRoute> = Vec::new();
    let mut fails: Vec<FailedNet> = Vec::new();
    let empty: Vec<Waypoint> = Vec::new();
    for name in order {
        let Some(conn) = problem.connections.iter().find(|c| &c.name == name) else {
            continue;
        };
        let waypoints = lanes.get(name).unwrap_or(&empty);

        // Attempts, tried in order until one succeeds:
        //  1. free pad-to-pad, NO vias — fast: a planar A* skips the per-cell via-
        //     barrel clearance scan, the dominant cost of a full-board 2-layer search,
        //     and succeeds for the common single-layer hotspot (e.g. congested's wall).
        //  2. free pad-to-pad, vias allowed — for a net that needs a via to detour.
        //  3. plan-guided through its wall-gap waypoint, vias allowed — when the free
        //     search grabbed a gap a later net needed.
        // The attempt planner skips impossible no-via searches for layer-changing nets
        // and skips the guided attempt when no waypoint exists, avoiding a duplicate
        // free+via full-board A*.
        // Each attempt runs on a clone so a failed attempt leaves no copper on the
        // committed grid; the first success replaces it.
        let mut committed: Option<(RouteGrid, CellRoute)> = None;
        let mut last_err = String::from("no path");
        for attempt in finisher_attempts(conn, !waypoints.is_empty()) {
            let attempt_costs = AStarCosts {
                allow_via: attempt.allow_via(),
                ..costs
            };
            let wps = if attempt.uses_waypoints() {
                waypoints
            } else {
                &empty
            };
            let mut trial = grid.clone();
            match finish_net(
                conn,
                wps,
                &mut trial,
                layer_count,
                finish_halo,
                via_halo,
                attempt_costs,
            ) {
                Ok(route) => {
                    committed = Some((trial, route));
                    break;
                }
                Err(e) => last_err = e,
            }
        }
        match committed {
            Some((trial, route)) => {
                *grid = trial;
                routes.push(route);
            }
            None => fails.push(FailedNet {
                connection: name.clone(),
                reason: format!("finisher: {last_err}"),
            }),
        }
    }
    (routes, fails)
}

fn keep_finisher_candidate(
    candidate: FinisherCandidateKey,
    candidate_idx: usize,
    incumbent: FinisherCandidateKey,
    incumbent_idx: usize,
) -> bool {
    (candidate, candidate_idx) < (incumbent, incumbent_idx)
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum FinishAttempt {
    Planar,
    Free,
    Guided,
}

impl FinishAttempt {
    fn allow_via(self) -> bool {
        !matches!(self, FinishAttempt::Planar)
    }

    fn uses_waypoints(self) -> bool {
        matches!(self, FinishAttempt::Guided)
    }
}

fn finisher_attempts(conn: &crate::problem::Connection, has_waypoints: bool) -> Vec<FinishAttempt> {
    let mut attempts = Vec::with_capacity(3);
    if same_terminal_layer(conn) {
        attempts.push(FinishAttempt::Planar);
    }
    attempts.push(FinishAttempt::Free);
    if has_waypoints {
        attempts.push(FinishAttempt::Guided);
    }
    attempts
}

fn same_terminal_layer(conn: &crate::problem::Connection) -> bool {
    conn.points_to_connect.first().is_none_or(|first| {
        conn.points_to_connect
            .iter()
            .all(|pt| pt.layer == first.layer)
    })
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
struct FinisherCandidateKey {
    fail_count: usize,
    failed_pad_weight: usize,
    via_count: usize,
    wirelength_um: u64,
}

impl FinisherCandidateKey {
    fn worst() -> Self {
        Self {
            fail_count: usize::MAX,
            failed_pad_weight: usize::MAX,
            via_count: usize::MAX,
            wirelength_um: u64::MAX,
        }
    }
}

fn finisher_candidate_key(
    problem: &RouteProblem,
    routes: &[CellRoute],
    failures: &[FailedNet],
) -> FinisherCandidateKey {
    FinisherCandidateKey {
        fail_count: failures.len(),
        failed_pad_weight: crate::problem::failed_pad_weight(problem, failures),
        via_count: cell_route_via_count(routes),
        wirelength_um: cell_route_wirelength_um(routes),
    }
}

fn cell_route_via_count(routes: &[CellRoute]) -> usize {
    routes.iter().map(|route| route.vias.len()).sum()
}

fn cell_route_wirelength_um(routes: &[CellRoute]) -> u64 {
    let wirelength = routes
        .iter()
        .flat_map(|route| route.traces.iter())
        .flat_map(|trace| trace.points.windows(2))
        .map(|w| w[1].dist(w[0]))
        .sum::<f64>();
    (wirelength * 1000.0).round() as u64
}

/// Stamp one [`CellRoute`]'s emitted copper into `grid` as its net's occupancy:
/// every trace polyline's swept clearance **capsule** and every via barrel (via halo
/// on every layer). Used to rebuild a clean finisher grid from only the copper that
/// survives into the solution, so a later finisher net keeps the full clearance from
/// the diagonal *body* of this copper (not just its vertex cells — see
/// [`mark_segment_capsule`]). The owning net is found by name; an unknown connection
/// is skipped (it would carry no foreign-blocking weight anyway).
fn stamp_route(grid: &mut RouteGrid, cr: &CellRoute, halo: f64, via_halo: f64) {
    let Some(conn_idx) = grid.connection_index(&cr.connection) else {
        return;
    };
    for t in &cr.traces {
        let layer = t
            .layer
            .index(grid.layer_count as u32)
            .unwrap_or(0)
            .min(grid.layer_count as u32 - 1) as usize;
        for w in t.points.windows(2) {
            mark_segment_capsule(grid, layer, &w[0], &w[1], conn_idx, halo);
        }
    }
    for v in &cr.vias {
        let (vx, vy) = grid.cell_of(v.at.x, v.at.y);
        for l in 0..grid.layer_count {
            grid.mark_net_halo_euclid(l, vx, vy, conn_idx, via_halo);
        }
    }
}

/// Stamp the **exact swept-clearance capsule** of trace segment `a`–`b` (mm) on
/// `layer` as copper owned by `conn`: every grid cell whose centre lies strictly
/// within `halo` (= `min_trace_width + clearance`, mm) of the *segment body* is
/// marked, not just the cells under the segment's endpoints.
///
/// This is the detailed router's diagonal-safe clearance primitive. The per-cell
/// Euclidean halo ([`RouteGrid::mark_net_halo_euclid`]) stamps a disc only around a
/// path's discrete cell **centres**; for a 45° run the union of those discs leaves a
/// thin un-stamped notch around each diagonal segment's midpoint, so a foreign trace
/// could dip under clearance against the diagonal *body* there (orthogonal runs have
/// no such notch — cell-centre spacing equals the perpendicular segment spacing).
/// Marking the analytic capsule — cell centre within `halo` of the segment, via exact
/// point-to-segment distance — closes the notch with zero residual: the owned region
/// is the true Minkowski dilation of the centreline, so two octilinear traces are
/// kept the full clearance apart by construction. Marking only adds obstacles for
/// FOREIGN nets (`conn` is never blocked from its own copper), so A* stays optimal and
/// the result deterministic. The scan is bounded to the segment's bbox dilated by
/// `halo`. A degenerate (zero-length) segment falls back to the single end cell's halo.
fn mark_segment_capsule(
    grid: &mut RouteGrid,
    layer: usize,
    a: &Point2,
    b: &Point2,
    conn: usize,
    halo: f64,
) {
    if layer >= grid.layer_count {
        return;
    }
    // A foreign centre exactly `halo` away is legal copper (its edge gap equals
    // `clearance`), so only strictly-closer centres are owned — matching the exact
    // DRC trace/trace edge-gap test and [`RouteGrid::mark_net_halo_euclid`].
    let thresh = halo - 1e-9;
    let thresh2 = thresh * thresh;
    // Cells whose centre could be within `halo` of the segment lie in its bbox
    // dilated by `halo`; clamp to the grid and test each by exact distance.
    let lo_x = a.x.min(b.x) - halo;
    let hi_x = a.x.max(b.x) + halo;
    let lo_y = a.y.min(b.y) - halo;
    let hi_y = a.y.max(b.y) + halo;
    let (ix0, iy0) = grid.cell_of(lo_x, lo_y);
    let (ix1, iy1) = grid.cell_of(hi_x, hi_y);
    let seg = geom::Segment::new(*a, *b);
    for ix in ix0..=ix1 {
        let cx = grid.cell_center_x(ix);
        for iy in iy0..=iy1 {
            let cy = grid.cell_center_y(iy);
            if seg.dist2_to_point(Point2::new(cx, cy)) <= thresh2 {
                grid.mark_net(layer, ix, iy, conn);
            }
        }
    }
}

/// Stamp a freshly routed cell `path`'s swept-clearance capsule into `grid` as
/// copper owned by `conn`, so a later net keeps the full clearance from this run's
/// diagonal *body* (see [`mark_segment_capsule`] for why vertex-only marking is
/// unsafe once the search routes octilinearly). Each same-layer segment of the path
/// is marked as a capsule; a layer change (via) breaks the run, so the two adjacent
/// states are marked as their own (single-cell) capsules on their respective layers.
/// Every state's own cell is haloed too, so a single-cell path still reserves its
/// clearance.
fn mark_path_capsule(grid: &mut RouteGrid, path: &[State], conn: usize, halo: f64) {
    if path.is_empty() {
        return;
    }
    // Precompute each state's cell-centre mm so the mark loop can borrow the grid
    // mutably (the closure would otherwise hold an immutable borrow of `grid`).
    let mm: Vec<Point2> = path
        .iter()
        .map(|s| Point2 {
            x: grid.cell_center_x(s.ix),
            y: grid.cell_center_y(s.iy),
        })
        .collect();
    // Halo every vertex cell (covers a lone-state path and the via endpoints).
    for (s, p) in path.iter().zip(&mm) {
        mark_segment_capsule(grid, s.layer, p, p, conn, halo);
    }
    // Capsule every same-layer segment body.
    for i in 0..path.len() - 1 {
        if path[i].layer == path[i + 1].layer {
            mark_segment_capsule(grid, path[i].layer, &mm[i], &mm[i + 1], conn, halo);
        }
    }
}

/// Re-route one failed net on the shared full-board `grid` — the hotspot finisher
/// (slice 3, Task 3.5).
///
/// Mirrors the slice-1 per-net tree routing ([`crate::router::route`]) on the
/// detailed stage's fine grid with the diagonal-safe swept-clearance capsule
/// ([`mark_segment_capsule`]): route point 0 seeds a routed tree; each further
/// `points_to_connect` (and, when `waypoints` is non-empty, each wall-gap waypoint
/// first) is A*-routed to the nearest tree cell, bounded to the net's own corridor
/// ([`leg_bounds`]). The move set and via policy come from `costs` (the caller routes
/// octilinearly and tries no-via before with-via). The net's own partial in-cell
/// copper already on the grid (marked `Net(conn)`) never blocks it, so the finisher
/// routes freely through where its dropped copper sat. On success the net's full
/// copper capsule is marked into the
/// grid (so later finishers keep clearance) and a single full-board [`CellRoute`] is
/// returned. The synthetic leaf id is [`usize::MAX`] — a finisher route spans the
/// board, not one leaf, and Task 3 stitches purely by net + endpoint identity.
#[allow(clippy::too_many_arguments)]
fn finish_net(
    conn: &crate::problem::Connection,
    waypoints: &[Waypoint],
    grid: &mut RouteGrid,
    layer_count: usize,
    halo: f64,
    via_halo: f64,
    costs: AStarCosts,
) -> Result<CellRoute, String> {
    let conn_idx = grid
        .connection_index(&conn.name)
        .ok_or_else(|| "connection has no grid index".to_string())?;

    let mut traces: Vec<CellTrace> = Vec::new();
    let mut vias: Vec<CellVia> = Vec::new();

    // Single- or zero-point nets are trivially connected: emit an empty route.
    if conn.points_to_connect.len() < 2 {
        return Ok(CellRoute {
            leaf: usize::MAX,
            connection: conn.name.clone(),
            traces,
            vias,
        });
    }

    // No endpoint snapping (the snap map stays empty): legs route to a target CELL
    // SET and continuity is carried by shared tree cells, so consecutive legs meet
    // without snapping, and a pad connects because the half-pitch cell centre sits
    // within half a trace width of the route point. Every emitted point therefore
    // stays cell-centre-aligned — which is also what the capsule clearance MARK
    // assumes (it dilates the cell-centre centreline). Snapping a pad endpoint off
    // its cell centre would read to the exact lint as a sub-clearance near-miss
    // against a neighbour's centre-aligned copper.
    let snap: BTreeMap<(usize, usize, usize), Point2> = BTreeMap::new();

    // Seed the tree with route point 0; mark its halo as this net's copper.
    let seed = route_point_cell(grid, &conn.points_to_connect[0], layer_count);
    let mut tree_cells: Vec<State> = vec![seed];
    grid.mark_net_halo_euclid(seed.layer, seed.ix, seed.iy, conn_idx, halo);

    // Margin (in mm, expressed in cells) by which a leg's search box is inflated
    // beyond the tree + target bounding box, so the route has room to detour around
    // obstacles without exploring the entire board. A net's pads + wall waypoint
    // already span its true corridor; a fixed several-mm margin gives detour room
    // while keeping the A* cost proportional to that corridor, not the whole board.
    // There is deliberately NO unconfined retry: a leg that cannot route within this
    // generous box is treated as a genuine failure (a wall a few mm of slack cannot
    // get around will not be gotten around by exploring distant board corners), and
    // the retry's full-board A* — re-run for every failing leg of every ordering —
    // was the finisher's dominant cost.
    let margin_cells = ((6.0 / grid.pitch).ceil() as usize).max(24);

    // A* one leg from the current tree to any cell in `targets`, marking copper +
    // emitting. The path may end at any target (the nearest reachable), so a guided
    // leg can take whichever free lane of a gap neighbourhood is open. The search is
    // BOUNDED to the inflated bounding box of the current tree and the targets.
    let mut route_leg = |grid: &mut RouteGrid,
                         tree_cells: &mut Vec<State>,
                         targets: &[State],
                         what: &str|
     -> Result<(), String> {
        let bounds = leg_bounds(grid, tree_cells, targets, margin_cells);
        let path = route_full_board_leg(grid, conn_idx, targets, tree_cells, costs, bounds)
            .ok_or_else(|| format!("no full-board path to {what}"))?;
        // Mark the leg's swept-clearance capsule (diagonal-safe) so a later finisher
        // net keeps full clearance from this run's body, then fold it into the tree.
        mark_path_capsule(grid, &path, conn_idx, halo);
        tree_cells.extend_from_slice(&path);
        emit_path(grid, &path, &snap, &mut traces, &mut vias);
        Ok(())
    };

    // **Guided legs (optional).** When `waypoints` is non-empty, route the net
    // through its globally-assigned crossing waypoints in plan order first: this
    // keeps the net in the lane the global stage negotiated (which wall gap, which
    // order through it) rather than letting a greedy pad-to-pad A* grab the nearest
    // gap. Each waypoint's target is a small CELL NEIGHBOURHOOD around the assigned
    // crossing (same layer, free cells within a couple of track pitches), so the leg
    // is pinned to the negotiated gap but free to take whichever lane in it is open
    // — the assigned slot itself may sit at the gap's blocked margin (slot spreading
    // can push the last slot to the edge), and an exact-point target would strand
    // the net there. Each leg is full-board, so it has room the per-cell pass lacked.
    let nbhd_cells = ((via_halo.max(halo) * 3.0) / grid.pitch).ceil() as isize;
    for w in waypoints {
        let (cx, cy) = grid.cell_of(w.at.x, w.at.y);
        let mut targets: Vec<State> = Vec::new();
        for dy in -nbhd_cells..=nbhd_cells {
            for dx in -nbhd_cells..=nbhd_cells {
                let (ix, iy) = (cx as isize + dx, cy as isize + dy);
                if ix < 0 || iy < 0 {
                    continue;
                }
                let (ix, iy) = (ix as usize, iy as usize);
                if grid.is_free_for(w.layer, ix, iy, conn_idx) {
                    targets.push(State {
                        layer: w.layer,
                        ix,
                        iy,
                    });
                }
            }
        }
        if targets.is_empty() {
            targets.push(State {
                layer: w.layer,
                ix: cx,
                iy: cy,
            });
        }
        route_leg(grid, &mut tree_cells, &targets, "an assigned crossing")?;
    }

    // Connect every remaining route point to the tree (the waypoints, if any, wove
    // the path through the gaps; the pads close it off). Grow toward the nearest
    // remaining pad each time instead of following input order: a repaired multi-pin
    // net should claim the smallest next branch first, and if that target is boxed in
    // by the current bounded corridor, another terminal may still be reachable.
    let mut remaining: Vec<usize> = (1..conn.points_to_connect.len()).collect();
    while !remaining.is_empty() {
        let mut ranked = remaining.clone();
        ranked.sort_by_key(|&idx| {
            let target = route_point_cell(grid, &conn.points_to_connect[idx], layer_count);
            let (layer_hops, distance) = terminal_tree_route_key(&tree_cells, target);
            (layer_hops, distance, idx)
        });
        let first_ranked = ranked[0];

        let mut routed = None;
        let mut first_err = None;
        for idx in ranked {
            let pt = &conn.points_to_connect[idx];
            let target = route_point_cell(grid, pt, layer_count);
            match route_leg(
                grid,
                &mut tree_cells,
                &[target],
                &format!("route point ({:.4},{:.4})", pt.x, pt.y),
            ) {
                Ok(()) => {
                    routed = Some(idx);
                    break;
                }
                Err(err) => {
                    if first_err.is_none() {
                        first_err = Some(err);
                    }
                }
            }
        }

        let Some(routed_idx) = routed else {
            let pt = &conn.points_to_connect[first_ranked];
            return Err(first_err.unwrap_or_else(|| {
                format!(
                    "no full-board path to route point ({:.4},{:.4})",
                    pt.x, pt.y
                )
            }));
        };
        remaining.retain(|&idx| idx != routed_idx);
    }

    // (The `route_leg` closure's mutable borrow of `traces`/`vias` ends at its last
    // call above, so the via list is readable here.)

    // Mark every via barrel this finisher produced on every layer.
    for v in &vias {
        let (vx, vy) = grid.cell_of(v.at.x, v.at.y);
        for l in 0..grid.layer_count {
            grid.mark_net_halo_euclid(l, vx, vy, conn_idx, via_halo);
        }
    }

    Ok(CellRoute {
        leaf: usize::MAX,
        connection: conn.name.clone(),
        traces,
        vias,
    })
}

/// The inclusive cell box covering `tree` ∪ `targets`, inflated by `margin` cells
/// and clamped to the grid — the bound for a finisher leg's A* so it explores the
/// net's own corridor, not the whole board.
fn leg_bounds(
    grid: &RouteGrid,
    tree: &[State],
    targets: &[State],
    margin: usize,
) -> astar::CellBounds {
    let mut ix0 = usize::MAX;
    let mut iy0 = usize::MAX;
    let mut ix1 = 0usize;
    let mut iy1 = 0usize;
    for s in tree.iter().chain(targets.iter()) {
        ix0 = ix0.min(s.ix);
        iy0 = iy0.min(s.iy);
        ix1 = ix1.max(s.ix);
        iy1 = iy1.max(s.iy);
    }
    if ix0 == usize::MAX {
        // Empty (defensive): the whole grid.
        return astar::CellBounds {
            ix0: 0,
            iy0: 0,
            ix1: grid.nx.saturating_sub(1),
            iy1: grid.ny.saturating_sub(1),
        };
    }
    astar::CellBounds {
        ix0: ix0.saturating_sub(margin),
        iy0: iy0.saturating_sub(margin),
        ix1: (ix1 + margin).min(grid.nx.saturating_sub(1)),
        iy1: (iy1 + margin).min(grid.ny.saturating_sub(1)),
    }
}

/// An ordered crossing waypoint a guided finisher leg must pass through (mm +
/// numeric copper layer), recovered from the global plan's assigned crossings.
struct Waypoint {
    at: Point2,
    layer: usize,
}

// ── per-job routing ──────────────────────────────────────────────────────────

/// Route one cell job into `wgrid`. The job's terminals are connected into a
/// single tree (terminal 0 seeds it; each further terminal is A*-routed to the
/// nearest tree cell, allowing vias). Routed copper + a clearance halo are marked
/// so later nets in the same cell avoid it. Returns the cell-local copper, or an
/// structured error (the caller adds the leaf id).
#[allow(clippy::too_many_arguments)]
fn route_one_job(
    problem: &RouteProblem,
    job: &CellJob,
    wgrid: &mut RouteGrid,
    window: &Rect,
    layer_count: usize,
    halo: f64,
    via_halo: f64,
    costs: AStarCosts,
) -> Result<CellRoute, DetailRouteError> {
    let conn_idx = wgrid
        .connection_index(&job.connection)
        .ok_or_else(|| DetailRouteError::new("connection has no grid index"))?;
    // Confine this job's A* to the leaf window (one-track-pitch inflated leaf).
    let bounds = window_cell_bounds(wgrid, window);

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
    let seed_idx = detail_seed_terminal_index(&terminals);
    if seed_idx != 0 {
        terminals.swap(0, seed_idx);
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
    let mut tree_cells: Vec<State> = Vec::new();
    append_terminal_tree_cells(&mut tree_cells, terminals[0], layer_count);
    // Exact mm snap position for each terminal cell, keyed by (layer, ix, iy):
    // when a routed path's endpoint sits on a terminal cell, replace the cell
    // centre with the terminal's exact mm coordinate.
    let mut snap: BTreeMap<(usize, usize, usize), Point2> = BTreeMap::new();
    for (t, s) in &terminals {
        snap.entry((s.layer, s.ix, s.iy)).or_insert_with(|| t.at);
        if t.kind == TerminalKind::Via {
            for layer in 0..layer_count {
                snap.entry((layer, s.ix, s.iy)).or_insert_with(|| t.at);
            }
        }
    }

    {
        let seed = terminals[0].1;
        if terminals[0].0.kind == TerminalKind::Via {
            for layer in 0..layer_count {
                wgrid.mark_net_halo_euclid(layer, seed.ix, seed.iy, conn_idx, halo);
            }
        } else {
            wgrid.mark_net_halo_euclid(seed.layer, seed.ix, seed.iy, conn_idx, halo);
        }
    }

    // Single-terminal job: nothing to connect, but record any pad/via the cell
    // owns. A lone via terminal still needs its site recorded so Task 3 can drop
    // the via even when no in-cell copper run reaches it on both layers. (The via
    // barrel halo is marked for every via at the end of the job.)
    for (t, _s) in &terminals {
        if t.kind == TerminalKind::Via {
            push_via(&mut vias, &t.at);
        }
    }
    let prefer_planar_leg = terminals
        .iter()
        .all(|(terminal, _)| terminal.kind == TerminalKind::Pad);

    let mut remaining: Vec<usize> = (1..terminals.len()).collect();
    while !remaining.is_empty() {
        // Keep the per-cell pass confined to the leaf window. A saturated tiny
        // leaf can still fail honestly; the bounded full-board finisher repairs
        // whole failed nets afterward without letting every cell job launch an
        // unbounded board-wide A* search.
        let mut ranked = remaining.clone();
        ranked.sort_by_key(|&idx| {
            let (_, state) = terminals[idx];
            let (layer_hops, distance) = terminal_tree_route_key(&tree_cells, state);
            (layer_hops, distance, idx)
        });
        let first_ranked = ranked[0];

        let mut routed = None;
        for idx in ranked {
            let (_, start) = terminals[idx];
            if let Some(path) = route_one_job_leg(
                wgrid,
                conn_idx,
                start,
                &tree_cells,
                costs,
                bounds,
                prefer_planar_leg,
            ) {
                routed = Some((idx, path));
                break;
            }
        }

        let (routed_idx, path) = routed.ok_or_else(|| {
            let (t, state) = terminals[first_ranked];
            let blockage = terminal_blockage(problem, wgrid, conn_idx, state, layer_count);
            let seed_blockage =
                terminal_blockage(problem, wgrid, conn_idx, terminals[0].1, layer_count);
            DetailRouteError {
                reason: format!(
                    "no in-cell path for terminal {:?} at ({:.4},{:.4}) (congestion or enclosure); {}; seed {}",
                    t.kind, t.at.x, t.at.y, blockage.summary, seed_blockage.summary
                ),
                foreign_owners: blockage
                    .foreign_owners
                    .iter()
                    .chain(seed_blockage.foreign_owners.iter())
                    .map(|&owner| connection_owner_label(problem, owner))
                    .collect(),
            }
        })?;
        remaining.retain(|&idx| idx != routed_idx);

        // Mark copper + clearance capsule (diagonal-safe) and fold the path into
        // the tree.
        mark_path_capsule(wgrid, &path, conn_idx, halo);
        tree_cells.extend_from_slice(&path);
        append_terminal_tree_cells(&mut tree_cells, terminals[routed_idx], layer_count);

        emit_path(wgrid, &path, &snap, &mut traces, &mut vias);
    }

    // Every via this job produced (assigned site or a layer change mid-path) is a
    // through-hole barrel: mark its full via halo on every layer so later foreign
    // copper keeps the via clearance away from the barrel.
    for v in &vias {
        let (vx, vy) = wgrid.cell_of(v.at.x, v.at.y);
        for l in 0..wgrid.layer_count {
            wgrid.mark_net_halo_euclid(l, vx, vy, conn_idx, via_halo);
        }
    }

    Ok(CellRoute {
        leaf: job.leaf,
        connection: job.connection.clone(),
        traces,
        vias,
    })
}

fn terminal_tree_route_key(tree_cells: &[State], terminal: State) -> (usize, u64) {
    tree_cells
        .iter()
        .map(|tree| {
            let layer_hops = terminal.layer.abs_diff(tree.layer);
            let dx = terminal.ix.abs_diff(tree.ix) as u64;
            let dy = terminal.iy.abs_diff(tree.iy) as u64;
            let diag = dx.min(dy);
            let straight = dx.max(dy) - diag;
            (layer_hops, diag * DIAG_COST as u64 + straight * 10)
        })
        .min()
        .unwrap_or((usize::MAX, u64::MAX))
}

#[derive(Debug)]
struct DetailRouteError {
    reason: String,
    foreign_owners: BTreeSet<String>,
}

impl DetailRouteError {
    fn new(reason: impl Into<String>) -> Self {
        DetailRouteError {
            reason: reason.into(),
            foreign_owners: BTreeSet::new(),
        }
    }
}

#[cfg(test)]
fn terminal_blockage_summary(
    problem: &RouteProblem,
    grid: &RouteGrid,
    conn_idx: usize,
    terminal: State,
    layer_count: usize,
) -> String {
    terminal_blockage(problem, grid, conn_idx, terminal, layer_count).summary
}

struct TerminalBlockage {
    summary: String,
    foreign_owners: BTreeSet<usize>,
}

fn terminal_blockage(
    problem: &RouteProblem,
    grid: &RouteGrid,
    conn_idx: usize,
    terminal: State,
    layer_count: usize,
) -> TerminalBlockage {
    let layer = terminal.layer.min(layer_count.saturating_sub(1));
    let cell = grid.cell(layer, terminal.ix, terminal.iy);
    let counts = terminal_neighborhood_counts(grid, conn_idx, layer, terminal.ix, terminal.iy, 1);
    let foreign_owners = format_foreign_owners(problem, &counts.foreign_owners);
    let summary = format!(
        "terminal_cell layer={} ix={} iy={} occ={} neighborhood(r=1 free={} own={} foreign={} blocked={} foreign_owners={})",
        layer_ref(layer, layer_count).0,
        terminal.ix,
        terminal.iy,
        occupancy_label(problem, cell, conn_idx),
        counts.free,
        counts.own,
        counts.foreign,
        counts.blocked,
        foreign_owners
    );
    TerminalBlockage {
        summary,
        foreign_owners: counts.foreign_owners,
    }
}

#[derive(Default)]
struct OccupancyCounts {
    free: usize,
    own: usize,
    foreign: usize,
    blocked: usize,
    foreign_owners: BTreeSet<usize>,
}

fn terminal_neighborhood_counts(
    grid: &RouteGrid,
    conn_idx: usize,
    layer: usize,
    ix: usize,
    iy: usize,
    radius: usize,
) -> OccupancyCounts {
    let mut counts = OccupancyCounts::default();
    let r = radius as isize;
    for dy in -r..=r {
        for dx in -r..=r {
            let x = ix as isize + dx;
            let y = iy as isize + dy;
            let cell = if x < 0 || y < 0 {
                Cell::BlockedAll
            } else {
                grid.cell(layer, x as usize, y as usize)
            };
            match cell {
                Cell::Free => counts.free += 1,
                Cell::Net(owner) if owner == conn_idx => counts.own += 1,
                Cell::Net(owner) => {
                    counts.foreign += 1;
                    counts.foreign_owners.insert(owner);
                }
                Cell::Shared(_) => counts.free += 1,
                Cell::BlockedAll => counts.blocked += 1,
            }
        }
    }
    counts
}

fn occupancy_label(problem: &RouteProblem, cell: Cell, conn_idx: usize) -> String {
    match cell {
        Cell::Free => "free".to_owned(),
        Cell::Net(owner) if owner == conn_idx => "own".to_owned(),
        Cell::Net(owner) => format!("foreign({})", connection_owner_label(problem, owner)),
        Cell::Shared(_) => "shared".to_owned(),
        Cell::BlockedAll => "blocked".to_owned(),
    }
}

fn format_foreign_owners(problem: &RouteProblem, owners: &BTreeSet<usize>) -> String {
    if owners.is_empty() {
        return "-".to_owned();
    }
    owners
        .iter()
        .map(|&owner| connection_owner_label(problem, owner))
        .collect::<Vec<_>>()
        .join("|")
}

fn connection_owner_label(problem: &RouteProblem, owner: usize) -> String {
    problem
        .connections
        .get(owner)
        .map(|conn| conn.name.clone())
        .unwrap_or_else(|| format!("#{owner}"))
}

fn detail_seed_terminal_index(terminals: &[(&Terminal, State)]) -> usize {
    if terminals.len() <= 2 {
        return 0;
    }
    terminals
        .iter()
        .enumerate()
        .map(|(idx, (terminal, state))| {
            let mut layer_hops = 0usize;
            let mut distance = 0u64;
            for (other_idx, (_, other)) in terminals.iter().enumerate() {
                if idx == other_idx {
                    continue;
                }
                let (hops, dist) = terminal_tree_route_key(&[*state], *other);
                layer_hops = layer_hops.saturating_add(hops);
                distance = distance.saturating_add(dist);
            }
            (
                layer_hops,
                distance,
                detail_seed_terminal_kind_rank(terminal.kind),
                idx,
            )
        })
        .min()
        .map(|(_, _, _, idx)| idx)
        .unwrap_or(0)
}

fn detail_seed_terminal_kind_rank(kind: TerminalKind) -> usize {
    match kind {
        TerminalKind::Via => 0,
        TerminalKind::Pad => 1,
        TerminalKind::Entry | TerminalKind::Exit => 2,
    }
}

fn append_terminal_tree_cells(
    tree_cells: &mut Vec<State>,
    terminal_state: (&Terminal, State),
    layer_count: usize,
) {
    let (terminal, state) = terminal_state;
    if terminal.kind == TerminalKind::Via {
        for layer in 0..layer_count {
            let expanded = State {
                layer,
                ix: state.ix,
                iy: state.iy,
            };
            if !tree_cells.contains(&expanded) {
                tree_cells.push(expanded);
            }
        }
    } else if !tree_cells.contains(&state) {
        tree_cells.push(state);
    }
}

#[cfg(test)]
fn seed_terminal_tree_cells(seed: (&Terminal, State), layer_count: usize) -> Vec<State> {
    let mut tree_cells = Vec::new();
    append_terminal_tree_cells(&mut tree_cells, seed, layer_count);
    tree_cells
}

#[cfg(test)]
fn terminal_tree_cells_after_connection(
    tree_cells: &[State],
    terminal_state: (&Terminal, State),
    layer_count: usize,
) -> Vec<State> {
    let mut out = tree_cells.to_vec();
    append_terminal_tree_cells(&mut out, terminal_state, layer_count);
    out
}

fn route_full_board_leg(
    grid: &RouteGrid,
    conn_idx: usize,
    starts: &[State],
    tree_cells: &[State],
    costs: AStarCosts,
    bounds: astar::CellBounds,
) -> Option<Vec<State>> {
    if costs.allow_via && any_same_layer_reachable(starts, tree_cells) {
        let planar_costs = AStarCosts {
            allow_via: false,
            ..costs
        };
        if let Some(path) = astar::search_bounded(
            grid,
            conn_idx,
            starts,
            tree_cells,
            planar_costs,
            Some(bounds),
        ) {
            return Some(path);
        }
    }
    astar::search_bounded(grid, conn_idx, starts, tree_cells, costs, Some(bounds))
}

fn route_one_job_leg(
    grid: &RouteGrid,
    conn_idx: usize,
    start: State,
    tree_cells: &[State],
    costs: AStarCosts,
    bounds: astar::CellBounds,
    prefer_planar: bool,
) -> Option<Vec<State>> {
    if prefer_planar && costs.allow_via && same_layer_reachable(start, tree_cells) {
        let planar_costs = AStarCosts {
            allow_via: false,
            ..costs
        };
        if let Some(path) = astar::search_bounded(
            grid,
            conn_idx,
            &[start],
            tree_cells,
            planar_costs,
            Some(bounds),
        ) {
            return Some(path);
        }
    }
    astar::search_bounded(grid, conn_idx, &[start], tree_cells, costs, Some(bounds))
}

fn any_same_layer_reachable(starts: &[State], targets: &[State]) -> bool {
    starts
        .iter()
        .any(|start| same_layer_reachable(*start, targets))
}

fn same_layer_reachable(start: State, targets: &[State]) -> bool {
    targets.iter().any(|target| start.layer == target.layer)
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
            *p
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
fn push_trace(traces: &mut Vec<CellTrace>, layer: usize, layer_count: usize, points: Vec<Point2>) {
    let simplified = geom::Polyline::new(points).simplify().into_points();
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
    vias.push(CellVia { at: *at });
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// The inclusive cell-index rectangle of the shared grid that covers `window`.
/// A job's A* is confined to this box so routes stay cell-local even though the
/// grid spans the whole board (shared occupancy). The box is inflated by one cell
/// each side so a terminal snapped onto the window edge — and the cell beyond it
/// the route legitimately needs to hug — stays reachable.
fn window_cell_bounds(grid: &RouteGrid, window: &Rect) -> astar::CellBounds {
    let (ix0, iy0) = grid.cell_of(window.min_x, window.min_y);
    let (ix1, iy1) = grid.cell_of(window.max_x, window.max_y);
    astar::CellBounds {
        ix0: ix0.saturating_sub(1),
        iy0: iy0.saturating_sub(1),
        ix1: (ix1 + 1).min(grid.nx.saturating_sub(1)),
        iy1: (iy1 + 1).min(grid.ny.saturating_sub(1)),
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

/// The grid cell + layer of a connection route point (the finisher's terminals).
fn route_point_cell(
    grid: &RouteGrid,
    pt: &crate::problem::RoutePoint,
    layer_count: usize,
) -> State {
    let layer = pt
        .layer
        .index(layer_count as u32)
        .unwrap_or(0)
        .min(layer_count as u32 - 1) as usize;
    let (ix, iy) = grid.cell_of(pt.x, pt.y);
    State { layer, ix, iy }
}

/// Connection name → slice-1 global net rank (ascending bounding-box
/// half-perimeter, ties by name). Lower rank routes first. Matches
/// [`crate::router`]'s `net_order` so the per-cell order is consistent with the
/// full-board router.
fn net_rank(problem: &RouteProblem) -> BTreeMap<String, usize> {
    let mut order: Vec<usize> = (0..problem.connections.len()).collect();
    order.sort_by(|&a, &b| {
        let ka = problem.connections[a].half_perimeter();
        let kb = problem.connections[b].half_perimeter();
        ka.partial_cmp(&kb)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                problem.connections[a]
                    .name
                    .cmp(&problem.connections[b].name)
            })
    });
    let mut rank: BTreeMap<String, usize> = BTreeMap::new();
    for (r, ci) in order.into_iter().enumerate() {
        rank.entry(problem.connections[ci].name.clone())
            .or_insert(r);
    }
    rank
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crossing::assign_crossings;
    use crate::pathing::global_route;
    use crate::problem::{Connection, Obstacle, Rect, RoutePoint};
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

    fn base(bounds: Rect, obstacles: Vec<Obstacle>, connections: Vec<Connection>) -> RouteProblem {
        RouteProblem {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles,
            connections,
            bounds,
            clearance: 0.2,
            via_diameter: 0.6,
            via_drill: 0.3,
            net_widths: Default::default(),
            outline: None,
            escape_layers: Default::default(),
            plane_nets: Default::default(),
        }
    }

    fn bounds(w: f64, h: f64) -> Rect {
        Rect {
            min_x: 0.0,
            max_x: w,
            min_y: 0.0,
            max_y: h,
        }
    }

    fn keepout(center: (f64, f64), w: f64, h: f64, layers: &[&str]) -> Obstacle {
        Obstacle {
            kind: "rect".to_owned(),
            layers: layers.iter().map(|l| LayerRef((*l).to_owned())).collect(),
            center: Point2 {
                x: center.0,
                y: center.1,
            },
            width: w,
            height: h,
            connected_to: vec![],
        }
    }

    fn run(p: &RouteProblem) -> (CapacityMesh, CellRouteResult) {
        let mesh = CapacityMesh::build(p);
        let plan = global_route(p).plan;
        let a = assign_crossings(p, &mesh, &plan);
        let r = route_cells(p, &mesh, &a);
        (mesh, r)
    }

    #[test]
    fn finisher_skips_large_multilayer_boards() {
        let mut p = base(bounds(30.0, 30.0), vec![], Vec::new());
        p.layer_count = 4;
        p.connections = (0..=FINISHER_MAX_MULTILAYER_CONNECTIONS)
            .map(|idx| {
                conn(
                    &format!("N{idx}"),
                    &[(1.0, 1.0, "top"), (29.0, 29.0, "top")],
                )
            })
            .collect();

        assert!(
            !should_try_finisher(&p),
            "large multilayer boards should report detailed cell failures instead of launching the expensive finisher portfolio"
        );
        p.layer_count = 2;
        assert!(
            should_try_finisher(&p),
            "two-layer boards keep the finisher regardless of connection count"
        );
    }

    #[test]
    fn finisher_skip_reason_reports_multilayer_cap() {
        let mut p = base(bounds(30.0, 30.0), vec![], Vec::new());
        p.layer_count = 4;
        p.connections = (0..=FINISHER_MAX_MULTILAYER_CONNECTIONS)
            .map(|idx| {
                conn(
                    &format!("N{idx}"),
                    &[(1.0, 1.0, "top"), (29.0, 29.0, "top")],
                )
            })
            .collect();

        let reason = finisher_skip_reason(&p);

        assert!(reason.contains("layers=4"), "{reason}");
        assert!(
            reason.contains(&format!(
                "connections={} > cap={}",
                FINISHER_MAX_MULTILAYER_CONNECTIONS + 1,
                FINISHER_MAX_MULTILAYER_CONNECTIONS
            )),
            "{reason}"
        );
    }

    #[test]
    fn finisher_attempts_skip_duplicate_guided_pass_without_waypoint() {
        let c = conn("N", &[(1.0, 1.0, "top"), (5.0, 1.0, "top")]);
        assert_eq!(
            finisher_attempts(&c, false),
            vec![FinishAttempt::Planar, FinishAttempt::Free]
        );
    }

    #[test]
    fn finisher_attempts_skip_impossible_no_via_for_layer_changing_net() {
        let c = conn("N", &[(1.0, 1.0, "top"), (5.0, 1.0, "bottom")]);
        assert_eq!(
            finisher_attempts(&c, true),
            vec![FinishAttempt::Free, FinishAttempt::Guided]
        );
    }

    #[test]
    fn finisher_orders_include_crossing_pressure_order() {
        let p = base(
            bounds(10.0, 10.0),
            vec![],
            vec![
                conn("VERT", &[(5.0, 1.0, "top"), (5.0, 9.0, "top")]),
                conn("H_HIGH", &[(1.0, 3.0, "top"), (9.0, 3.0, "top")]),
                conn("H_LOW", &[(1.0, 7.0, "top"), (9.0, 7.0, "top")]),
            ],
        );
        let names = ["VERT", "H_HIGH", "H_LOW"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let orders = finisher_orders(&p, &names, &net_rank(&p));

        assert!(
            orders
                .iter()
                .any(|order| order
                    == &vec!["VERT".to_owned(), "H_HIGH".to_owned(), "H_LOW".to_owned()]),
            "the finisher should try a crossing-pressure order for failed hotspot nets: {orders:?}"
        );
    }

    #[test]
    fn finisher_orders_include_segment_obstacle_pressure_order() {
        let p = base(
            bounds(20.0, 20.0),
            vec![
                keepout((8.0, 10.0), 1.0, 1.0, &["top"]),
                keepout((8.0, 4.0), 1.0, 1.0, &["top"]),
            ],
            vec![
                conn("OPEN", &[(2.0, 2.0, "top"), (6.0, 2.0, "top")]),
                conn("SEG_PINCHED", &[(2.0, 10.0, "top"), (12.0, 10.0, "top")]),
                conn(
                    "BBOX_ONLY",
                    &[(2.0, 4.0, "top"), (2.0, 14.0, "top"), (12.0, 14.0, "top")],
                ),
            ],
        );
        let names = ["OPEN", "SEG_PINCHED", "BBOX_ONLY"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let metrics = finisher_order_metrics(&p, &names);
        let mut segment_order = vec![
            "OPEN".to_owned(),
            "SEG_PINCHED".to_owned(),
            "BBOX_ONLY".to_owned(),
        ];
        segment_order.sort_by(|a, b| {
            let ma = finisher_metric(&metrics, a);
            let mb = finisher_metric(&metrics, b);
            mb.segment_obstacle_pressure_um
                .cmp(&ma.segment_obstacle_pressure_um)
                .then_with(|| mb.obstacle_pressure_um.cmp(&ma.obstacle_pressure_um))
                .then_with(|| mb.crossing_pressure.cmp(&ma.crossing_pressure))
                .then_with(|| mb.span_um.cmp(&ma.span_um))
                .then_with(|| mb.pin_count.cmp(&ma.pin_count))
                .then_with(|| a.cmp(b))
        });

        let orders = finisher_orders(&p, &names, &net_rank(&p));

        assert_eq!(segment_order[0], "SEG_PINCHED");
        assert_eq!(
            finisher_metric(&metrics, "BBOX_ONLY").segment_obstacle_pressure_um,
            0,
            "bbox-only keepouts away from tree segments should not count as segment pressure"
        );
        assert!(
            orders.iter().any(|order| order == &segment_order),
            "the finisher should try segment-obstacle order for corridor-pinched failed nets: {orders:?}"
        );
    }

    #[test]
    fn finisher_candidate_key_uses_failed_pad_weight_after_count() {
        let p = base(
            bounds(10.0, 10.0),
            vec![],
            vec![
                conn("SIG", &[(1.0, 1.0, "top"), (2.0, 1.0, "top")]),
                conn(
                    "BUS",
                    &[(1.0, 2.0, "top"), (2.0, 2.0, "top"), (3.0, 2.0, "top")],
                ),
            ],
        );
        let failed = |name: &str| {
            vec![FailedNet {
                connection: name.to_owned(),
                reason: "test".to_owned(),
            }]
        };

        assert!(
            finisher_candidate_key(&p, &[], &failed("SIG"))
                < finisher_candidate_key(&p, &[], &failed("BUS")),
            "at equal failure count, fewer failed pads should win"
        );
    }

    #[test]
    fn finisher_candidate_key_tiebreaks_by_vias_then_wirelength() {
        let p = base(bounds(20.0, 20.0), vec![], vec![]);
        let route = |vias: usize, points: Vec<Point2>| CellRoute {
            leaf: usize::MAX,
            connection: "N".to_owned(),
            traces: vec![CellTrace {
                layer: LayerRef::top(),
                points,
            }],
            vias: (0..vias)
                .map(|i| CellVia {
                    at: Point2 {
                        x: i as f64,
                        y: 0.0,
                    },
                })
                .collect(),
        };
        let long_no_via = vec![route(
            0,
            vec![Point2 { x: 0.0, y: 0.0 }, Point2 { x: 10.0, y: 0.0 }],
        )];
        let short_with_via = vec![route(
            1,
            vec![Point2 { x: 0.0, y: 0.0 }, Point2 { x: 1.0, y: 0.0 }],
        )];
        let short_no_via = vec![route(
            0,
            vec![Point2 { x: 0.0, y: 0.0 }, Point2 { x: 1.0, y: 0.0 }],
        )];

        assert!(
            finisher_candidate_key(&p, &long_no_via, &[])
                < finisher_candidate_key(&p, &short_with_via, &[]),
            "fewer vias should beat shorter wire when routability ties"
        );
        assert!(
            finisher_candidate_key(&p, &short_no_via, &[])
                < finisher_candidate_key(&p, &long_no_via, &[]),
            "with equal via count, shorter repair wire should win"
        );
    }

    #[test]
    fn finisher_candidate_selection_checks_later_zero_via_wirelength() {
        let incumbent = FinisherCandidateKey {
            fail_count: 0,
            failed_pad_weight: 0,
            via_count: 0,
            wirelength_um: 10_000,
        };
        let shorter_later = FinisherCandidateKey {
            wirelength_um: 5_000,
            ..incumbent
        };
        let exact_tie_later = incumbent;

        assert!(
            keep_finisher_candidate(shorter_later, 3, incumbent, 0),
            "a later all-routed zero-via pass should still win if it emits shorter copper"
        );
        assert!(
            !keep_finisher_candidate(exact_tie_later, 3, incumbent, 0),
            "exact ties should keep the earlier deterministic order"
        );
    }

    fn assignment_for_connections(problem: &RouteProblem) -> CrossingAssignment {
        CrossingAssignment {
            jobs: problem
                .connections
                .iter()
                .map(|conn| CellJob {
                    leaf: 0,
                    connection: conn.name.clone(),
                    terminals: Vec::new(),
                })
                .collect(),
            crossings: Vec::new(),
            failures: Vec::new(),
        }
    }

    fn names_for_job_order(assignment: &CrossingAssignment, order: &[usize]) -> Vec<String> {
        order
            .iter()
            .map(|&idx| assignment.jobs[idx].connection.clone())
            .collect()
    }

    #[test]
    fn detail_ranked_order_is_cheap_and_retry_orders_include_crossing_pressure() {
        let p = base(
            bounds(10.0, 10.0),
            vec![],
            vec![
                conn("VERT", &[(5.0, 1.0, "top"), (5.0, 9.0, "top")]),
                conn("H_HIGH", &[(1.0, 3.0, "top"), (9.0, 3.0, "top")]),
                conn("H_LOW", &[(1.0, 7.0, "top"), (9.0, 7.0, "top")]),
            ],
        );
        let assignment = assignment_for_connections(&p);
        let rank = net_rank(&p);
        let baseline = detail_ranked_job_order(&assignment, &rank);
        let retry_orders = detail_retry_job_orders(&p, &assignment, &rank);

        assert_eq!(
            names_for_job_order(&assignment, &baseline),
            vec!["H_HIGH".to_owned(), "H_LOW".to_owned(), "VERT".to_owned()],
            "clean detailed boards should only pay for the ranked shortest/name order"
        );
        assert_eq!(
            names_for_job_order(&assignment, &retry_orders[0]),
            names_for_job_order(&assignment, &baseline),
            "retry portfolio must keep the ranked baseline order as the exact-tie winner"
        );
        assert!(
            retry_orders
                .iter()
                .any(|order| names_for_job_order(&assignment, order)
                    == vec!["VERT".to_owned(), "H_HIGH".to_owned(), "H_LOW".to_owned()]),
            "failed detailed passes should be able to retry high-crossing nets first: {retry_orders:?}"
        );
    }

    #[test]
    fn detail_blocker_retry_order_routes_blocked_net_before_owner() {
        let p = base(
            bounds(10.0, 10.0),
            vec![],
            vec![
                conn("A", &[(1.0, 1.0, "top"), (9.0, 1.0, "top")]),
                conn("B", &[(1.0, 2.0, "top"), (9.0, 2.0, "top")]),
                conn("C", &[(1.0, 3.0, "top"), (9.0, 3.0, "top")]),
            ],
        );
        let assignment = assignment_for_connections(&p);
        let order = detail_blocker_retry_net_order(
            &assignment,
            &net_rank(&p),
            &[DetailBlockerEdge {
                blocked: "C".to_owned(),
                blocker: "A".to_owned(),
            }],
        );

        let c_pos = order.iter().position(|name| name == "C").unwrap();
        let a_pos = order.iter().position(|name| name == "A").unwrap();
        assert!(
            c_pos < a_pos,
            "a net blocked by foreign owner A should get a retry before A claims the corridor: {order:?}"
        );
    }

    #[test]
    fn detail_pass_candidate_selection_prefers_lower_failed_pad_weight() {
        let p = base(
            bounds(10.0, 10.0),
            vec![],
            vec![
                conn("SIG", &[(1.0, 1.0, "top"), (2.0, 1.0, "top")]),
                conn(
                    "BUS",
                    &[(1.0, 2.0, "top"), (2.0, 2.0, "top"), (3.0, 2.0, "top")],
                ),
            ],
        );
        let failed = |name: &str| {
            vec![FailedNet {
                connection: name.to_owned(),
                reason: "test".to_owned(),
            }]
        };
        let bus_failed = cell_route_candidate_key(&p, &[], &failed("BUS"));
        let sig_failed = cell_route_candidate_key(&p, &[], &failed("SIG"));

        assert!(
            keep_cell_route_candidate(sig_failed, 1, bus_failed, 0),
            "a later detailed pass that leaves fewer failed pads should beat the first pass"
        );
        assert!(
            !keep_cell_route_candidate(bus_failed, 1, bus_failed, 0),
            "exact ties should keep the first detailed pass"
        );
    }

    #[test]
    fn detail_pass_candidate_selection_rejects_geometry_regression() {
        let p = base(
            bounds(10.0, 10.0),
            vec![],
            vec![
                conn("A", &[(1.0, 1.0, "top"), (9.0, 1.0, "top")]),
                conn("B", &[(1.0, 1.2, "top"), (9.0, 1.2, "top")]),
            ],
        );
        let geometry_bad_routes = vec![
            CellRoute {
                leaf: 0,
                connection: "A".to_owned(),
                traces: vec![CellTrace {
                    layer: LayerRef::top(),
                    points: vec![Point2 { x: 1.0, y: 1.0 }, Point2 { x: 9.0, y: 1.0 }],
                }],
                vias: Vec::new(),
            },
            CellRoute {
                leaf: 0,
                connection: "B".to_owned(),
                traces: vec![CellTrace {
                    layer: LayerRef::top(),
                    points: vec![Point2 { x: 1.0, y: 1.2 }, Point2 { x: 9.0, y: 1.2 }],
                }],
                vias: Vec::new(),
            },
        ];
        let clean_with_failure = vec![FailedNet {
            connection: "B".to_owned(),
            reason: "test".to_owned(),
        }];
        let geometry_bad = cell_route_candidate_key(&p, &geometry_bad_routes, &[]);
        let clean = cell_route_candidate_key(&p, &[], &clean_with_failure);

        assert!(
            !keep_cell_route_candidate(geometry_bad, 1, clean, 0),
            "a retry candidate must not trade DRC geometry faults for fewer failed nets"
        );
    }

    #[test]
    fn failed_detail_job_does_not_pollute_later_jobs() {
        let mut p = base(
            bounds(10.0, 10.0),
            vec![keepout((8.0, 5.0), 1.0, 1.0, &["top"])],
            (0..9)
                .map(|idx| conn(&format!("D{idx}"), &[(5.0, 5.0, "top")]))
                .chain(vec![
                    conn("A", &[(2.0, 5.0, "top"), (8.0, 5.0, "top")]),
                    conn("B", &[(1.0, 5.0, "top"), (3.0, 5.0, "top")]),
                ])
                .collect(),
        );
        p.layer_count = 4;
        assert!(
            !should_try_finisher(&p),
            "the rollback path is scoped to large multilayer boards"
        );
        let mesh = CapacityMesh::build(&p);
        let leaf = mesh.cell_at(&Point2 { x: 2.0, y: 5.0 });
        let assignment = CrossingAssignment {
            jobs: vec![
                CellJob {
                    leaf,
                    connection: "A".to_owned(),
                    terminals: vec![
                        Terminal {
                            kind: TerminalKind::Entry,
                            at: Point2 { x: 2.0, y: 5.0 },
                            layer: LayerRef::top(),
                        },
                        Terminal {
                            kind: TerminalKind::Exit,
                            at: Point2 { x: 8.0, y: 5.0 },
                            layer: LayerRef::top(),
                        },
                    ],
                },
                CellJob {
                    leaf,
                    connection: "B".to_owned(),
                    terminals: vec![
                        Terminal {
                            kind: TerminalKind::Entry,
                            at: Point2 { x: 1.0, y: 5.0 },
                            layer: LayerRef::top(),
                        },
                        Terminal {
                            kind: TerminalKind::Exit,
                            at: Point2 { x: 3.0, y: 5.0 },
                            layer: LayerRef::top(),
                        },
                    ],
                },
            ],
            crossings: Vec::new(),
            failures: Vec::new(),
        };
        let costs = AStarCosts {
            diag: DIAG_COST,
            allow_via: false,
            ..AStarCosts::default()
        };

        let (routes, failed, _) = route_detail_pass(
            &p,
            &mesh,
            &assignment,
            &[0, 1],
            grid::grid_pitch(&p),
            p.layer_count as usize,
            p.min_trace_width + p.clearance,
            p.min_trace_width + p.clearance,
            p.via_diameter / 2.0 + p.clearance + p.min_trace_width / 2.0,
            costs,
        );

        assert!(
            failed.iter().any(|f| f.connection == "A"),
            "the first job should fail against the keepout target: {failed:?}"
        );
        assert!(
            !failed.iter().any(|f| f.connection == "B"),
            "the later job should not be blocked by A's rolled-back seed: {failed:?}"
        );
        assert!(
            routes.iter().any(|r| r.connection == "B"),
            "B should commit a route after A fails transactionally"
        );
    }

    #[test]
    fn failed_detail_net_discards_earlier_successful_cell_routes() {
        let mut p = base(
            bounds(10.0, 10.0),
            vec![keepout((8.0, 5.0), 1.0, 1.0, &["top"])],
            (0..10)
                .map(|idx| conn(&format!("D{idx}"), &[(5.0, 5.0, "top")]))
                .chain(vec![conn(
                    "A",
                    &[
                        (1.0, 1.0, "top"),
                        (2.0, 1.0, "top"),
                        (7.0, 5.0, "top"),
                        (8.0, 5.0, "top"),
                    ],
                )])
                .collect(),
        );
        p.layer_count = 4;
        assert!(!should_try_finisher(&p));
        let mesh = CapacityMesh::build(&p);
        let assignment = CrossingAssignment {
            jobs: vec![
                CellJob {
                    leaf: mesh.cell_at(&Point2 { x: 1.0, y: 1.0 }),
                    connection: "A".to_owned(),
                    terminals: vec![
                        Terminal {
                            kind: TerminalKind::Entry,
                            at: Point2 { x: 1.0, y: 1.0 },
                            layer: LayerRef::top(),
                        },
                        Terminal {
                            kind: TerminalKind::Exit,
                            at: Point2 { x: 2.0, y: 1.0 },
                            layer: LayerRef::top(),
                        },
                    ],
                },
                CellJob {
                    leaf: mesh.cell_at(&Point2 { x: 7.0, y: 5.0 }),
                    connection: "A".to_owned(),
                    terminals: vec![
                        Terminal {
                            kind: TerminalKind::Entry,
                            at: Point2 { x: 7.0, y: 5.0 },
                            layer: LayerRef::top(),
                        },
                        Terminal {
                            kind: TerminalKind::Exit,
                            at: Point2 { x: 8.0, y: 5.0 },
                            layer: LayerRef::top(),
                        },
                    ],
                },
            ],
            crossings: Vec::new(),
            failures: Vec::new(),
        };
        let costs = AStarCosts {
            diag: DIAG_COST,
            allow_via: false,
            ..AStarCosts::default()
        };

        let (routes, failed, _) = route_detail_pass(
            &p,
            &mesh,
            &assignment,
            &[0, 1],
            grid::grid_pitch(&p),
            p.layer_count as usize,
            p.min_trace_width + p.clearance,
            p.min_trace_width + p.clearance,
            p.via_diameter / 2.0 + p.clearance + p.min_trace_width / 2.0,
            costs,
        );

        assert!(
            failed.iter().any(|f| f.connection == "A"),
            "the later A job should fail against the keepout target: {failed:?}"
        );
        assert!(
            !routes.iter().any(|route| route.connection == "A"),
            "a failed no-finisher net must not leave earlier successful cell routes committed: {routes:?}"
        );
    }

    #[test]
    fn route_one_job_connects_nearest_remaining_terminal_first() {
        let p = base(
            bounds(20.0, 20.0),
            vec![],
            vec![conn(
                "N",
                &[(5.5, 5.5, "top"), (14.5, 14.5, "top"), (6.5, 5.5, "top")],
            )],
        );
        let mut grid = RouteGrid::build_with_pitch(&p, 1.0);
        let job = CellJob {
            leaf: 0,
            connection: "N".to_owned(),
            terminals: vec![
                Terminal {
                    kind: TerminalKind::Pad,
                    at: Point2 { x: 5.5, y: 5.5 },
                    layer: LayerRef::top(),
                },
                Terminal {
                    kind: TerminalKind::Exit,
                    at: Point2 { x: 14.5, y: 14.5 },
                    layer: LayerRef::top(),
                },
                Terminal {
                    kind: TerminalKind::Pad,
                    at: Point2 { x: 6.5, y: 5.5 },
                    layer: LayerRef::top(),
                },
            ],
        };

        let route = route_one_job(
            &p,
            &job,
            &mut grid,
            &p.bounds,
            p.layer_count as usize,
            p.min_trace_width + p.clearance,
            p.via_diameter / 2.0 + p.clearance + p.min_trace_width / 2.0,
            AStarCosts {
                diag: DIAG_COST,
                ..AStarCosts::default()
            },
        )
        .expect("open cell should route");

        let first = route
            .traces
            .first()
            .expect("nearest terminal should emit a trace");
        assert!(
            first.points.iter().any(|p| p.x == 6.5 && p.y == 5.5),
            "first detailed leg should connect the near terminal before the insertion-order far terminal: {:?}",
            route.traces
        );
        assert!(
            !first.points.iter().any(|p| p.x == 14.5 && p.y == 14.5),
            "far terminal should no longer be the first routed leg: {:?}",
            route.traces
        );
    }

    #[test]
    fn detail_seed_prefers_central_via_for_dense_multiterminal_job() {
        let terminals = [
            Terminal {
                kind: TerminalKind::Entry,
                at: Point2 { x: 10.0, y: 10.0 },
                layer: LayerRef("inner1".to_owned()),
            },
            Terminal {
                kind: TerminalKind::Via,
                at: Point2 { x: 10.0, y: 10.0 },
                layer: LayerRef("inner1".to_owned()),
            },
            Terminal {
                kind: TerminalKind::Pad,
                at: Point2 { x: 10.0, y: 11.0 },
                layer: LayerRef::top(),
            },
            Terminal {
                kind: TerminalKind::Exit,
                at: Point2 { x: 18.0, y: 18.0 },
                layer: LayerRef("inner1".to_owned()),
            },
        ];
        let states = [
            State {
                layer: 1,
                ix: 10,
                iy: 10,
            },
            State {
                layer: 1,
                ix: 10,
                iy: 10,
            },
            State {
                layer: 0,
                ix: 10,
                iy: 11,
            },
            State {
                layer: 1,
                ix: 18,
                iy: 18,
            },
        ];
        let pairs = terminals.iter().zip(states).collect::<Vec<_>>();

        assert_eq!(
            detail_seed_terminal_index(&pairs),
            1,
            "central same-cell via should seed a dense detailed job before the arbitrary entry"
        );
    }

    #[test]
    fn via_seed_initializes_tree_on_every_layer() {
        let terminal = Terminal {
            kind: TerminalKind::Via,
            at: Point2 { x: 10.0, y: 10.0 },
            layer: LayerRef("inner1".to_owned()),
        };
        let cells = seed_terminal_tree_cells(
            (
                &terminal,
                State {
                    layer: 1,
                    ix: 10,
                    iy: 12,
                },
            ),
            4,
        );

        assert_eq!(
            cells,
            vec![
                State {
                    layer: 0,
                    ix: 10,
                    iy: 12
                },
                State {
                    layer: 1,
                    ix: 10,
                    iy: 12
                },
                State {
                    layer: 2,
                    ix: 10,
                    iy: 12
                },
                State {
                    layer: 3,
                    ix: 10,
                    iy: 12
                },
            ],
            "a through via terminal should be reachable as the tree root on every copper layer"
        );
    }

    #[test]
    fn connected_via_expands_existing_tree_to_every_layer() {
        let terminal = Terminal {
            kind: TerminalKind::Via,
            at: Point2 { x: 10.0, y: 10.0 },
            layer: LayerRef("inner1".to_owned()),
        };
        let existing = vec![State {
            layer: 0,
            ix: 4,
            iy: 5,
        }];
        let cells = terminal_tree_cells_after_connection(
            &existing,
            (
                &terminal,
                State {
                    layer: 1,
                    ix: 10,
                    iy: 12,
                },
            ),
            4,
        );

        assert!(cells.contains(&existing[0]));
        for layer in 0..4 {
            assert!(
                cells.contains(&State {
                    layer,
                    ix: 10,
                    iy: 12
                }),
                "connected through-via terminal should expand the existing tree to layer {layer}: {cells:?}"
            );
        }
    }

    #[test]
    fn terminal_blockage_summary_reports_cell_occupancy() {
        let p = base(
            bounds(12.0, 12.0),
            vec![keepout((6.5, 6.5), 1.0, 1.0, &["top"])],
            vec![
                conn("A", &[(2.0, 2.0, "top")]),
                conn("B", &[(9.0, 9.0, "top")]),
            ],
        );
        let mut grid = RouteGrid::build_with_pitch(&p, 1.0);
        let conn_idx = grid.connection_index("A").unwrap();
        let (ix, iy) = grid.cell_of(6.5, 6.5);

        let summary = terminal_blockage_summary(
            &p,
            &grid,
            conn_idx,
            State { layer: 0, ix, iy },
            p.layer_count as usize,
        );

        assert!(summary.contains("terminal_cell layer=top"), "{summary}");
        assert!(summary.contains("occ=blocked"), "{summary}");
        assert!(summary.contains("neighborhood(r=1"), "{summary}");

        let foreign_idx = grid.connection_index("B").unwrap();
        let (fx, fy) = grid.cell_of(7.5, 7.5);
        grid.mark_net_halo_euclid(0, fx, fy, foreign_idx, 0.0);

        let summary = terminal_blockage_summary(
            &p,
            &grid,
            conn_idx,
            State {
                layer: 0,
                ix: fx,
                iy: fy,
            },
            p.layer_count as usize,
        );

        assert!(summary.contains("occ=foreign(B)"), "{summary}");
        assert!(summary.contains("foreign_owners=B"), "{summary}");
    }

    #[test]
    fn route_one_job_tries_planar_before_cheap_via_hop() {
        let p = base(
            bounds(30.0, 20.0),
            vec![keepout((15.5, 10.5), 1.0, 8.0, &["top"])],
            vec![conn("N", &[(2.5, 10.5, "top"), (28.5, 10.5, "top")])],
        );
        let mut grid = RouteGrid::build_with_pitch(&p, 1.0);
        let job = CellJob {
            leaf: 0,
            connection: "N".to_owned(),
            terminals: vec![
                Terminal {
                    kind: TerminalKind::Pad,
                    at: Point2 { x: 2.5, y: 10.5 },
                    layer: LayerRef::top(),
                },
                Terminal {
                    kind: TerminalKind::Pad,
                    at: Point2 { x: 28.5, y: 10.5 },
                    layer: LayerRef::top(),
                },
            ],
        };

        let route = route_one_job(
            &p,
            &job,
            &mut grid,
            &p.bounds,
            p.layer_count as usize,
            p.min_trace_width + p.clearance,
            p.via_diameter / 2.0 + p.clearance + p.min_trace_width / 2.0,
            AStarCosts {
                via: 1,
                diag: DIAG_COST,
                ..AStarCosts::default()
            },
        )
        .expect("same-layer detailed leg should route around the keepout");

        assert!(
            route.vias.is_empty(),
            "same-layer detailed leg should take the planar detour before considering a cheap via hop: {:?}",
            route.vias
        );
        assert!(
            route
                .traces
                .iter()
                .all(|trace| trace.layer == LayerRef::top()),
            "planar detailed detour should stay on the terminal layer: {:?}",
            route.traces
        );
    }

    #[test]
    fn finish_net_connects_nearest_remaining_route_point_first() {
        let p = base(
            bounds(20.0, 20.0),
            vec![],
            vec![conn(
                "N",
                &[(5.5, 5.5, "top"), (14.5, 14.5, "top"), (6.5, 5.5, "top")],
            )],
        );
        let mut grid = RouteGrid::build_with_pitch(&p, 1.0);
        let route = finish_net(
            &p.connections[0],
            &[],
            &mut grid,
            p.layer_count as usize,
            p.min_trace_width + p.clearance,
            p.via_diameter / 2.0 + p.clearance + p.min_trace_width / 2.0,
            AStarCosts {
                diag: DIAG_COST,
                allow_via: false,
                ..AStarCosts::default()
            },
        )
        .expect("open-board finisher route should succeed");

        let first = route
            .traces
            .first()
            .expect("nearest route point should emit a trace");
        assert!(
            first.points.iter().any(|p| p.x == 6.5 && p.y == 5.5),
            "first finisher leg should connect the near route point before the input-order far point: {:?}",
            route.traces
        );
        assert!(
            !first.points.iter().any(|p| p.x == 14.5 && p.y == 14.5),
            "far route point should not be the first finisher leg: {:?}",
            route.traces
        );
    }

    #[test]
    fn finish_net_tries_planar_before_cheap_via_hop() {
        let p = base(
            bounds(30.0, 20.0),
            vec![keepout((15.5, 10.5), 1.0, 8.0, &["top"])],
            vec![conn("N", &[(2.5, 10.5, "top"), (28.5, 10.5, "top")])],
        );
        let mut grid = RouteGrid::build_with_pitch(&p, 1.0);

        let route = finish_net(
            &p.connections[0],
            &[],
            &mut grid,
            p.layer_count as usize,
            p.min_trace_width + p.clearance,
            p.via_diameter / 2.0 + p.clearance + p.min_trace_width / 2.0,
            AStarCosts {
                via: 1,
                diag: DIAG_COST,
                ..AStarCosts::default()
            },
        )
        .expect("same-layer finisher leg should route around the keepout");

        assert!(
            route.vias.is_empty(),
            "same-layer finisher leg should take the planar detour before considering a cheap via hop: {:?}",
            route.vias
        );
        assert!(
            route
                .traces
                .iter()
                .all(|trace| trace.layer == LayerRef::top()),
            "planar finisher detour should stay on the terminal layer: {:?}",
            route.traces
        );
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
            ((q.x - 2.0).abs() < 1e-12 || (q.x - 18.0).abs() < 1e-12) && (q.y - 4.0).abs() < 1e-12
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
            connections.push(conn(&format!("N{i}"), &[(3.0, y, "top"), (13.0, y, "top")]));
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

    /// The hotspot-repair finisher routes OCTILINEARLY: it builds `finish_costs`
    /// with `diag = DIAG_COST`, guarded by a `debug_assert` this (debug build) test
    /// makes live. `congested` exercises the finisher (its per-cell pass leaves
    /// failures the finisher repairs), so a finisher that drifted back to the
    /// `u32::MAX` orthogonal sentinel would panic here. We also assert the run
    /// completes sanely — the finisher ran without tripping its own invariant.
    #[test]
    fn finisher_routes_octilinearly() {
        let p = load("congested.json");
        let r = crate::pipeline::route_detailed(&p);
        assert!(
            r.failed.len() <= p.connections.len(),
            "finisher produced a sane result"
        );
    }

    /// The win: the finisher actually EMITS diagonal (45°) segments now, and its
    /// diagonal copper is DRC-clean. `quad` over-converges six central crossings the
    /// per-cell pass cannot complete; the full-board finisher (leaf `usize::MAX`)
    /// repairs them — octilinearly. We assert at least one finisher trace segment is a
    /// true 45° run (|dx| ≈ |dy| > 0), and that the stitched solution lints CLEAN — so
    /// the diagonal swept-clearance capsule held (no trace/trace clearance fault).
    #[test]
    fn finisher_emits_clean_diagonals() {
        let p = load("quad.json");
        let mesh = CapacityMesh::build(&p);
        let plan = global_route(&p).plan;
        let a = assign_crossings(&p, &mesh, &plan);
        let r = route_cells(&p, &mesh, &a);
        assert!(
            r.is_clean(),
            "quad must route clean through the finisher: {:?}",
            r.failed
        );
        // Finisher routes carry the synthetic leaf id usize::MAX.
        let finisher: Vec<&CellRoute> = r
            .cell_routes
            .iter()
            .filter(|cr| cr.leaf == usize::MAX)
            .collect();
        assert!(
            !finisher.is_empty(),
            "the finisher must have repaired at least one net"
        );
        let has_45 = finisher.iter().any(|cr| {
            cr.traces.iter().any(|t| {
                t.points.windows(2).any(|w| {
                    let dx = (w[1].x - w[0].x).abs();
                    let dy = (w[1].y - w[0].y).abs();
                    dx > 1e-9 && dy > 1e-9 && (dx - dy).abs() < 1e-6
                })
            })
        });
        assert!(
            has_45,
            "the finisher must emit a 45° diagonal segment (the win)"
        );
        // The whole detailed solution (per-cell + finisher copper) lints CLEAN: the
        // capsule MARK kept the diagonal finisher runs the full clearance apart.
        let r = crate::pipeline::route_detailed(&p);
        let vs = crate::lint::lint(&p, &r.solution);
        assert!(
            vs.is_empty(),
            "finisher diagonals must lint clean, got {vs:?}"
        );
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

    #[test]
    fn route_cells_with_diagnostics_matches_plain_result() {
        let p = load("quad.json");
        let mesh = CapacityMesh::build(&p);
        let plan = global_route(&p).plan;
        let a = assign_crossings(&p, &mesh, &plan);

        let plain = route_cells(&p, &mesh, &a);
        let (diagnosed, diagnostics) = route_cells_with_diagnostics(&p, &mesh, &a);

        assert_eq!(diagnosed, plain);
        assert!(
            diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.selected)
                .count()
                == 1,
            "diagnostics should mark exactly one retained detailed pass: {diagnostics:?}"
        );
        assert_eq!(
            diagnostics.first().map(|diagnostic| diagnostic.index),
            Some(0),
            "the ranked baseline should always be diagnostic candidate zero"
        );
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
        assert!(
            r.is_clean(),
            "led-r must route cell-by-cell: {:?}",
            r.failed
        );
        for x in &a.crossings {
            let appears = r
                .cell_routes
                .iter()
                .filter(|cr| cr.connection == x.connection)
                .flat_map(|cr| &cr.traces)
                .any(|t| {
                    let f = &t.points[0];
                    let l = &t.points[t.points.len() - 1];
                    let eq =
                        |q: &Point2| (q.x - x.at.x).abs() < 1e-12 && (q.y - x.at.y).abs() < 1e-12;
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
        // The detailed router routes on a finer lattice than the design pitch
        // (`grid_pitch/2`), so a *non*-terminal interior point of a multi-crossing
        // net can sit within a fraction of the design pitch of *another* of the
        // net's crossings without being that crossing's snapped endpoint. The
        // snap-exactness invariant is local: an endpoint that is its OWN crossing's
        // terminal is byte-exact. Use a "near" threshold well under the detailed
        // pitch so only an endpoint genuinely snapped to a crossing is asserted
        // exact (a quarter of the detailed pitch — below the cell-centre spacing, so
        // a foreign interior cell-centre never trips it).
        let pitch = grid::grid_pitch(&p) / 2.0;
        let near = pitch / 4.0;
        for cr in &r.cell_routes {
            for t in &cr.traces {
                for end in [&t.points[0], &t.points[t.points.len() - 1]] {
                    for x in a.crossings.iter().filter(|x| x.connection == cr.connection) {
                        let dx = (end.x - x.at.x).abs();
                        let dy = (end.y - x.at.y).abs();
                        // Within a quarter detailed pitch of a crossing ⇒ must be exact.
                        if dx < near && dy < near {
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

    /// Fixtures route through the detailed stage (per-cell pass + hotspot finisher)
    /// without panicking; any residual failure is reported honestly with provenance —
    /// a per-cell failure that the finisher could not repair carries `finisher: …`,
    /// and led-r/quad route fully clean.
    #[test]
    fn fixtures_route_or_report_honestly() {
        // led-r and quad route fully clean through the detailed stage (per-cell pass
        // + hotspot finisher). congested's saturated-wall residual is exercised by
        // `pipeline::tests::congested_auto_reports_honest_failures` — kept out of this
        // (cheaper) test so it does not pay for the full-board finisher twice.
        for name in ["led-r.json", "quad.json"] {
            let p = load(name);
            let (_mesh, r) = run(&p);
            // Any residual failure would carry finisher provenance; there are none.
            for f in &r.failed {
                assert!(
                    f.reason.starts_with("finisher: "),
                    "{name}: a residual failure must carry finisher provenance: {}",
                    f.reason
                );
            }
            assert!(
                r.is_clean(),
                "{name}: detailed stage must be clean: {:?}",
                r.failed
            );
            assert!(
                !r.cell_routes.is_empty(),
                "{name}: produced at least one cell route"
            );
        }
    }
}
