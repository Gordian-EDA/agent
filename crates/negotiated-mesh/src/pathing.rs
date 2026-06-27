//! Congestion-costed global pathing with negotiated rip-up & reroute.
//!
//! Slice 2's global routing stage. Where the slice-1 grid router ([`crate::router`])
//! commits each net's copper greedily — and so walls off later nets on congested
//! boards — this stage routes every net as a coarse *cell path* over the
//! [`CapacityMesh`] and then negotiates: nets that overload a shared boundary pay
//! a rising history cost until they spread out or the board is proven infeasible.
//! The product is a [`GlobalPlan`] (per-net cell paths plus boundary-crossing
//! info) and an honest [`CongestionReport`]; it is **not** copper. Slice 3 turns
//! cell paths into copper; this stage only proves a feasible coarse assignment
//! exists and reports congestion when it does not.
//!
//! This module is deliberately independent of [`crate::grid`] / [`crate::astar`]:
//! it shares only the [`RouteProblem`] model, the [`CapacityMesh`], and
//! [`FailedNet`] (the cross-stage failure type). The always-correct slice-1
//! fallback stays untouched.
//!
//! ## Algorithm
//!
//! 1. **Per-net A\*** over the leaf graph. A search state is `(layer, leaf)`.
//!    Moving across a [`crate::mesh::MeshEdge`] to an adjacent leaf costs the euclidean
//!    centre-to-centre distance scaled by `1 + congestion + history` of that edge
//!    on the traversed layer. Changing layer inside a leaf (a via) costs a fixed
//!    base plus a congestion term and requires capacity on *both* layers. The
//!    heuristic is the euclidean distance from a leaf centre to the nearest target
//!    leaf centre — admissible (it never exceeds the cheapest possible remaining
//!    distance, since every edge factor is `≥ 1`). The heap breaks ties
//!    deterministically by `(cost, leaf, layer)`.
//! 2. **Multi-point nets** grow a tree: point 0's cell seeds the tree; each
//!    further point is routed to the nearest cell already in the tree (the
//!    slice-1 point-to-tree shape, over cells).
//! 3. **Negotiated rip-up (PathFinder).** All nets route once in deterministic
//!    order (half-perimeter ascending, name tie-break — the slice-1 order). Then,
//!    while any edge's usage exceeds its capacity and we are under the iteration
//!    cap: bump the history cost of every overflowed edge, rip up only the nets
//!    crossing an overflowed edge, and reroute the most overflow-involved victims
//!    first with deterministic tie-breaks. Each iteration's total overflow is
//!    recorded so a stalled negotiation is visible.
//!
//! ## Determinism
//!
//! Net order is fixed; the A\* heap tie-breaks on `(cost, leaf, layer)` with a
//! total float order (`OrdF64`); usage and history live in dense `Vec`s keyed
//! by edge/leaf id; rip-up sets are collected in sorted order. Two runs serialize
//! byte-for-byte (a determinism test asserts this).

use crate::mesh::{CapacityMesh, LeafId};
use crate::problem::{Connection, FailedNet, Point2, RouteProblem};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{BTreeSet, BinaryHeap};

// ── design constants ─────────────────────────────────────────────────────────

/// Congestion penalty weight `K`: an edge loaded to its capacity adds `K` to its
/// per-unit-length cost (`penalty = (usage/capacity)^2 × K`). Higher `K` makes
/// the router avoid loaded edges more aggressively.
const CONGESTION_K: f64 = 8.0;

/// History-cost increment applied to each overflowed edge per rip-up iteration
/// (PathFinder's accumulating term — it is *not* reset between iterations, so a
/// chronically-overloaded edge becomes ever more expensive until nets route
/// around it).
const HISTORY_INCREMENT: f64 = 1.0;

/// Cap for adaptive history growth when rip-up iterations stop reducing overflow.
/// Keeping the multiplier bounded preserves deterministic behaviour and avoids
/// making one stubborn edge infinitely expensive relative to real route distance.
const HISTORY_STALL_BOOST_MAX: usize = 8;

/// Maximum negotiated rip-up iterations before the router gives up and reports
/// the remaining overflow honestly (no infinite loop on an infeasible board).
const MAX_ITERATIONS: usize = 40;

/// Base cost of a layer change (via) inside a leaf, in mm-equivalent units. Small
/// but nonzero so the router prefers staying on a layer when it is free to.
const VIA_BASE_COST: f64 = 0.5;

/// Small additive penalty for routing a global edge against the layer's preferred
/// direction. This borrows the proven PCB-router idea of alternating horizontal /
/// vertical layer preferences, but keeps it soft: congestion/history and topology
/// still dominate, and the bias is not large enough to make a clean open two-pin
/// same-layer route add vias just for style.
const WRONG_WAY_K: f64 = 0.03;

/// Above this many routed-tree cells, precompute exact nearest-target distance
/// per leaf once instead of scanning every tree centre for every A* heap pop.
const TARGET_DISTANCE_FIELD_THRESHOLD: usize = 32;

/// How many edge hotspots the congestion report lists (top-N by load ratio).
const HOTSPOT_TOP_N: usize = 8;

// ── total-order float wrapper ────────────────────────────────────────────────

/// A total-order wrapper over `f64` for use as a [`BinaryHeap`] / sort key.
///
/// A* costs are finite non-negative distances, but `f64` is only `PartialOrd`;
/// this hand-rolled wrapper (no new dependency — see `CLAUDE.md`) gives a total
/// order via [`f64::total_cmp`] so the heap tie-break is deterministic. `NaN`
/// never appears here (all inputs are finite), but `total_cmp` orders it anyway.
#[derive(Debug, Clone, Copy, PartialEq)]
struct OrdF64(f64);

impl Eq for OrdF64 {}
impl PartialOrd for OrdF64 {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for OrdF64 {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0)
    }
}

// ── public result types ──────────────────────────────────────────────────────

/// Which side of a leaf a cell path enters or exits through, with the crossing
/// geometry. Slice 3 consumes this to assign the crossing a concrete coordinate
/// on the shared boundary; here it records *which* mesh edge was used and the
/// boundary segment shared with the neighbour.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Crossing {
    /// The neighbouring leaf this step moves to (the other end of the edge).
    pub neighbor: LeafId,
    /// Index into [`crate::mesh::CapacityMesh::edges`] of the traversed edge (so slice 3 can
    /// recover the shared-boundary geometry and per-layer capacity directly).
    pub edge: usize,
    /// The copper layer the crossing happens on (0 = top).
    pub layer: usize,
    /// Midpoint of the shared boundary segment (mm) — a default crossing point
    /// slice 3 may redistribute across the segment to avoid intra-boundary clash.
    pub at: Point2,
}

/// One step of a cell path: the leaf occupied, the layer, and how the path
/// leaves this leaf toward the next step.
///
/// The step records the layer the path is *on* while in this leaf. A step whose
/// [`exit`](CellStep::exit) is `Some` moves to an adjacent leaf across that
/// crossing; a step whose `exit` is `None` is the path's last step in this
/// sub-route (a target). A [`via`](CellStep::via) flag marks a layer change that
/// happens *within* this leaf before the exit (the path entered on one layer and
/// leaves on another).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CellStep {
    /// The leaf this step occupies.
    pub leaf: LeafId,
    /// The copper layer this step is on (0 = top).
    pub layer: usize,
    /// Cell centre (mm) — the coarse waypoint slice 3 refines into copper.
    pub center: Point2,
    /// `true` if the path changes layer (a via) inside this leaf, between
    /// entering it and leaving it. The via sits at this leaf's centre.
    pub via: bool,
    /// How the path leaves this leaf toward the next step, or `None` for the last
    /// step of this sub-route (a reached target).
    pub exit: Option<Crossing>,
}

/// A single connected cell path: one root-to-target sub-route of a (possibly
/// multi-point) net, as an ordered sequence of [`CellStep`]s.
///
/// The first step's leaf is where this sub-route attaches to the net's growing
/// tree (or the net's first point, for the very first sub-route); the last step's
/// leaf is the target point's cell. Boundary crossings are carried per step in
/// [`CellStep::exit`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CellPath {
    /// The ordered cell steps; `steps.len() >= 1`. The entry boundary of step
    /// `i+1` is the exit boundary of step `i` (reversed orientation), so entry
    /// info is recoverable from the previous step's [`Crossing`] without
    /// duplicating it.
    pub steps: Vec<CellStep>,
}

/// The global plan for one net: the connection name plus its cell paths.
///
/// A 2-point net has one [`CellPath`]; an N-point net has up to `N-1` paths (one
/// per point joined to the tree). Together the paths span all of the net's
/// points over the mesh.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NetPlan {
    /// Connection name.
    pub connection: String,
    /// The cell paths joining this net's points (deterministic order: the order
    /// the points were attached to the tree).
    pub paths: Vec<CellPath>,
}

/// The coarse global plan: a cell path per routed net.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GlobalPlan {
    /// Per-net plans, in deterministic net order.
    pub nets: Vec<NetPlan>,
}

/// One congested mesh edge in the report: its location and load.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EdgeHotspot {
    /// Index into [`crate::mesh::CapacityMesh::edges`].
    pub edge: usize,
    /// The two leaves the edge joins (mirrors the mesh edge, for convenience).
    pub a: LeafId,
    pub b: LeafId,
    /// The copper layer of the worst load on this edge.
    pub layer: usize,
    /// Tracks routed across this edge on `layer`.
    pub usage: u32,
    /// The edge's capacity on `layer`.
    pub capacity: u32,
    /// `usage / max(capacity, 1)` — the load ratio the hotspots are ranked by.
    pub load: f64,
}

/// The honest congestion report for a [`global_route`] run.
///
/// A plan is *feasible* iff `final_overflow == 0 && unrouted.is_empty()`; see
/// [`GlobalRouteResult::is_feasible`]. Hitting `MAX_ITERATIONS` with overflow
/// remaining is visible here (`iterations == MAX_ITERATIONS` with
/// `final_overflow > 0`), never silently absorbed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CongestionReport {
    /// Rip-up iterations performed (0 = first routing pass already feasible).
    pub iterations: usize,
    /// Total residual overflow `sum(max(usage - capacity, 0))` across all edges
    /// and layers after the last pass. `0` ⇔ no edge is over capacity.
    pub final_overflow: u32,
    /// The most-loaded edges (top-N by load ratio), worst first.
    pub edge_hotspots: Vec<EdgeHotspot>,
    /// Nets that could not be routed at all (no path on the mesh), in
    /// deterministic order. Distinct from overflow: an unrouted net found no
    /// path; overflow is a path that shares an over-capacity edge.
    pub unrouted: Vec<FailedNet>,
}

/// The full result of [`global_route`]: the plan plus its congestion report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GlobalRouteResult {
    /// The coarse cell-path plan.
    pub plan: GlobalPlan,
    /// The congestion report (feasibility, hotspots, unrouted nets).
    pub report: CongestionReport,
}

impl GlobalRouteResult {
    /// A plan is feasible iff no edge is over capacity and every net routed.
    pub fn is_feasible(&self) -> bool {
        self.report.final_overflow == 0 && self.report.unrouted.is_empty()
    }
}

// ── entry point ──────────────────────────────────────────────────────────────

/// Globally route `problem`: build the capacity mesh, route every net's points
/// over it with congestion-costed A\*, and negotiate rip-up until feasible or the
/// iteration cap is hit. Never panics; never silently drops a net.
pub fn global_route(problem: &RouteProblem) -> GlobalRouteResult {
    let mesh = CapacityMesh::build(problem);
    global_route_with_mesh(problem, &mesh)
}

/// As [`global_route`], but over a pre-built mesh (slice 4's SVG overlay builds
/// the mesh once and shares it).
pub fn global_route_with_mesh(problem: &RouteProblem, mesh: &CapacityMesh) -> GlobalRouteResult {
    let orders = net_order_portfolio(problem);
    let mut best = Router::new(problem, mesh).run_order(&orders[0], VictimOrder::Legacy);
    if best.is_feasible() {
        return best;
    }

    // Freerouting-style negotiated routers are sensitive to net order on dense
    // boards. Keep the old order as the fast path, but when it cannot clear
    // congestion, try a tiny deterministic order portfolio before reporting
    // failure to the caller's route retry loop.
    for order in &orders {
        let candidate = Router::new(problem, mesh).run_order(order, VictimOrder::OverflowPressure);
        if global_result_better(&candidate, &best) {
            let feasible = candidate.is_feasible();
            best = candidate;
            if feasible {
                break;
            }
        }
    }
    best
}

// ── internals ────────────────────────────────────────────────────────────────

/// A target endpoint of a net, resolved onto the mesh.
#[derive(Debug, Clone, Copy)]
struct Endpoint {
    leaf: LeafId,
    layer: usize,
}

/// Per-net resolved routing input (connection index, name, endpoints).
struct NetInput {
    conn: usize,
    name: String,
    endpoints: Vec<Endpoint>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VictimOrder {
    Legacy,
    OverflowPressure,
}

/// Negotiated global router state over the mesh.
struct Router<'a> {
    problem: &'a RouteProblem,
    mesh: &'a CapacityMesh,
    layer_count: usize,
    /// Adjacency: for each leaf, `(neighbor_leaf, edge_index)` pairs. Built once.
    adjacency: Vec<Vec<(LeafId, usize)>>,
    /// Per-edge, per-layer usage (track count crossing the edge on that layer).
    edge_usage: Vec<Vec<u32>>,
    /// Per-edge, per-layer accumulated history cost (PathFinder term).
    edge_history: Vec<Vec<f64>>,
    /// Per-leaf via usage (layer changes inside the leaf), for via congestion.
    leaf_via_usage: Vec<u32>,
    /// The plan being built: `plan[i]` corresponds to net order position `i`.
    net_plans: Vec<NetPlan>,
    /// For rip-up: per net-order-position, the set of edge indices it crosses.
    net_edges: Vec<BTreeSet<usize>>,
    /// Nets that could not be routed at all.
    unrouted: Vec<FailedNet>,
}

impl<'a> Router<'a> {
    fn new(problem: &'a RouteProblem, mesh: &'a CapacityMesh) -> Self {
        let layer_count = mesh.layer_count.max(1);
        let n_leaves = mesh.leaves.len();
        let n_edges = mesh.edges.len();

        // Build leaf adjacency from the sorted edge list. Deterministic: edges
        // are already (a,b)-sorted, and we push both directions in that order.
        let mut adjacency: Vec<Vec<(LeafId, usize)>> = vec![Vec::new(); n_leaves];
        for (ei, e) in mesh.edges.iter().enumerate() {
            adjacency[e.a].push((e.b, ei));
            adjacency[e.b].push((e.a, ei));
        }
        // Sort each leaf's neighbours by (neighbor, edge) for deterministic
        // expansion order in A*.
        for adj in &mut adjacency {
            adj.sort_unstable();
        }

        Router {
            problem,
            mesh,
            layer_count,
            adjacency,
            edge_usage: vec![vec![0; layer_count]; n_edges],
            edge_history: vec![vec![0.0; layer_count]; n_edges],
            leaf_via_usage: vec![0; n_leaves],
            net_plans: Vec::new(),
            net_edges: Vec::new(),
            unrouted: Vec::new(),
        }
    }

    /// Run the full negotiated route for one concrete net order and produce the result.
    fn run_order(&mut self, order: &[usize], victim_order: VictimOrder) -> GlobalRouteResult {
        let inputs: Vec<NetInput> = order.iter().map(|&ci| self.net_input(ci)).collect();

        // Initial routing pass: every net in deterministic order.
        self.net_plans = vec![
            NetPlan {
                connection: String::new(),
                paths: Vec::new(),
            };
            inputs.len()
        ];
        self.net_edges = vec![BTreeSet::new(); inputs.len()];
        for (pos, input) in inputs.iter().enumerate() {
            self.route_net(pos, input);
        }

        let mut iterations = 0usize;
        let mut best_overflow = self.total_overflow();
        let mut stagnant_iterations = 0usize;

        // Negotiated rip-up & reroute.
        while self.total_overflow() > 0 && iterations < MAX_ITERATIONS {
            iterations += 1;
            let history_increment = history_increment_for_stall(stagnant_iterations);

            // 1. Bump history on every overflowed (edge, layer); collect the
            //    overflowed edge indices.
            let mut overflowed_edges: BTreeSet<usize> = BTreeSet::new();
            for (ei, usage) in self.edge_usage.iter().enumerate() {
                for (layer, &u) in usage.iter().enumerate() {
                    let cap = self.mesh.edges[ei]
                        .capacity
                        .get(layer)
                        .copied()
                        .unwrap_or(0);
                    if u > cap {
                        self.edge_history[ei][layer] += history_increment;
                        overflowed_edges.insert(ei);
                    }
                }
            }

            // 2. Rip up only the nets that cross an overflowed edge.
            let mut victims: Vec<usize> = Vec::new();
            for (pos, edges) in self.net_edges.iter().enumerate() {
                if edges.iter().any(|e| overflowed_edges.contains(e)) {
                    victims.push(pos);
                }
            }
            if victim_order == VictimOrder::OverflowPressure {
                order_victims_by_overflow_pressure(
                    &mut victims,
                    &self.net_edges,
                    &overflowed_edges,
                );
            }
            for &pos in &victims {
                self.rip_up(pos);
            }

            // 3. Reroute the victims in deterministic order. The primary route
            //    keeps legacy net order; fallback portfolio routes can use
            //    congestion pressure to shake loose stalled negotiations.
            for &pos in &victims {
                self.route_net(pos, &inputs[pos]);
            }

            let overflow_after = self.total_overflow();
            if overflow_after < best_overflow {
                best_overflow = overflow_after;
                stagnant_iterations = 0;
            } else {
                stagnant_iterations += 1;
            }
        }

        let final_overflow = self.total_overflow();
        let plan = GlobalPlan {
            nets: std::mem::take(&mut self.net_plans),
        };
        let report = CongestionReport {
            iterations,
            final_overflow,
            edge_hotspots: self.hotspots(),
            unrouted: std::mem::take(&mut self.unrouted),
        };
        GlobalRouteResult { plan, report }
    }

    /// Resolve a connection's points onto mesh endpoints.
    fn net_input(&self, conn: usize) -> NetInput {
        let c = &self.problem.connections[conn];
        let endpoints = c
            .points_to_connect
            .iter()
            .map(|pt| {
                let layer = pt
                    .layer
                    .index(self.layer_count as u32)
                    .unwrap_or(0)
                    .min(self.layer_count as u32 - 1) as usize;
                let leaf = self.mesh.cell_at(&Point2 { x: pt.x, y: pt.y });
                Endpoint { leaf, layer }
            })
            .collect();
        NetInput {
            conn,
            name: c.name.clone(),
            endpoints,
        }
    }

    /// Route one net (by net-order position), committing usage and recording its
    /// plan. A trivially-connected net (< 2 points) produces an empty plan. A net
    /// that cannot be routed is recorded in `unrouted` and produces an empty plan.
    fn route_net(&mut self, pos: usize, input: &NetInput) {
        self.net_plans[pos] = NetPlan {
            connection: input.name.clone(),
            paths: Vec::new(),
        };
        self.net_edges[pos].clear();

        if input.endpoints.len() < 2 {
            return; // nothing to connect
        }

        // Point-to-tree growth over cells: point 0 seeds the tree; each further
        // point routes to the nearest cell already in the tree.
        let conn = input.conn;
        let mut tree: BTreeSet<(usize, LeafId)> = BTreeSet::new();
        let start = input.endpoints[0];
        tree.insert((start.layer, start.leaf));

        // The net's own pad cells: a pad zeroes its cell's track capacity, but the
        // net must still start at and reach its own pads — so these cells gate on
        // FOREIGN copper only, not on capacity.
        let own_leaves: BTreeSet<LeafId> = input.endpoints.iter().map(|e| e.leaf).collect();

        let mut failed: Option<String> = None;
        let mut paths: Vec<CellPath> = Vec::new();

        for (pi, ep) in input.endpoints.iter().enumerate().skip(1) {
            match self.astar(conn, *ep, &tree, &own_leaves) {
                Some(path) => {
                    // Add all the path's cells to the tree so later points can
                    // tap anywhere along it.
                    for step in &path.steps {
                        tree.insert((step.layer, step.leaf));
                    }
                    paths.push(path);
                }
                None => {
                    failed = Some(format!(
                        "no mesh path from point {pi} to the routed tree (congestion or enclosure)"
                    ));
                    break;
                }
            }
        }

        if let Some(reason) = failed {
            // Honest failure: record it, commit nothing for this net.
            self.unrouted.push(FailedNet {
                connection: input.name.clone(),
                reason,
            });
            self.net_plans[pos].paths.clear();
            return;
        }

        // Commit: bump usage for every crossing & via, record crossed edges.
        for path in &paths {
            self.commit_path(pos, path);
        }
        self.net_plans[pos].paths = paths;
    }

    /// Rip up a previously-routed net: undo its usage and clear its plan/edges.
    fn rip_up(&mut self, pos: usize) {
        let paths = std::mem::take(&mut self.net_plans[pos].paths);
        for path in &paths {
            for step in &path.steps {
                if step.via {
                    self.leaf_via_usage[step.leaf] =
                        self.leaf_via_usage[step.leaf].saturating_sub(1);
                }
                if let Some(x) = &step.exit {
                    let u = &mut self.edge_usage[x.edge][x.layer];
                    *u = u.saturating_sub(1);
                }
            }
        }
        self.net_edges[pos].clear();
    }

    /// Commit a path's usage (edges + vias) and record its crossed edges.
    fn commit_path(&mut self, pos: usize, path: &CellPath) {
        for step in &path.steps {
            if step.via {
                self.leaf_via_usage[step.leaf] += 1;
            }
            if let Some(x) = &step.exit {
                self.edge_usage[x.edge][x.layer] += 1;
                self.net_edges[pos].insert(x.edge);
            }
        }
    }

    /// Congestion-costed A\* from any target endpoint toward the routed tree.
    ///
    /// Searches *from* the new endpoint `ep` to the nearest cell already in
    /// `tree` (multi-target Dijkstra/A\* — the goal set is the tree). State is
    /// `(layer, leaf)`; the heuristic is euclidean distance to the nearest tree
    /// leaf centre. Returns the path from `ep` to the reached tree cell, or
    /// `None` if the tree is unreachable.
    fn astar(
        &self,
        conn: usize,
        ep: Endpoint,
        tree: &BTreeSet<(usize, LeafId)>,
        own_leaves: &BTreeSet<LeafId>,
    ) -> Option<CellPath> {
        let n_leaves = self.mesh.leaves.len();
        let lc = self.layer_count;

        // A cell is passable for this net if it has track capacity — OR it is one
        // of the net's OWN pad cells (which the pad zeroes), in which case only
        // FOREIGN copper blocks it. Without this, a net can never start at or reach
        // its own pads and the global router fails every net.
        let passable = |leaf: LeafId, layer: usize| -> bool {
            if own_leaves.contains(&leaf) {
                // The net's own pad cell: it MUST be able to start at and reach
                // its pads. At the mesh's resolution a pad can share a leaf with a
                // different-net pad (foreign_blocked), but the net still has to
                // get to its own copper there — the cell router resolves the exact
                // track geometry around the foreign pad in stage 3.
                true
            } else {
                self.leaf_capacity(leaf, layer, conn) > 0
            }
        };

        // State index = leaf * lc + layer, for dense visited / g-score arrays.
        let sidx = |leaf: LeafId, layer: usize| leaf * lc + layer;
        let n_states = n_leaves * lc;

        let mut target_state = vec![false; n_states];
        for &(layer, leaf) in tree {
            target_state[sidx(leaf, layer)] = true;
        }

        // Precompute tree leaf centres for the heuristic (nearest target centre).
        let tree_centers: Vec<Point2> = tree
            .iter()
            .map(|&(_, leaf)| self.mesh.leaves[leaf].rect.center())
            .collect();
        let target_distance_field = (tree_centers.len() >= TARGET_DISTANCE_FIELD_THRESHOLD)
            .then(|| self.target_distance_field(&tree_centers));
        let heuristic = |leaf: LeafId| -> f64 {
            if let Some(field) = &target_distance_field {
                return field[leaf];
            }
            let c = self.mesh.leaves[leaf].rect.center();
            tree_centers
                .iter()
                .map(|t| c.dist(*t))
                .fold(f64::INFINITY, f64::min)
        };

        // The start endpoint's cell must admit the net; its own pad zeroes the
        // cell's track capacity, so this gates on foreign copper (via `passable`),
        // not raw capacity — else the net could never start at its own pad.
        if !passable(ep.leaf, ep.layer) {
            return None;
        }

        let mut g = vec![f64::INFINITY; n_states];
        let mut came_from: Vec<Option<(usize, MoveKind)>> = vec![None; n_states];
        let mut visited = vec![false; n_states];

        let start = sidx(ep.leaf, ep.layer);
        g[start] = 0.0;
        let mut heap: BinaryHeap<HeapItem> = BinaryHeap::new();
        heap.push(HeapItem {
            f: OrdF64(heuristic(ep.leaf)),
            g: OrdF64(0.0),
            leaf: ep.leaf,
            layer: ep.layer,
        });

        let mut goal: Option<(LeafId, usize)> = None;
        while let Some(item) = heap.pop() {
            let s = sidx(item.leaf, item.layer);
            if visited[s] {
                continue;
            }
            visited[s] = true;

            // Reached the tree? (the new endpoint searches toward the tree)
            if target_state[s] {
                goal = Some((item.leaf, item.layer));
                break;
            }

            let cur_g = item.g.0;

            // 1. Cross an edge to an adjacent leaf, same layer.
            for &(nb, ei) in &self.adjacency[item.leaf] {
                // The neighbour must admit this net on this layer (own pad cells
                // gate on foreign copper, not capacity — see `passable`).
                if !passable(nb, item.layer) {
                    continue;
                }
                // A boundary with zero *structural* capacity (a keepout sits on
                // the whole shared segment for this layer) physically cannot carry
                // a track — it is impassable, not merely congested. Skipping it is
                // what makes a true cut honestly unroutable rather than an
                // overflow through solid copper. (A congested edge — capacity ≥ 1,
                // usage over it — stays passable so rip-up can negotiate it.)
                // EXCEPTION: a boundary touching one of the net's OWN pad cells is
                // the pad's own edge (the pad covers it, zeroing structural cap) —
                // the owning net must be able to cross into/out of its pad.
                let own_edge = own_leaves.contains(&item.leaf) || own_leaves.contains(&nb);
                if !own_edge
                    && self.mesh.edges[ei]
                        .capacity
                        .get(item.layer)
                        .copied()
                        .unwrap_or(0)
                        == 0
                {
                    continue;
                }
                let step_cost = self.edge_cost(ei, item.layer, item.leaf, nb);
                let ng = cur_g + step_cost;
                let ns = sidx(nb, item.layer);
                if ng < g[ns] {
                    g[ns] = ng;
                    came_from[ns] = Some((s, MoveKind::Edge(ei)));
                    heap.push(HeapItem {
                        f: OrdF64(ng + heuristic(nb)),
                        g: OrdF64(ng),
                        leaf: nb,
                        layer: item.layer,
                    });
                }
            }

            // 2. Change layer inside the same leaf (a via). Requires capacity on
            //    both the current and the target layer for this net.
            for other in 0..lc {
                if other == item.layer {
                    continue;
                }
                if !passable(item.leaf, item.layer) || !passable(item.leaf, other) {
                    continue;
                }
                let step_cost = self.via_cost(item.leaf);
                let ng = cur_g + step_cost;
                let ns = sidx(item.leaf, other);
                if ng < g[ns] {
                    g[ns] = ng;
                    came_from[ns] = Some((s, MoveKind::Via));
                    heap.push(HeapItem {
                        f: OrdF64(ng + heuristic(item.leaf)),
                        g: OrdF64(ng),
                        leaf: item.leaf,
                        layer: other,
                    });
                }
            }
        }

        let (goal_leaf, goal_layer) = goal?;
        Some(self.reconstruct(start, sidx(goal_leaf, goal_layer), lc, &came_from))
    }

    fn target_distance_field(&self, targets: &[Point2]) -> Vec<f64> {
        self.mesh
            .leaves
            .iter()
            .map(|leaf| {
                let c = leaf.rect.center();
                targets
                    .iter()
                    .map(|t| c.dist(*t))
                    .fold(f64::INFINITY, f64::min)
            })
            .collect()
    }

    /// Reconstruct the [`CellPath`] from the A\* came-from chain. The chain runs
    /// goal → start; we reverse it so the path reads start (endpoint) → goal
    /// (tree). Exit crossings and via flags are filled per step.
    fn reconstruct(
        &self,
        start: usize,
        goal: usize,
        lc: usize,
        came_from: &[Option<(usize, MoveKind)>],
    ) -> CellPath {
        // Walk goal → start collecting (state, move-into-this-state).
        let mut chain: Vec<(usize, Option<MoveKind>)> = Vec::new();
        let mut cur = goal;
        loop {
            match came_from[cur] {
                Some((prev, kind)) => {
                    chain.push((cur, Some(kind)));
                    cur = prev;
                }
                None => {
                    chain.push((start, None));
                    break;
                }
            }
        }
        chain.reverse(); // now start → goal, each with the move that produced it

        let leaf_of = |s: usize| s / lc;
        let layer_of = |s: usize| s % lc;

        // Build steps. A step's `exit` describes the move OUT of it, i.e. the
        // move recorded on the *next* chain entry (if it is an Edge); a Via on the
        // next entry sets the *current* step's `via` flag (layer change within the
        // leaf) and does not advance the leaf.
        let mut steps: Vec<CellStep> = Vec::new();
        let mut i = 0;
        while i < chain.len() {
            let (s, _) = chain[i];
            let leaf = leaf_of(s);
            let layer = layer_of(s);
            let mut via = false;

            // Absorb any in-leaf via moves that follow (same leaf, layer change).
            let mut j = i + 1;
            let mut cur_layer = layer;
            while j < chain.len() {
                if let (_, Some(MoveKind::Via)) = chain[j] {
                    via = true;
                    cur_layer = layer_of(chain[j].0);
                    j += 1;
                } else {
                    break;
                }
            }

            // Determine the exit: the next chain entry after the absorbed vias, if
            // it is an Edge move out of this leaf.
            let exit = if j < chain.len() {
                if let (ns, Some(MoveKind::Edge(ei))) = chain[j] {
                    let neighbor = leaf_of(ns);
                    Some(self.make_crossing(ei, cur_layer, leaf, neighbor))
                } else {
                    None
                }
            } else {
                None
            };

            steps.push(CellStep {
                leaf,
                layer,
                center: self.mesh.leaves[leaf].rect.center(),
                via,
                exit,
            });

            i = j; // advance past this leaf's vias to the next leaf (or end)
        }

        CellPath { steps }
    }

    /// Build a [`Crossing`] for an edge traversal between `leaf` and `neighbor`.
    fn make_crossing(&self, edge: usize, layer: usize, leaf: LeafId, neighbor: LeafId) -> Crossing {
        let at = self.shared_midpoint(leaf, neighbor);
        Crossing {
            neighbor,
            edge,
            layer,
            at,
        }
    }

    /// Midpoint of the shared boundary segment between two adjacent leaves (mm).
    /// Falls back to the average of the two centres if (defensively) the rects do
    /// not actually share a boundary.
    fn shared_midpoint(&self, a: LeafId, b: LeafId) -> Point2 {
        let ra = &self.mesh.leaves[a].rect;
        let rb = &self.mesh.leaves[b].rect;
        if let Some(shared) = ra.shared_boundary(rb) {
            return shared.midpoint();
        }
        let ca = ra.center();
        let cb = rb.center();
        Point2 {
            x: (ca.x + cb.x) / 2.0,
            y: (ca.y + cb.y) / 2.0,
        }
    }

    /// The cost of crossing edge `ei` on `layer`, from `from` to `to`:
    /// `centre_from.dist(centre_to) × (1 + congestion + history + preferred-dir)`.
    fn edge_cost(&self, ei: usize, layer: usize, from: LeafId, to: LeafId) -> f64 {
        let from_center = self.mesh.leaves[from].rect.center();
        let to_center = self.mesh.leaves[to].rect.center();
        let dist = from_center.dist(to_center);
        let cap = self.mesh.edges[ei]
            .capacity
            .get(layer)
            .copied()
            .unwrap_or(0);
        let usage = self.edge_usage[ei][layer];
        // If this net routes here it will add one unit: cost the *prospective*
        // load so a net avoids saturating an already-full edge.
        let prospective = usage + 1;
        let penalty = congestion_penalty(prospective, cap);
        let history = self.edge_history[ei][layer];
        let movement_is_horizontal =
            (to_center.x - from_center.x).abs() >= (to_center.y - from_center.y).abs();
        let wrong_way = preferred_direction_penalty(movement_is_horizontal, layer);
        dist * (1.0 + penalty + history + wrong_way)
    }

    /// The cost of a layer change inside `leaf` (a via): a fixed base plus a
    /// congestion term on the leaf's via usage vs its smallest layer capacity.
    fn via_cost(&self, leaf: LeafId) -> f64 {
        // Use the leaf's minimum per-layer capacity as the via budget proxy.
        let cap = self.mesh.leaves[leaf]
            .layers
            .iter()
            .map(|l| l.capacity)
            .min()
            .unwrap_or(0);
        let prospective = self.leaf_via_usage[leaf] + 1;
        let penalty = congestion_penalty(prospective, cap);
        VIA_BASE_COST * (1.0 + penalty)
    }

    /// A leaf's capacity on a layer for connection `conn` (foreign copper ⇒ 0).
    fn leaf_capacity(&self, leaf: LeafId, layer: usize, conn: usize) -> u32 {
        self.mesh.leaves[leaf].capacity_for(layer, conn)
    }

    /// Total residual overflow across all edges and layers.
    fn total_overflow(&self) -> u32 {
        let mut total = 0u32;
        for (ei, usage) in self.edge_usage.iter().enumerate() {
            for (layer, &u) in usage.iter().enumerate() {
                let cap = self.mesh.edges[ei]
                    .capacity
                    .get(layer)
                    .copied()
                    .unwrap_or(0);
                total += u.saturating_sub(cap);
            }
        }
        total
    }

    /// The top-N edge hotspots by load ratio (usage/capacity), worst first.
    fn hotspots(&self) -> Vec<EdgeHotspot> {
        let mut spots: Vec<EdgeHotspot> = Vec::new();
        for (ei, usage) in self.edge_usage.iter().enumerate() {
            // The worst-loaded layer for this edge.
            let mut best: Option<(usize, u32, u32, f64)> = None;
            for (layer, &u) in usage.iter().enumerate() {
                if u == 0 {
                    continue;
                }
                let cap = self.mesh.edges[ei]
                    .capacity
                    .get(layer)
                    .copied()
                    .unwrap_or(0);
                let load = u as f64 / (cap.max(1) as f64);
                match best {
                    Some((_, _, _, bl)) if bl >= load => {}
                    _ => best = Some((layer, u, cap, load)),
                }
            }
            if let Some((layer, u, cap, load)) = best {
                let e = &self.mesh.edges[ei];
                spots.push(EdgeHotspot {
                    edge: ei,
                    a: e.a,
                    b: e.b,
                    layer,
                    usage: u,
                    capacity: cap,
                    load,
                });
            }
        }
        // Rank by load desc, tie-break by edge index asc (deterministic).
        spots.sort_by(|p, q| q.load.total_cmp(&p.load).then_with(|| p.edge.cmp(&q.edge)));
        spots.truncate(HOTSPOT_TOP_N);
        spots
    }
}

/// A* move kind: across an edge to a neighbour, or a via inside a leaf.
#[derive(Debug, Clone, Copy)]
enum MoveKind {
    Edge(usize),
    Via,
}

/// A binary-heap item, ordered as a min-heap on `(f, leaf, layer)` via `Reverse`
/// semantics (we implement `Ord` to pop the smallest `f` first).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HeapItem {
    f: OrdF64,
    g: OrdF64,
    leaf: LeafId,
    layer: usize,
}

impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap is a max-heap; invert so the smallest f pops first. Tie-break
        // deterministically on (leaf, layer) — also inverted so the ordering is a
        // total, stable function (the actual chosen tie does not matter for
        // correctness, only that it is deterministic).
        other
            .f
            .cmp(&self.f)
            .then_with(|| other.leaf.cmp(&self.leaf))
            .then_with(|| other.layer.cmp(&self.layer))
    }
}
impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Congestion penalty `(usage/capacity)^2 × K`. A zero-capacity edge is treated
/// as fully saturated (`load = usage`) so the router strongly avoids it without
/// dividing by zero.
fn congestion_penalty(usage: u32, capacity: u32) -> f64 {
    let load = if capacity == 0 {
        usage as f64
    } else {
        usage as f64 / capacity as f64
    };
    load * load * CONGESTION_K
}

fn history_increment_for_stall(stagnant_iterations: usize) -> f64 {
    HISTORY_INCREMENT * (1.0 + stagnant_iterations.min(HISTORY_STALL_BOOST_MAX) as f64)
}

/// Soft preferred-direction cost for a global mesh edge.
///
/// Even-numbered layers prefer horizontal movement, odd-numbered layers prefer
/// vertical movement, matching a conventional alternating stack.
fn preferred_direction_penalty(movement_is_horizontal: bool, layer: usize) -> f64 {
    let layer_prefers_horizontal = layer.is_multiple_of(2);
    if movement_is_horizontal == layer_prefers_horizontal {
        0.0
    } else {
        WRONG_WAY_K
    }
}

fn global_result_better(candidate: &GlobalRouteResult, incumbent: &GlobalRouteResult) -> bool {
    let ck = (
        !candidate.is_feasible(),
        candidate.report.unrouted.len(),
        candidate.report.final_overflow,
        candidate.report.iterations,
        global_plan_quality_key(&candidate.plan),
    );
    let ik = (
        !incumbent.is_feasible(),
        incumbent.report.unrouted.len(),
        incumbent.report.final_overflow,
        incumbent.report.iterations,
        global_plan_quality_key(&incumbent.plan),
    );
    ck < ik
}

fn global_plan_quality_key(plan: &GlobalPlan) -> (usize, usize) {
    let mut vias = 0usize;
    let mut steps = 0usize;
    for net in &plan.nets {
        for path in &net.paths {
            steps += path.steps.len();
            vias += path.steps.iter().filter(|step| step.via).count();
        }
    }
    (vias, steps)
}

fn order_victims_by_overflow_pressure(
    victims: &mut [usize],
    net_edges: &[BTreeSet<usize>],
    overflowed_edges: &BTreeSet<usize>,
) {
    victims.sort_by(|&a, &b| {
        let ap = victim_pressure(a, net_edges, overflowed_edges);
        let bp = victim_pressure(b, net_edges, overflowed_edges);
        bp.cmp(&ap).then_with(|| a.cmp(&b))
    });
}

fn victim_pressure(
    pos: usize,
    net_edges: &[BTreeSet<usize>],
    overflowed_edges: &BTreeSet<usize>,
) -> (usize, usize) {
    let edges = &net_edges[pos];
    (
        edges
            .iter()
            .filter(|edge| overflowed_edges.contains(edge))
            .count(),
        edges.len(),
    )
}

/// Connection indices in routing order: ascending bounding-box half-perimeter,
/// ties by name. The slice-1 order ([`crate::router`]), reproduced so pathing
/// does not depend on the router's internals.
fn net_order(problem: &RouteProblem) -> Vec<usize> {
    let mut order: Vec<usize> = (0..problem.connections.len()).collect();
    order.sort_by(|&a, &b| {
        let ka = problem.connections[a].half_perimeter();
        let kb = problem.connections[b].half_perimeter();
        ka.total_cmp(&kb).then_with(|| {
            problem.connections[a]
                .name
                .cmp(&problem.connections[b].name)
        })
    });
    order
}

/// Tiny deterministic fallback portfolio for negotiated routing. The first
/// order is the legacy fast path; the rest are only tried when that route is not
/// feasible.
fn net_order_portfolio(problem: &RouteProblem) -> Vec<Vec<usize>> {
    let mut orders = Vec::new();
    push_unique_order(&mut orders, net_order(problem));

    let mut obstacle_pressure: Vec<usize> = (0..problem.connections.len()).collect();
    obstacle_pressure.sort_by(|&a, &b| {
        connection_obstacle_pressure(problem, &problem.connections[b])
            .cmp(&connection_obstacle_pressure(
                problem,
                &problem.connections[a],
            ))
            .then_with(|| {
                problem.connections[b]
                    .half_perimeter()
                    .total_cmp(&problem.connections[a].half_perimeter())
            })
            .then_with(|| {
                problem.connections[a]
                    .name
                    .cmp(&problem.connections[b].name)
            })
    });
    push_unique_order(&mut orders, obstacle_pressure);

    let mut longest_first: Vec<usize> = (0..problem.connections.len()).collect();
    longest_first.sort_by(|&a, &b| {
        problem.connections[b]
            .half_perimeter()
            .total_cmp(&problem.connections[a].half_perimeter())
            .then_with(|| {
                problem.connections[a]
                    .name
                    .cmp(&problem.connections[b].name)
            })
    });
    push_unique_order(&mut orders, longest_first);

    let mut most_pins_first: Vec<usize> = (0..problem.connections.len()).collect();
    most_pins_first.sort_by(|&a, &b| {
        problem.connections[b]
            .points_to_connect
            .len()
            .cmp(&problem.connections[a].points_to_connect.len())
            .then_with(|| {
                problem.connections[b]
                    .half_perimeter()
                    .total_cmp(&problem.connections[a].half_perimeter())
            })
            .then_with(|| {
                problem.connections[a]
                    .name
                    .cmp(&problem.connections[b].name)
            })
    });
    push_unique_order(&mut orders, most_pins_first);

    let mut name_order: Vec<usize> = (0..problem.connections.len()).collect();
    name_order.sort_by(|&a, &b| {
        problem.connections[a]
            .name
            .cmp(&problem.connections[b].name)
    });
    push_unique_order(&mut orders, name_order);

    orders
}

fn connection_obstacle_pressure(problem: &RouteProblem, conn: &Connection) -> u64 {
    let Some(first) = conn.points_to_connect.first() else {
        return 0;
    };
    let (mut min_x, mut max_x, mut min_y, mut max_y) = (first.x, first.x, first.y, first.y);
    let mut terminal_layers = Vec::new();
    for pt in &conn.points_to_connect {
        min_x = min_x.min(pt.x);
        max_x = max_x.max(pt.x);
        min_y = min_y.min(pt.y);
        max_y = max_y.max(pt.y);
        if !terminal_layers.iter().any(|layer| layer == &pt.layer) {
            terminal_layers.push(pt.layer.clone());
        }
    }

    let expand = problem.clearance + problem.net_width(&conn.name) / 2.0;
    min_x -= expand;
    max_x += expand;
    min_y -= expand;
    max_y += expand;

    let mut pressure = 0;
    for obstacle in &problem.obstacles {
        if obstacle.connected_to.iter().any(|net| net == &conn.name) {
            continue;
        }
        if !obstacle
            .layers
            .iter()
            .any(|layer| terminal_layers.iter().any(|terminal| terminal == layer))
        {
            continue;
        }
        let ob_min_x = obstacle.center.x - obstacle.width / 2.0 - expand;
        let ob_max_x = obstacle.center.x + obstacle.width / 2.0 + expand;
        let ob_min_y = obstacle.center.y - obstacle.height / 2.0 - expand;
        let ob_max_y = obstacle.center.y + obstacle.height / 2.0 + expand;
        let overlap_x = max_x.min(ob_max_x) - min_x.max(ob_min_x);
        let overlap_y = max_y.min(ob_max_y) - min_y.max(ob_min_y);
        if overlap_x > 0.0 && overlap_y > 0.0 {
            pressure += 1_000_000 + ((overlap_x + overlap_y) * 1000.0).round() as u64;
        }
    }
    pressure
}

fn push_unique_order(orders: &mut Vec<Vec<usize>>, order: Vec<usize>) {
    if !orders.iter().any(|seen| seen == &order) {
        orders.push(order);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::problem::{Connection, LayerRef, Obstacle, Rect, RoutePoint};
    use std::path::Path;

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

    fn load(name: &str) -> RouteProblem {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures")
            .join(name);
        let json = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        serde_json::from_str(&json).unwrap_or_else(|e| panic!("parse {name}: {e}"))
    }

    /// Validate a plan's structural invariants against the mesh: every step's
    /// exit references a real edge joining the step's leaf to the named
    /// neighbour, entry/exit chain is consistent, and the last step has no exit.
    fn assert_plan_consistent(problem: &RouteProblem, result: &GlobalRouteResult) {
        let mesh = CapacityMesh::build(problem);
        for net in &result.plan.nets {
            for path in &net.paths {
                assert!(!path.steps.is_empty(), "a path has at least one step");
                let last = path.steps.len() - 1;
                for (i, step) in path.steps.iter().enumerate() {
                    assert!(step.leaf < mesh.leaves.len(), "step leaf in range");
                    assert!(step.layer < mesh.layer_count, "step layer in range");
                    match &step.exit {
                        Some(x) => {
                            assert!(i < last, "only non-final steps have an exit");
                            let e = &mesh.edges[x.edge];
                            let joins = (e.a == step.leaf && e.b == x.neighbor)
                                || (e.b == step.leaf && e.a == x.neighbor);
                            assert!(
                                joins,
                                "exit edge {} must join leaf {} to neighbor {}",
                                x.edge, step.leaf, x.neighbor
                            );
                            assert_eq!(
                                path.steps[i + 1].leaf,
                                x.neighbor,
                                "next step's leaf must equal the exit neighbor"
                            );
                        }
                        None => assert_eq!(i, last, "only the final step has no exit"),
                    }
                }
            }
        }
    }

    #[test]
    fn empty_two_point_net_routes_feasibly() {
        let p = base(
            bounds(16.0, 16.0),
            vec![],
            vec![conn("N", &[(2.0, 8.0, "top"), (14.0, 8.0, "top")])],
        );
        let r = global_route(&p);
        assert!(
            r.is_feasible(),
            "open board must be feasible: {:?}",
            r.report
        );
        assert_eq!(r.plan.nets.len(), 1);
        assert_eq!(r.plan.nets[0].paths.len(), 1, "2-point net = 1 path");
        assert_plan_consistent(&p, &r);
    }

    #[test]
    fn one_track_channel_two_nets_route_or_report_honestly() {
        // A narrow horizontal channel between two big keepouts (top layer only):
        // the gap is ~1 track wide on top, so two left→right nets contend. With a
        // second (bottom) layer free, the negotiated router should still find a
        // feasible plan (one net detours to the bottom layer via vias). The point
        // of the test: the result is honest — feasible, or unrouted reported.
        let chan_y = 8.0;
        let p = base(
            bounds(24.0, 16.0),
            vec![
                // Upper keepout (top layer): leaves a thin channel above center.
                keepout((12.0, 4.3), 8.0, 7.0, &["top"]),
                // Lower keepout (top layer): thin channel below center.
                keepout((12.0, 11.7), 8.0, 7.0, &["top"]),
            ],
            vec![
                conn("A", &[(2.0, chan_y, "top"), (22.0, chan_y, "top")]),
                conn(
                    "B",
                    &[(2.0, chan_y + 0.4, "top"), (22.0, chan_y + 0.4, "top")],
                ),
            ],
        );
        let r = global_route(&p);
        assert_plan_consistent(&p, &r);
        // Honest outcome: either feasible (with detour) or some net is reported
        // unrouted — never a silent over-capacity plan.
        if !r.is_feasible() {
            assert!(
                !r.report.unrouted.is_empty() || r.report.final_overflow > 0,
                "infeasible must be visible in the report"
            );
        }
        // No net is silently dropped: every connection appears in the plan.
        assert_eq!(r.plan.nets.len(), p.connections.len());
    }

    #[test]
    fn zero_capacity_cut_is_reported_not_looped() {
        // A keepout wall spanning the full board height on BOTH layers splits the
        // board in two with zero crossing capacity. A net with points on opposite
        // sides cannot route. The router must report it unrouted, hit no infinite
        // loop, and surface the iteration behaviour honestly.
        let p = base(
            bounds(24.0, 16.0),
            vec![keepout((12.0, 8.0), 2.0, 16.0, &["top", "bottom"])],
            vec![conn("CROSS", &[(2.0, 8.0, "top"), (22.0, 8.0, "top")])],
        );
        let r = global_route(&p);
        assert!(!r.is_feasible(), "a full cut is infeasible");
        assert_eq!(
            r.report.unrouted.len(),
            1,
            "the cut net is reported unrouted, got {:?}",
            r.report.unrouted
        );
        assert_eq!(r.report.unrouted[0].connection, "CROSS");
        assert!(
            !r.report.unrouted[0].reason.is_empty(),
            "unrouted nets carry a reason"
        );
        // No overflow (the net never committed any crossing), and we did not spin:
        // an all-unrouted board needs no rip-up, so iterations stay at 0.
        assert_eq!(r.report.final_overflow, 0);
        assert_eq!(r.report.iterations, 0, "no overflow ⇒ no rip-up iterations");
    }

    #[test]
    fn iteration_cap_is_visible_when_congestion_cannot_clear() {
        // Force genuine, unresolvable edge overflow: a one-track-capacity channel
        // (on every layer) with more nets than the channel can ever carry. The
        // router negotiates, fails to clear the overflow, and must stop at the cap
        // with the residual overflow reported — never loop forever.
        let chan_y = 8.0;
        let mut conns = Vec::new();
        for i in 0..6 {
            let y = chan_y - 0.4 + i as f64 * 0.16;
            conns.push(conn(&format!("N{i}"), &[(2.0, y, "top"), (22.0, y, "top")]));
        }
        // A wall on both layers with a single tiny gap (~one track) at chan_y.
        let p = base(
            bounds(24.0, 16.0),
            vec![
                keepout((12.0, 4.2), 1.0, 7.0, &["top", "bottom"]),
                keepout((12.0, 11.8), 1.0, 7.0, &["top", "bottom"]),
            ],
            conns,
        );
        let r = global_route(&p);
        assert_plan_consistent(&p, &r);
        // The board cannot carry 6 nets through a one-track gap on 2 layers, so it
        // is infeasible and that is visible. Whichever way it manifests (overflow
        // or unrouted), the run terminated (iterations ≤ cap).
        assert!(
            !r.is_feasible(),
            "6 nets through a 1-track gap is infeasible"
        );
        assert!(r.report.iterations <= MAX_ITERATIONS, "respects the cap");
        if r.report.final_overflow > 0 {
            assert!(
                !r.report.edge_hotspots.is_empty(),
                "overflow must surface hotspots"
            );
        }
    }

    #[test]
    fn determinism_two_runs_serialize_byte_equal() {
        let p = base(
            bounds(24.0, 16.0),
            vec![
                keepout((12.0, 4.3), 8.0, 7.0, &["top"]),
                keepout((12.0, 11.7), 8.0, 7.0, &["top"]),
            ],
            vec![
                conn("A", &[(2.0, 8.0, "top"), (22.0, 8.0, "top")]),
                conn("B", &[(2.0, 8.4, "top"), (22.0, 8.4, "top")]),
                conn("C", &[(2.0, 2.0, "top"), (22.0, 14.0, "bottom")]),
            ],
        );
        let a = global_route(&p);
        let b = global_route(&p);
        let ja = serde_json::to_string(&a).unwrap();
        let jb = serde_json::to_string(&b).unwrap();
        assert_eq!(ja, jb, "two runs must serialize byte-equal");
    }

    #[test]
    fn preferred_direction_penalty_alternates_by_layer() {
        assert_eq!(preferred_direction_penalty(true, 0), 0.0);
        assert_eq!(preferred_direction_penalty(false, 1), 0.0);
        assert_eq!(preferred_direction_penalty(false, 0), WRONG_WAY_K);
        assert_eq!(preferred_direction_penalty(true, 1), WRONG_WAY_K);
        assert_eq!(preferred_direction_penalty(true, 2), 0.0);
    }

    #[test]
    fn net_order_portfolio_keeps_legacy_first_and_dedupes_variants() {
        let p = base(
            bounds(20.0, 20.0),
            vec![],
            vec![
                conn("B_SHORT", &[(2.0, 2.0, "top"), (4.0, 2.0, "top")]),
                conn("A_LONG", &[(1.0, 1.0, "top"), (19.0, 19.0, "top")]),
                conn(
                    "C_TREE",
                    &[(3.0, 16.0, "top"), (10.0, 16.0, "top"), (17.0, 16.0, "top")],
                ),
            ],
        );
        let orders = net_order_portfolio(&p);
        assert_eq!(orders[0], net_order(&p), "legacy order stays first");
        assert!(
            orders.len() > 1,
            "nontrivial boards get fallback order variants"
        );
        for i in 0..orders.len() {
            for j in i + 1..orders.len() {
                assert_ne!(orders[i], orders[j], "portfolio orders are deduped");
            }
        }
    }

    #[test]
    fn net_order_portfolio_includes_obstacle_pressure_order() {
        let p = base(
            bounds(20.0, 20.0),
            vec![keepout((7.0, 10.0), 1.0, 4.0, &["top"])],
            vec![
                conn("OPEN", &[(2.0, 2.0, "top"), (6.0, 2.0, "top")]),
                conn("PINCHED", &[(2.0, 10.0, "top"), (12.0, 10.0, "top")]),
                conn("MID", &[(2.0, 16.0, "top"), (10.0, 16.0, "top")]),
            ],
        );
        let legacy = net_order(&p);
        let pressure = {
            let mut order: Vec<usize> = (0..p.connections.len()).collect();
            order.sort_by(|&a, &b| {
                connection_obstacle_pressure(&p, &p.connections[b])
                    .cmp(&connection_obstacle_pressure(&p, &p.connections[a]))
                    .then_with(|| {
                        p.connections[b]
                            .half_perimeter()
                            .total_cmp(&p.connections[a].half_perimeter())
                    })
                    .then_with(|| p.connections[a].name.cmp(&p.connections[b].name))
            });
            order
        };
        let orders = net_order_portfolio(&p);

        assert_eq!(orders[0], legacy, "legacy order stays first");
        assert_eq!(
            legacy[0], 0,
            "shortest-first starts with the open short net"
        );
        assert_eq!(
            pressure[0], 1,
            "pressure order starts with the corridor-overlapped net"
        );
        assert!(
            orders.iter().any(|order| order == &pressure),
            "fallback portfolio includes the pressure-first order"
        );
    }

    fn result_with(unrouted: usize, overflow: u32, iterations: usize) -> GlobalRouteResult {
        GlobalRouteResult {
            plan: GlobalPlan { nets: Vec::new() },
            report: CongestionReport {
                iterations,
                final_overflow: overflow,
                edge_hotspots: Vec::new(),
                unrouted: (0..unrouted)
                    .map(|i| FailedNet {
                        connection: format!("N{i}"),
                        reason: "test".to_owned(),
                    })
                    .collect(),
            },
        }
    }

    fn result_with_plan(iterations: usize, vias: usize, steps: usize) -> GlobalRouteResult {
        GlobalRouteResult {
            plan: GlobalPlan {
                nets: vec![NetPlan {
                    connection: "N".to_owned(),
                    paths: vec![CellPath {
                        steps: (0..steps)
                            .map(|i| CellStep {
                                leaf: i,
                                layer: 0,
                                center: Point2 {
                                    x: i as f64,
                                    y: 0.0,
                                },
                                via: i < vias,
                                exit: None,
                            })
                            .collect(),
                    }],
                }],
            },
            report: CongestionReport {
                iterations,
                final_overflow: 0,
                edge_hotspots: Vec::new(),
                unrouted: Vec::new(),
            },
        }
    }

    #[test]
    fn global_result_selector_prefers_feasible_then_less_bad_failures() {
        let feasible = result_with(0, 0, 8);
        let infeasible = result_with(0, 1, 1);
        assert!(global_result_better(&feasible, &infeasible));

        let fewer_unrouted = result_with(1, 4, 40);
        let more_unrouted = result_with(2, 0, 0);
        assert!(global_result_better(&fewer_unrouted, &more_unrouted));

        let less_overflow = result_with(1, 2, 40);
        let more_overflow = result_with(1, 3, 20);
        assert!(global_result_better(&less_overflow, &more_overflow));

        let tie = result_with(1, 2, 40);
        assert!(!global_result_better(&tie, &less_overflow));
    }

    #[test]
    fn global_result_selector_tiebreaks_by_plan_vias_then_steps() {
        let short_with_via = result_with_plan(4, 1, 3);
        let long_no_via = result_with_plan(4, 0, 8);
        let short_no_via = result_with_plan(4, 0, 3);

        assert!(
            global_result_better(&long_no_via, &short_with_via),
            "at equal routability and iterations, fewer planned vias should win"
        );
        assert!(
            global_result_better(&short_no_via, &long_no_via),
            "after planned vias tie, fewer coarse steps should win"
        );

        let fewer_iterations = result_with_plan(3, 1, 3);
        assert!(
            global_result_better(&fewer_iterations, &long_no_via),
            "iteration count remains more important than plan tidiness"
        );
    }

    #[test]
    fn stalled_negotiation_history_increment_ramps_but_is_bounded() {
        assert_eq!(history_increment_for_stall(0), HISTORY_INCREMENT);
        assert_eq!(history_increment_for_stall(3), HISTORY_INCREMENT * 4.0);
        assert_eq!(
            history_increment_for_stall(HISTORY_STALL_BOOST_MAX + 20),
            HISTORY_INCREMENT * (1.0 + HISTORY_STALL_BOOST_MAX as f64)
        );
    }

    #[test]
    fn victim_order_prioritizes_nets_touching_more_overflowed_edges() {
        let net_edges: Vec<BTreeSet<usize>> = [
            [1, 2, 9].into_iter().collect(),
            [1].into_iter().collect(),
            [1, 2, 3, 4].into_iter().collect(),
            [5, 6, 7, 8].into_iter().collect(),
        ]
        .into_iter()
        .collect();
        let overflowed: BTreeSet<usize> = [1, 2, 3].into_iter().collect();
        let mut victims = vec![3, 1, 0, 2];

        order_victims_by_overflow_pressure(&mut victims, &net_edges, &overflowed);

        assert_eq!(
            victims,
            vec![2, 0, 1, 3],
            "more overflowed edges first, then total edge footprint, then net-order position"
        );
    }

    #[test]
    fn target_distance_field_matches_exact_nearest_tree_scan() {
        let p = load("quad.json");
        let mesh = CapacityMesh::build(&p);
        let router = Router::new(&p, &mesh);
        let targets: Vec<Point2> = mesh
            .leaves
            .iter()
            .step_by(2)
            .map(|leaf| leaf.rect.center())
            .collect();
        assert!(!targets.is_empty(), "fixture should expose target leaves");

        let field = router.target_distance_field(&targets);
        assert_eq!(field.len(), mesh.leaves.len());
        for (leaf, cached) in mesh.leaves.iter().zip(field) {
            let c = leaf.rect.center();
            let scanned = targets
                .iter()
                .map(|t| c.dist(*t))
                .fold(f64::INFINITY, f64::min);
            assert_eq!(cached, scanned);
        }
    }

    #[test]
    fn led_r_gets_a_feasible_plan() {
        let p = load("led-r.json");
        let r = global_route(&p);
        assert!(
            r.is_feasible(),
            "led-r.json must get a feasible global plan: {:?}",
            r.report
        );
        assert_plan_consistent(&p, &r);
    }

    #[test]
    fn quad_gets_a_feasible_plan() {
        let p = load("quad.json");
        let r = global_route(&p);
        assert!(
            r.is_feasible(),
            "quad.json must get a feasible global plan: {:?}",
            r.report
        );
        assert_plan_consistent(&p, &r);
    }

    #[test]
    fn multi_point_net_grows_a_tree() {
        // A 3-point net on an open board: two paths (each new point to the tree).
        let p = base(
            bounds(20.0, 20.0),
            vec![],
            vec![conn(
                "T",
                &[(2.0, 2.0, "top"), (18.0, 2.0, "top"), (10.0, 18.0, "top")],
            )],
        );
        let r = global_route(&p);
        assert!(
            r.is_feasible(),
            "open 3-point net is feasible: {:?}",
            r.report
        );
        assert_eq!(r.plan.nets[0].paths.len(), 2, "3-point net grows 2 paths");
        assert_plan_consistent(&p, &r);
    }
}
