//! Boundary crossing assignment: the first detailed-routing stage (slice 3).
//!
//! The slice-2 [`crate::pathing::GlobalPlan`] is a coarse, *cell-level*
//! plan: each net crosses a sequence of mesh-edge boundaries, but every crossing
//! carries only a placeholder coordinate — the shared-boundary **midpoint** (see
//! [`Crossing::at`](crate::pathing::Crossing)). If the detailed router took those
//! midpoints literally, every net crossing the same boundary would aim for the
//! same point and clash. This stage replaces each placeholder with a *concrete*
//! crossing point, spread along the boundary so that no two crossings on the same
//! boundary/layer come closer than one track pitch, and ordered to minimise the
//! tangle inside each cell.
//!
//! The product is a [`CrossingAssignment`]: a per-cell list of [`CellJob`] work
//! orders, each holding the leaf's terminals (pad points, boundary entry/exit
//! points, via sites) with mm positions and layers — exactly what the per-cell
//! router ([`crate::detail`], Task 2) iterates over. It is deliberately
//! independent of [`crate::grid`] / [`crate::astar`]; it consumes the public
//! [`CapacityMesh`] / [`GlobalPlan`] API and the [`RouteProblem`] obstacle model.
//!
//! ## Algorithm
//!
//! 1. **Gather crossings.** Walk every net's cell paths. Each non-final step has
//!    an `exit` `Crossing`; that is one *use* of a mesh edge on a layer, shared
//!    between the step's leaf (the exit side) and the neighbour (the entry side of
//!    the next step). We record, per `(edge, layer)`, every using net together
//!    with the two anchor points that decide where on the boundary the net "wants"
//!    to cross: the centre of the cell it is leaving and the centre of the cell it
//!    is entering (the coarse waypoints on either side).
//!
//! 2. **Order along the boundary.** A shared boundary runs along one axis (a
//!    vertical edge varies in `y`, a horizontal one in `x`). For each using net we
//!    project its *other* anchor onto that axis — the mean of the two cell centres'
//!    coordinate along the boundary axis — and sort the nets by it (deterministic
//!    tie-break by net name). Nets whose endpoints sit lower on the boundary get
//!    the lower slots, so crossings do not cross each other inside either cell.
//!
//! 3. **Place on the unblocked portion.** The boundary minus the obstacle-covered
//!    sub-intervals (foreign copper / keepouts straddling the boundary line on that
//!    layer) is the usable region. Slots are centred on the largest usable
//!    sub-interval, spaced at the **track pitch** (`min_trace_width + clearance`,
//!    the same unit the mesh capacity is measured in). If the usable region cannot
//!    hold the slots at ≥ track pitch, the assignment fails with
//!    [`AssignmentFailure::Overflow`] — slice 2 guarantees usage ≤ capacity, so a
//!    genuine overflow here means the boundary is blocked unevenly and is reported
//!    honestly, never squeezed below clearance.
//!
//! 4. **Via sites.** A `via: true` step needs a via location inside its leaf,
//!    clear of foreign copper (problem obstacles inflated by `clearance + via
//!    radius`). Default is the leaf centre; if that collides, a deterministic
//!    spiral over detailed-grid offsets nudges it until it fits. Failure is
//!    reported ([`AssignmentFailure::ViaSite`]), never panicked.
//!
//! ## Determinism
//!
//! Net/edge/leaf iteration is over dense vectors and sorted keys; slot ordering
//! breaks ties by net name; the spiral is a fixed sequence. Two runs serialize
//! byte-for-byte (a determinism test asserts this).

use crate::grid::grid_pitch;
use crate::mesh::{CapacityMesh, LeafId};
use crate::pathing::GlobalPlan;
use crate::problem::Rect;
use crate::problem::{LayerRef, Point2, RouteProblem};
use geom::{BoundaryAxis, STRICT_EPS, SharedBoundary};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ── design constants ─────────────────────────────────────────────────────────

/// Geometric slop (mm) for "on the boundary" / interval comparisons.
const EPS: f64 = STRICT_EPS;

/// How many detailed-grid rings the via-site spiral searches before giving up.
/// At the detailed pitch a leaf is at most a handful of pitches across, so a
/// generous ring count still terminates quickly; failure is reported, not looped.
const VIA_SPIRAL_MAX_RING: i32 = 64;

// ── public result types ──────────────────────────────────────────────────────

/// What a terminal in a [`CellJob`] represents — which copper feature the
/// per-cell router must connect at this point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TerminalKind {
    /// A net pad / route point that lies inside this leaf (a true endpoint).
    Pad,
    /// The point where the net enters this leaf from the previous cell.
    Entry,
    /// The point where the net leaves this leaf toward the next cell.
    Exit,
    /// A via site: a layer change happens here inside the leaf.
    Via,
}

/// One terminal of a per-cell routing job: a point the per-cell router must
/// reach, with its layer and what it represents.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Terminal {
    /// What this terminal is (pad / entry / exit / via).
    pub kind: TerminalKind,
    /// Position (mm, y-down).
    pub at: Point2,
    /// The copper layer the terminal sits on. For a [`TerminalKind::Via`] this is
    /// the layer the path is *on* when the via is placed (the entry layer of the
    /// via step); the via itself joins all layers.
    pub layer: LayerRef,
}

/// A per-cell work order: everything the detailed router needs to route one
/// net's traversal of one leaf.
///
/// A net that merely passes through a leaf has an [`TerminalKind::Entry`] and an
/// [`TerminalKind::Exit`]; a net that starts/ends here has its
/// `TerminalKind::Pad`; a layer change inside the leaf adds a
/// [`TerminalKind::Via`]. The router connects all terminals in a job into copper.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CellJob {
    /// The leaf this job routes inside.
    pub leaf: LeafId,
    /// The connection (net) name this job belongs to.
    pub connection: String,
    /// The terminals to connect, in a deterministic order (entries, then exits,
    /// then vias, then pads — see [`assign_crossings`]).
    pub terminals: Vec<Terminal>,
}

/// The concrete crossing point assigned to one plan `Crossing`, replacing its
/// default midpoint. Carried separately from the cell jobs so Task 3 can stitch
/// per-net polylines across cells by matching `(net, edge, layer)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AssignedCrossing {
    /// The connection (net) name.
    pub connection: String,
    /// Index of the net's cell path within its [`NetPlan`](crate::pathing::NetPlan).
    pub path: usize,
    /// Index of the step within that path whose `exit` this crossing belongs to.
    pub step: usize,
    /// The mesh edge (index into [`CapacityMesh::edges`]) crossed.
    pub edge: usize,
    /// The copper layer of the crossing.
    pub layer: usize,
    /// The concrete crossing point (mm) on the shared boundary.
    pub at: Point2,
}

/// Why crossing assignment failed (reported, never panicked).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum AssignmentFailure {
    /// A boundary's usable (unblocked) length could not hold the required
    /// crossing slots at ≥ track pitch without squeezing below clearance.
    Overflow {
        /// The mesh edge index whose boundary overflowed.
        edge: usize,
        /// The layer that overflowed.
        layer: usize,
        /// Number of crossings that needed a slot on this boundary/layer.
        needed: usize,
        /// Number of slots the usable length could actually hold at track pitch.
        available: usize,
    },
    /// No via site clear of foreign copper was found inside a leaf.
    ViaSite {
        /// The connection whose via could not be placed.
        connection: String,
        /// The leaf the via had to sit in.
        leaf: LeafId,
    },
}

/// The output of [`assign_crossings`]: per-cell work orders, the concrete
/// crossing points, and any honest failures.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CrossingAssignment {
    /// Per-cell work orders, in `(leaf, net-order)` order.
    pub jobs: Vec<CellJob>,
    /// Every concrete crossing point, in `(net, path, step)` order, so Task 3 can
    /// match a leaf's exit to its neighbour's entry by `(edge, layer)` + net.
    pub crossings: Vec<AssignedCrossing>,
    /// Failures encountered (empty ⇒ a clean assignment). Reported, not panicked.
    pub failures: Vec<AssignmentFailure>,
}

impl CrossingAssignment {
    /// A clean assignment has no failures.
    pub fn is_clean(&self) -> bool {
        self.failures.is_empty()
    }
}

// ── entry point ──────────────────────────────────────────────────────────────

/// Assign concrete boundary crossings and per-cell work orders for `plan`.
///
/// `plan` is the slice-2 [`GlobalPlan`] over `mesh` for `problem`. Never panics;
/// failures (overflow, unplaceable via) are collected in
/// [`CrossingAssignment::failures`].
pub fn assign_crossings(
    problem: &RouteProblem,
    mesh: &CapacityMesh,
    plan: &GlobalPlan,
) -> CrossingAssignment {
    let track_pitch = mesh.track_pitch;
    let layer_count = mesh.layer_count.max(1);

    // 1. Gather every crossing use, keyed by (edge, layer), in plan order.
    //    Each use records the net, its path/step location, and the two anchors
    //    (exit-side cell centre, entry-side cell centre) that order it.
    let mut uses: BTreeMap<(usize, usize), Vec<CrossingUse>> = BTreeMap::new();
    for (net_pos, net) in plan.nets.iter().enumerate() {
        for (pi, path) in net.paths.iter().enumerate() {
            for (si, step) in path.steps.iter().enumerate() {
                let Some(x) = &step.exit else { continue };
                let exit_center = step.center.clone();
                let entry_center = path.steps[si + 1].center.clone();
                uses.entry((x.edge, x.layer))
                    .or_default()
                    .push(CrossingUse {
                        net_pos,
                        connection: net.connection.clone(),
                        path: pi,
                        step: si,
                        exit_center,
                        entry_center,
                    });
            }
        }
    }

    let mut failures: Vec<AssignmentFailure> = Vec::new();
    // Concrete crossing point keyed by (net_pos, path, step) for job assembly,
    // plus the flat `crossings` list (filled in net/path/step order at the end).
    let mut point_of: BTreeMap<(usize, usize, usize), CrossingPoint> = BTreeMap::new();

    for ((edge, layer), mut group) in uses {
        let e = &mesh.edges[edge];
        let Some(boundary) = mesh.leaves[e.a]
            .rect
            .shared_boundary(&mesh.leaves[e.b].rect)
        else {
            // Defensive: a plan edge must join abutting leaves. If not, skip —
            // the lint will catch any resulting disconnect; we do not panic.
            continue;
        };

        // Order the nets along the boundary by the projection of their anchors
        // onto the boundary axis (mean of the two cell centres' along-coordinate),
        // tie-break by connection name for determinism.
        group.sort_by(|p, q| {
            let kp = p.projection(boundary.axis);
            let kq = q.projection(boundary.axis);
            kp.total_cmp(&kq)
                .then_with(|| p.connection.cmp(&q.connection))
                .then_with(|| (p.net_pos, p.path, p.step).cmp(&(q.net_pos, q.path, q.step)))
        });

        // Usable sub-intervals of the boundary (boundary minus obstacle coverage
        // on this layer). Place the slots on the largest usable sub-interval.
        let usable = usable_intervals(problem, mesh, &boundary, layer);
        let slots = place_slots(&usable, group.len(), track_pitch);

        match slots {
            Some(coords) => {
                for (u, coord) in group.iter().zip(coords) {
                    let at = boundary.point_at(coord);
                    point_of.insert(
                        (u.net_pos, u.path, u.step),
                        CrossingPoint { edge, layer, at },
                    );
                }
            }
            None => {
                let available = max_slots(&usable, track_pitch);
                failures.push(AssignmentFailure::Overflow {
                    edge,
                    layer,
                    needed: group.len(),
                    available,
                });
                // Still place what we can (centred fallback) so downstream stages
                // have *a* point per crossing; the failure flags it as unsound.
                let coords = place_slots_fallback(&boundary, group.len());
                for (u, coord) in group.iter().zip(coords) {
                    let at = boundary.point_at(coord);
                    point_of.insert(
                        (u.net_pos, u.path, u.step),
                        CrossingPoint { edge, layer, at },
                    );
                }
            }
        }
    }

    // 2. Build the flat `crossings` list in (net, path, step) order.
    let mut crossings: Vec<AssignedCrossing> = Vec::new();
    for (net_pos, net) in plan.nets.iter().enumerate() {
        for (pi, path) in net.paths.iter().enumerate() {
            for (si, step) in path.steps.iter().enumerate() {
                if step.exit.is_none() {
                    continue;
                }
                if let Some(cp) = point_of.get(&(net_pos, pi, si)) {
                    crossings.push(AssignedCrossing {
                        connection: net.connection.clone(),
                        path: pi,
                        step: si,
                        edge: cp.edge,
                        layer: cp.layer,
                        at: cp.at.clone(),
                    });
                }
            }
        }
    }

    // 3. Build per-cell jobs. A job collects, per (leaf, net), the terminals the
    //    cell router must connect: entries, exits, vias, and pads inside the leaf.
    let obstacles = ForeignCopper::build(problem, mesh.layer_count.max(1));
    let via_radius = problem.via_diameter / 2.0;
    let via_clearance = problem.clearance + via_radius;
    let detail_pitch = grid_pitch(problem);

    // (leaf, net_pos) → terminals, kept in a BTreeMap for deterministic order.
    let mut job_terminals: BTreeMap<(LeafId, usize), JobAcc> = BTreeMap::new();

    for (net_pos, net) in plan.nets.iter().enumerate() {
        for (pi, path) in net.paths.iter().enumerate() {
            for (si, step) in path.steps.iter().enumerate() {
                let acc = job_terminals
                    .entry((step.leaf, net_pos))
                    .or_insert_with(|| JobAcc::new(net.connection.clone()));

                // Entry into this leaf = previous step's exit crossing point
                // (reversed orientation). The previous step is in a *different*
                // leaf, so the entry sits on the same boundary point.
                if let Some(cp) = si
                    .checked_sub(1)
                    .and_then(|prev| point_of.get(&(net_pos, pi, prev)))
                {
                    acc.terminals.push(Terminal {
                        kind: TerminalKind::Entry,
                        at: cp.at.clone(),
                        layer: layer_ref(cp.layer, layer_count),
                    });
                }

                // Exit out of this leaf = this step's exit crossing point. (This
                // step has an exit iff a concrete point was assigned for it.)
                if let Some(cp) = point_of.get(&(net_pos, pi, si)) {
                    acc.terminals.push(Terminal {
                        kind: TerminalKind::Exit,
                        at: cp.at.clone(),
                        layer: layer_ref(cp.layer, layer_count),
                    });
                }

                // Via site: a layer change inside this leaf. Place it clear of
                // foreign copper; the via step's `layer` is the entry layer.
                if step.via {
                    let leaf_rect = &mesh.leaves[step.leaf].rect;
                    match place_via(leaf_rect, net_pos, &obstacles, via_clearance, detail_pitch) {
                        Some(at) => acc.terminals.push(Terminal {
                            kind: TerminalKind::Via,
                            at,
                            layer: layer_ref(step.layer, layer_count),
                        }),
                        None => failures.push(AssignmentFailure::ViaSite {
                            connection: net.connection.clone(),
                            leaf: step.leaf,
                        }),
                    }
                }
            }
        }
    }

    // 4. Pad terminals: a net's route points belong in the leaf containing them.
    let name_index = name_index(problem);
    for (net_pos, net) in plan.nets.iter().enumerate() {
        // Resolve the connection's own pad points to their leaves. Skip nets the
        // plan did not route (no paths) — their pads have no cell job to join.
        if net.paths.is_empty() {
            continue;
        }
        let Some(&ci) = name_index.get(&net.connection) else {
            continue;
        };
        // ci is the connection index; it equals net_pos only if plan net order
        // matches connections order, which it need not. Use ci for the problem.
        let conn = &problem.connections[ci];
        for pt in &conn.points_to_connect {
            let p = Point2 { x: pt.x, y: pt.y };
            let leaf = mesh.cell_at(&p);
            let layer = pt
                .layer
                .index(layer_count as u32)
                .unwrap_or(0)
                .min(layer_count as u32 - 1) as usize;
            let acc = job_terminals
                .entry((leaf, net_pos))
                .or_insert_with(|| JobAcc::new(net.connection.clone()));
            acc.terminals.push(Terminal {
                kind: TerminalKind::Pad,
                at: p,
                layer: layer_ref(layer, layer_count),
            });
        }
    }

    let jobs: Vec<CellJob> = job_terminals
        .into_iter()
        .map(|((leaf, _net_pos), acc)| CellJob {
            leaf,
            connection: acc.connection,
            terminals: acc.terminals,
        })
        .collect();

    CrossingAssignment {
        jobs,
        crossings,
        failures,
    }
}

// ── internals ────────────────────────────────────────────────────────────────

/// One use of a mesh edge by a net at a particular path/step.
struct CrossingUse {
    net_pos: usize,
    connection: String,
    path: usize,
    step: usize,
    exit_center: Point2,
    entry_center: Point2,
}

impl CrossingUse {
    /// The along-boundary coordinate the net "wants" to cross at: the mean of the
    /// two adjacent cell centres projected onto the boundary axis. Ordering nets
    /// by this minimises intra-cell crossing tangle.
    fn projection(&self, axis: BoundaryAxis) -> f64 {
        match axis {
            // Vertical boundary varies in y; project the cell centres' y.
            BoundaryAxis::Vertical => (self.exit_center.y + self.entry_center.y) / 2.0,
            // Horizontal boundary varies in x; project the cell centres' x.
            BoundaryAxis::Horizontal => (self.exit_center.x + self.entry_center.x) / 2.0,
        }
    }
}

/// The concrete point chosen for one crossing.
struct CrossingPoint {
    edge: usize,
    layer: usize,
    at: Point2,
}

/// Accumulator for one cell job's terminals while building.
struct JobAcc {
    connection: String,
    terminals: Vec<Terminal>,
}

impl JobAcc {
    fn new(connection: String) -> Self {
        JobAcc {
            connection,
            terminals: Vec::new(),
        }
    }
}

/// The usable (unblocked) sub-intervals `[lo, hi]` of a boundary on `layer`: the
/// full span minus the obstacle-covered portions that straddle the boundary
/// line. Mirrors `mesh::edge_capacity`'s coverage logic, but keeps the gaps.
fn usable_intervals(
    problem: &RouteProblem,
    mesh: &CapacityMesh,
    boundary: &SharedBoundary,
    layer: usize,
) -> Vec<(f64, f64)> {
    let layer_count = mesh.layer_count.max(1);
    // Collect covered sub-intervals of [lo, hi].
    let mut covered: Vec<(f64, f64)> = Vec::new();
    for ob in &problem.obstacles {
        // Only obstacles present on this copper layer block the boundary.
        let on_layer = ob
            .layers
            .iter()
            .any(|l| l.index(layer_count as u32) == Some(layer as u32));
        if !on_layer {
            continue;
        }
        let hw = ob.width / 2.0;
        let hh = ob.height / 2.0;
        let (min_x, max_x) = (ob.center.x - hw, ob.center.x + hw);
        let (min_y, max_y) = (ob.center.y - hh, ob.center.y + hh);
        let (on_line, seg_lo, seg_hi) = match boundary.axis {
            BoundaryAxis::Vertical => (
                min_x <= boundary.coord && boundary.coord <= max_x,
                min_y.max(boundary.lo),
                max_y.min(boundary.hi),
            ),
            BoundaryAxis::Horizontal => (
                min_y <= boundary.coord && boundary.coord <= max_y,
                min_x.max(boundary.lo),
                max_x.min(boundary.hi),
            ),
        };
        if on_line && seg_hi > seg_lo {
            covered.push((seg_lo, seg_hi));
        }
    }
    subtract_intervals(boundary.lo, boundary.hi, &mut covered)
}

/// `[lo, hi]` minus the union of `covered` sub-intervals, as a sorted list of
/// open gaps (each with positive length).
fn subtract_intervals(lo: f64, hi: f64, covered: &mut [(f64, f64)]) -> Vec<(f64, f64)> {
    covered.sort_by(|p, q| p.0.total_cmp(&q.0));
    let mut gaps: Vec<(f64, f64)> = Vec::new();
    let mut cursor = lo;
    for &(c_lo, c_hi) in covered.iter() {
        let c_lo = c_lo.max(lo);
        let c_hi = c_hi.min(hi);
        if c_hi <= c_lo {
            continue;
        }
        if c_lo - cursor > EPS {
            gaps.push((cursor, c_lo));
        }
        cursor = cursor.max(c_hi);
    }
    if hi - cursor > EPS {
        gaps.push((cursor, hi));
    }
    gaps
}

/// Maximum slots that fit across a set of usable intervals at `pitch`: the sum,
/// per interval, of `floor(len / pitch) + 1` if the interval can hold a centred
/// slot at all (`len >= ~0`), but counting slots as the number of pitch-spaced
/// centres that fit. Concretely `floor(len / pitch)` slots per interval (a slot
/// needs a pitch of room around it), summed.
fn max_slots(usable: &[(f64, f64)], pitch: f64) -> usize {
    if pitch <= 0.0 {
        return 0;
    }
    usable
        .iter()
        .map(|&(lo, hi)| {
            let len = hi - lo;
            if len < pitch - EPS {
                // A sub-pitch gap still admits a single centred crossing.
                if len > EPS { 1 } else { 0 }
            } else {
                (len / pitch + EPS).floor() as usize
            }
        })
        .sum()
}

/// Place `n` crossing slots across the usable intervals, spaced ≥ `pitch`,
/// returning their along-axis coordinates in interval order. `None` if they do
/// not fit (the caller reports an overflow). Slots are packed into the usable
/// intervals largest-first by capacity, then the coordinates are returned sorted
/// so the i-th sorted net gets the i-th sorted slot (preserving the boundary
/// order computed by the caller).
fn place_slots(usable: &[(f64, f64)], n: usize, pitch: f64) -> Option<Vec<f64>> {
    if n == 0 {
        return Some(Vec::new());
    }
    if pitch <= 0.0 {
        return None;
    }
    // Per-interval slot capacity.
    let caps: Vec<usize> = usable
        .iter()
        .map(|&(lo, hi)| {
            let len = hi - lo;
            if len < pitch - EPS {
                if len > EPS { 1 } else { 0 }
            } else {
                (len / pitch + EPS).floor() as usize
            }
        })
        .collect();
    let total: usize = caps.iter().sum();
    if total < n {
        return None;
    }

    // Distribute n slots across intervals proportional to capacity, largest
    // remainder first — deterministic. Then centre that many slots in each
    // interval at exactly `pitch` spacing.
    let alloc = distribute(&caps, n);
    let mut coords: Vec<f64> = Vec::with_capacity(n);
    for (&(lo, hi), &k) in usable.iter().zip(&alloc) {
        if k == 0 {
            continue;
        }
        let len = hi - lo;
        let mid = (lo + hi) / 2.0;
        if k == 1 {
            coords.push(mid);
            continue;
        }
        // Spread the k slots across the usable interval rather than packing them at
        // the minimum pitch in the centre. Maximising mutual spacing (up to the room
        // available) gives a saturated boundary the margin the detailed router needs
        // to absorb its grid-snap distortion without two crossings' approach traces
        // converging below clearance. We keep an end margin (up to half a pitch) so
        // no slot hugs a boundary end — two boundaries meeting at a leaf corner would
        // otherwise each place a slot on that shared corner, stacking two foreign
        // nets' crossings. Spacing never drops below `pitch` (the interval holds k
        // slots at pitch by construction, so `len/(k-1) ≥ pitch`).
        let max_span = len - pitch; // leave ≥ half a pitch at each end when possible
        let min_span = (k as f64 - 1.0) * pitch; // tightest legal packing
        let span = max_span.max(min_span).min(len);
        let step = span / (k as f64 - 1.0);
        let start = (mid - span / 2.0).max(lo).min(hi - span).max(lo);
        for s in 0..k {
            coords.push(start + s as f64 * step);
        }
    }
    coords.sort_by(|p, q| p.total_cmp(q));
    Some(coords)
}

/// Distribute `n` units across buckets with the given capacities, largest
/// remainder first (deterministic, never exceeding a bucket's capacity).
fn distribute(caps: &[usize], n: usize) -> Vec<usize> {
    let total: usize = caps.iter().sum();
    if total == 0 {
        return vec![0; caps.len()];
    }
    let mut alloc: Vec<usize> = caps
        .iter()
        .map(|&c| n * c / total) // floor of proportional share
        .collect();
    let mut remaining = n - alloc.iter().sum::<usize>();
    // Give the leftover to buckets with the largest fractional remainder that
    // still have spare capacity; tie-break by index for determinism.
    let mut order: Vec<usize> = (0..caps.len()).collect();
    order.sort_by(|&i, &j| {
        let ri = (n * caps[i]) % total;
        let rj = (n * caps[j]) % total;
        rj.cmp(&ri).then_with(|| i.cmp(&j))
    });
    let mut oi = 0;
    while remaining > 0 && oi < order.len() * 2 {
        let idx = order[oi % order.len()];
        if alloc[idx] < caps[idx] {
            alloc[idx] += 1;
            remaining -= 1;
        }
        oi += 1;
    }
    alloc
}

/// Fallback slot placement when the boundary overflows: spread `n` points evenly
/// across the *full* boundary span (so every crossing still gets *a* point), even
/// though the overflow has already been flagged.
fn place_slots_fallback(boundary: &SharedBoundary, n: usize) -> Vec<f64> {
    if n == 0 {
        return Vec::new();
    }
    let (lo, hi) = (boundary.lo, boundary.hi);
    if n == 1 {
        return vec![(lo + hi) / 2.0];
    }
    (0..n)
        .map(|i| lo + (hi - lo) * (i as f64 + 0.5) / n as f64)
        .collect()
}

/// Foreign-copper geometry per layer: obstacle rects tagged with owning-net
/// indices, for via-site clearance checks. A via site must clear copper not its
/// own on every layer (vias are through-hole in v1).
struct ForeignCopper {
    /// `rects[layer]` = obstacles on that layer, each `(rect, owners)`.
    rects: Vec<Vec<(Rect, Vec<usize>)>>,
}

impl ForeignCopper {
    fn build(problem: &RouteProblem, layer_count: usize) -> Self {
        let name_index = name_index(problem);
        let mut rects: Vec<Vec<(Rect, Vec<usize>)>> = vec![Vec::new(); layer_count];
        for ob in &problem.obstacles {
            let hw = ob.width / 2.0;
            let hh = ob.height / 2.0;
            let rect = Rect {
                min_x: ob.center.x - hw,
                min_y: ob.center.y - hh,
                max_x: ob.center.x + hw,
                max_y: ob.center.y + hh,
            };
            let mut owners: Vec<usize> = ob
                .connected_to
                .iter()
                .filter_map(|n| name_index.get(n).copied())
                .collect();
            owners.sort_unstable();
            owners.dedup();
            for layer_ref in &ob.layers {
                if let Some(layer) = layer_ref.index(layer_count as u32) {
                    rects[layer as usize].push((rect, owners.clone()));
                }
            }
        }
        ForeignCopper { rects }
    }

    /// Is `p` clear of all copper foreign to net index `conn` by `clearance`, on
    /// every layer? A via is through-hole, so any layer's foreign copper blocks
    /// it. An obstacle owned (only) by `conn` does not block its own via.
    fn via_clear(&self, p: &Point2, conn: usize, clearance: f64) -> bool {
        for layer_rects in &self.rects {
            for (rect, owners) in layer_rects {
                // The net's own copper does not block its via.
                let foreign = owners.is_empty() || owners.iter().any(|&o| o != conn);
                if !foreign {
                    continue;
                }
                if rect.dist_to_point(*p) < clearance - EPS {
                    return false;
                }
            }
        }
        true
    }
}

/// Choose a via site inside `leaf_rect` clear of foreign copper by `clearance`.
/// Default is the leaf centre; if blocked, spiral outward at `pitch` offsets
/// (deterministic ring order) until a clear point is found, staying inside the
/// leaf. `None` if no clear site exists within the spiral bound.
fn place_via(
    leaf_rect: &Rect,
    conn: usize,
    obstacles: &ForeignCopper,
    clearance: f64,
    pitch: f64,
) -> Option<Point2> {
    let center = Point2 {
        x: (leaf_rect.min_x + leaf_rect.max_x) / 2.0,
        y: (leaf_rect.min_y + leaf_rect.max_y) / 2.0,
    };
    if obstacles.via_clear(&center, conn, clearance) {
        return Some(center);
    }
    let pitch = if pitch > 0.0 { pitch } else { 0.1 };
    for ring in 1..=VIA_SPIRAL_MAX_RING {
        // Walk the square ring at Chebyshev radius `ring`, in a fixed order:
        // increasing dy, then increasing dx, taking only the ring's perimeter.
        for dy in -ring..=ring {
            for dx in -ring..=ring {
                if dx.abs() != ring && dy.abs() != ring {
                    continue; // interior of the ring already visited
                }
                let p = Point2 {
                    x: center.x + dx as f64 * pitch,
                    y: center.y + dy as f64 * pitch,
                };
                // Stay strictly inside the leaf rect (a via centre on the leaf
                // boundary would belong to a neighbour; keep it interior).
                if p.x <= leaf_rect.min_x + EPS
                    || p.x >= leaf_rect.max_x - EPS
                    || p.y <= leaf_rect.min_y + EPS
                    || p.y >= leaf_rect.max_y - EPS
                {
                    continue;
                }
                if obstacles.via_clear(&p, conn, clearance) {
                    return Some(p);
                }
            }
        }
    }
    None
}

/// Connection name → dense index (connections order, first-wins) — mirrors
/// `mesh::name_index` / `grid`'s map so pad-leaf attribution matches the mesh.
fn name_index(problem: &RouteProblem) -> BTreeMap<String, usize> {
    let mut m = BTreeMap::new();
    for (i, c) in problem.connections.iter().enumerate() {
        m.entry(c.name.clone()).or_insert(i);
    }
    m
}

/// The [`LayerRef`] for a numeric copper layer index (0 = top, last = bottom,
/// between = `inner{n}`) — the inverse of [`LayerRef::index`]. Mirrors
/// `router::layer_ref` (private there) so terminals carry KiCAD-style names.
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

    /// Assert: every plan crossing got exactly one concrete point; spacing on
    /// every boundary/layer ≥ track pitch; points lie on the shared boundary; via
    /// sites clear foreign copper.
    fn assert_assignment_sound(p: &RouteProblem) {
        let mesh = CapacityMesh::build(p);
        let plan = global_route(p).plan;
        let a = assign_crossings(p, &mesh, &plan);

        // Count plan crossings.
        let mut plan_crossings = 0usize;
        for net in &plan.nets {
            for path in &net.paths {
                for step in &path.steps {
                    if step.exit.is_some() {
                        plan_crossings += 1;
                    }
                }
            }
        }
        assert_eq!(
            a.crossings.len(),
            plan_crossings,
            "every plan crossing gets exactly one concrete point"
        );
        assert!(a.is_clean(), "assignment must be clean: {:?}", a.failures);

        // Every concrete point lies on its edge's shared boundary (within EPS),
        // grouped per (edge, layer) for the spacing check.
        let track_pitch = mesh.track_pitch;
        let mut per_boundary: BTreeMap<(usize, usize), Vec<f64>> = BTreeMap::new();
        for x in &a.crossings {
            let e = &mesh.edges[x.edge];
            let b = mesh.leaves[e.a]
                .rect
                .shared_boundary(&mesh.leaves[e.b].rect)
                .expect("plan edge joins abutting leaves");
            let on = match b.axis {
                BoundaryAxis::Vertical => {
                    (x.at.x - b.coord).abs() < EPS && x.at.y >= b.lo - EPS && x.at.y <= b.hi + EPS
                }
                BoundaryAxis::Horizontal => {
                    (x.at.y - b.coord).abs() < EPS && x.at.x >= b.lo - EPS && x.at.x <= b.hi + EPS
                }
            };
            assert!(
                on,
                "crossing {} on edge {} must lie on the shared boundary (at {:?}, axis {:?}, coord {}, [{}, {}])",
                x.connection, x.edge, x.at, b.axis, b.coord, b.lo, b.hi
            );
            let t = match b.axis {
                BoundaryAxis::Vertical => x.at.y,
                BoundaryAxis::Horizontal => x.at.x,
            };
            per_boundary.entry((x.edge, x.layer)).or_default().push(t);
        }
        for ((edge, layer), mut ts) in per_boundary {
            ts.sort_by(|p, q| p.total_cmp(q));
            for w in ts.windows(2) {
                let gap = w[1] - w[0];
                assert!(
                    gap >= track_pitch - 1e-9,
                    "edge {edge} layer {layer}: spacing {gap} < track pitch {track_pitch}"
                );
            }
        }

        // Via sites clear of foreign copper.
        let layer_count = mesh.layer_count.max(1);
        let fc = ForeignCopper::build(p, layer_count);
        let ni = name_index(p);
        let via_clearance = p.clearance + p.via_diameter / 2.0;
        for job in &a.jobs {
            let ci = ni[&job.connection];
            for t in &job.terminals {
                if t.kind == TerminalKind::Via {
                    assert!(
                        fc.via_clear(&t.at, ci, via_clearance),
                        "via for {} in leaf {} at {:?} must clear foreign copper",
                        job.connection,
                        job.leaf,
                        t.at
                    );
                }
            }
        }
    }

    #[test]
    fn determinism_two_assignments_serialize_byte_equal() {
        let p = load("quad.json");
        let mesh = CapacityMesh::build(&p);
        let plan = global_route(&p).plan;
        let a = assign_crossings(&p, &mesh, &plan);
        let b = assign_crossings(&p, &mesh, &plan);
        let ja = serde_json::to_string(&a).unwrap();
        let jb = serde_json::to_string(&b).unwrap();
        assert_eq!(ja, jb, "two assignments must serialize byte-equal");
    }

    #[test]
    fn congested_assignment_is_sound() {
        assert_assignment_sound(&load("congested.json"));
    }

    #[test]
    fn quad_assignment_is_sound() {
        assert_assignment_sound(&load("quad.json"));
    }

    #[test]
    fn led_r_assignment_is_sound() {
        assert_assignment_sound(&load("led-r.json"));
    }

    #[test]
    fn every_crossing_has_a_terminal_pair_across_the_boundary() {
        // A simple T-junction-ish case: an off-centre keepout forces refinement,
        // and a net routes across the refined region. Each crossing point should
        // appear as an Exit in the leaving cell's job and an Entry in the entered
        // cell's job, at the same coordinate.
        let p = base(
            bounds(24.0, 16.0),
            vec![keepout((6.0, 4.0), 2.0, 2.0, &["top"])],
            vec![conn("N", &[(2.0, 8.0, "top"), (22.0, 8.0, "top")])],
        );
        let mesh = CapacityMesh::build(&p);
        let plan = global_route(&p).plan;
        let a = assign_crossings(&p, &mesh, &plan);
        assert!(a.is_clean(), "{:?}", a.failures);

        // For each crossing, there is an Exit terminal and an Entry terminal with
        // matching coordinates somewhere in the jobs.
        for x in &a.crossings {
            let exits = a.jobs.iter().flat_map(|j| &j.terminals).filter(|t| {
                t.kind == TerminalKind::Exit
                    && (t.at.x - x.at.x).abs() < 1e-9
                    && (t.at.y - x.at.y).abs() < 1e-9
            });
            assert!(exits.count() >= 1, "crossing has an exit terminal");
            let entries = a.jobs.iter().flat_map(|j| &j.terminals).filter(|t| {
                t.kind == TerminalKind::Entry
                    && (t.at.x - x.at.x).abs() < 1e-9
                    && (t.at.y - x.at.y).abs() < 1e-9
            });
            assert!(
                entries.count() >= 1,
                "crossing has a matching entry terminal"
            );
        }
    }

    #[test]
    fn pads_land_in_their_containing_leaf() {
        let p = base(
            bounds(24.0, 16.0),
            vec![keepout((6.0, 4.0), 2.0, 2.0, &["top"])],
            vec![conn("N", &[(2.0, 8.0, "top"), (22.0, 8.0, "top")])],
        );
        let mesh = CapacityMesh::build(&p);
        let plan = global_route(&p).plan;
        let a = assign_crossings(&p, &mesh, &plan);
        // Both pad points must show up as Pad terminals in the leaf containing them.
        for &(px, py) in &[(2.0, 8.0), (22.0, 8.0)] {
            let leaf = mesh.cell_at(&Point2 { x: px, y: py });
            let job = a
                .jobs
                .iter()
                .find(|j| j.leaf == leaf && j.connection == "N")
                .expect("a job exists for the pad's leaf");
            assert!(
                job.terminals.iter().any(|t| {
                    t.kind == TerminalKind::Pad
                        && (t.at.x - px).abs() < 1e-9
                        && (t.at.y - py).abs() < 1e-9
                }),
                "pad ({px},{py}) is a terminal in leaf {leaf}'s job"
            );
        }
    }

    #[test]
    fn slot_placement_spreads_and_respects_pitch() {
        // Three nets crossing one wide-open boundary should get three distinct
        // points spaced ≥ pitch, centred on the boundary.
        let usable = vec![(0.0, 10.0)];
        let pitch = 0.45;
        let coords = place_slots(&usable, 3, pitch).expect("three slots fit in 10mm");
        assert_eq!(coords.len(), 3);
        for w in coords.windows(2) {
            assert!(w[1] - w[0] >= pitch - 1e-9, "slots spaced ≥ pitch");
        }
    }

    #[test]
    fn overflow_is_reported_when_boundary_too_short() {
        // Two slots cannot fit in a sub-pitch usable interval.
        let usable = vec![(0.0, 0.1)];
        let pitch = 0.45;
        assert!(
            place_slots(&usable, 2, pitch).is_none(),
            "two slots do not fit in 0.1mm at pitch 0.45"
        );
    }

    #[test]
    fn via_site_nudges_off_foreign_copper() {
        // A leaf whose centre sits on a foreign keepout: the via must nudge clear.
        let leaf = Rect {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 8.0,
            max_y: 8.0,
        };
        let p = base(
            bounds(8.0, 8.0),
            vec![keepout((4.0, 4.0), 2.0, 2.0, &["top", "bottom"])],
            vec![conn("N", &[(1.0, 1.0, "top")])],
        );
        let fc = ForeignCopper::build(&p, 2);
        let via_clearance = p.clearance + p.via_diameter / 2.0;
        let site = place_via(&leaf, 0, &fc, via_clearance, grid_pitch(&p))
            .expect("a clear via site exists in the corner");
        assert!(
            fc.via_clear(&site, 0, via_clearance),
            "nudged via site clears the keepout"
        );
        // The centre itself was NOT clear (the keepout covers it).
        assert!(!fc.via_clear(&Point2 { x: 4.0, y: 4.0 }, 0, via_clearance));
    }
}
