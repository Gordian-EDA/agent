//! Quadtree capacity mesh: the global router's coarse model of the board.
//!
//! Slice 2's global stage cannot reason cell-by-cell over the slice-1 grid —
//! that is what defeats it on congested boards. Instead it builds a *capacity
//! mesh*: an XY quadtree over [`RouteProblem::bounds`] that subdivides only
//! where it must (around obstacle boundaries), so open board area stays one big
//! cheap leaf while pin fields refine down to fine leaves. Each leaf carries,
//! **per copper layer**, how many tracks can pass through it (its capacity) and
//! which nets' copper already covers it; leaves that share a boundary are joined
//! by an edge whose per-layer capacity is how many tracks can cross that shared
//! boundary. Slice-2 pathing (Task 2) routes nets over this graph,
//! congestion-costed.
//!
//! This module is deliberately independent of [`crate::grid`] / [`crate::astar`]
//! / [`crate::router`]: the slice-1 fallback path stays untouched and
//! always-correct. The mesh shares only the [`RouteProblem`] model and the
//! obstacle/net-attribution convention (a net's own pads never block it).
//!
//! ## Design constants
//!
//! - **track pitch** = `min_trace_width + clearance` — the centre-to-centre
//!   spacing of adjacent parallel tracks, hence the capacity unit (one track of
//!   width `min_trace_width` plus its clearance gutter). Note this is *twice*
//!   the slice-1 grid pitch, which is a rasterization step, not a track slot.
//! - **max depth**: the deepest level at which a leaf is still ≥ `4 × track
//!   pitch` on its shorter side, so the finest leaf still admits a handful of
//!   tracks. Computed from the bounds' shorter side.
//! - **subdivide** a cell while it straddles an obstacle *edge* (the obstacle's
//!   rectangle boundary passes through the cell — not mere containment, which
//!   would needlessly refine a leaf wholly inside a big keepout) and depth < max.
//!
//! ## Determinism
//!
//! Leaves are numbered in depth-first tree-path order (children always visited
//! NW, NE, SW, SE), so leaf ids are a pure function of the tree shape, which is
//! a pure function of the problem. Adjacency edges are produced by a single
//! deterministic sweep and sorted by `(min(a,b), max(a,b))`. No `HashMap`
//! iteration, no float-keyed ordering, leaks into the output — the slice's
//! determinism tests serialize the mesh twice and compare byte-for-byte.

use crate::problem::{Point2, RouteProblem};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A leaf cell's stable identifier (depth-first tree-path order, 0-based).
pub type LeafId = usize;

/// An axis-aligned rectangle in board mm (y-down), `[min, max]` per axis.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rect {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

impl Rect {
    #[inline]
    fn width(&self) -> f64 {
        self.max_x - self.min_x
    }
    #[inline]
    fn height(&self) -> f64 {
        self.max_y - self.min_y
    }
    #[inline]
    fn area(&self) -> f64 {
        self.width() * self.height()
    }
    #[inline]
    fn center(&self) -> Point2 {
        Point2 {
            x: (self.min_x + self.max_x) / 2.0,
            y: (self.min_y + self.max_y) / 2.0,
        }
    }
    /// Does this rect overlap `other` with positive area?
    #[inline]
    fn overlaps(&self, other: &Rect) -> bool {
        self.min_x < other.max_x
            && self.max_x > other.min_x
            && self.min_y < other.max_y
            && self.max_y > other.min_y
    }
    /// The overlap rectangle with `other`, or `None` if they do not overlap.
    fn intersection(&self, other: &Rect) -> Option<Rect> {
        let min_x = self.min_x.max(other.min_x);
        let max_x = self.max_x.min(other.max_x);
        let min_y = self.min_y.max(other.min_y);
        let max_y = self.max_y.min(other.max_y);
        if min_x < max_x && min_y < max_y {
            Some(Rect {
                min_x,
                min_y,
                max_x,
                max_y,
            })
        } else {
            None
        }
    }
    /// Does the *boundary* (any of the four edges) of `other` pass through the
    /// interior of `self`? True when the rects overlap but `other` does not
    /// wholly contain `self` — i.e. at least one edge of `other` cuts across.
    fn boundary_crosses(&self, other: &Rect) -> bool {
        if !self.overlaps(other) {
            return false;
        }
        // `other` covers `self` entirely ⇒ no edge of `other` is inside `self`.
        let covers = other.min_x <= self.min_x
            && other.max_x >= self.max_x
            && other.min_y <= self.min_y
            && other.max_y >= self.max_y;
        !covers
    }
}

/// A flattened obstacle footprint on a single layer: its rectangle plus which
/// connections own it (empty ⇒ keepout / foreign copper that blocks everyone).
#[derive(Debug, Clone)]
struct LayerObstacle {
    rect: Rect,
    /// Owning connection indices (dense, from `connections` order). Empty for a
    /// keepout. A leaf's capacity for a net is *not* cut by an obstacle the net
    /// owns.
    owners: Vec<usize>,
}

/// Per-layer state of one leaf.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeafLayer {
    /// Fraction of the leaf's area not covered by *blocking* copper, in
    /// `[0, 1]`. Blocking = keepouts + foreign copper; a net's own copper does
    /// not reduce this (capacity is net-relative via [`Leaf::blocking`]).
    pub free_fraction: f64,
    /// Track capacity through this leaf on this layer for a net that is *not*
    /// blocked here: `floor(min(w,h) / track_pitch)` scaled by `free_fraction`.
    /// This is the capacity a net sees when none of the covering copper is
    /// foreign to it; nets listed in [`Leaf::blocking`] for this layer see a
    /// capacity reduced (down to 0) by that foreign coverage — use
    /// [`Leaf::capacity_for`].
    pub capacity: u32,
}

/// One quadtree leaf: a board region with per-layer capacity and net blocking.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Leaf {
    /// Stable id (== index into [`CapacityMesh::leaves`]).
    pub id: LeafId,
    /// The leaf's board rectangle (mm).
    pub rect: Rect,
    /// Quadtree depth (root = 0).
    pub depth: u32,
    /// Per-copper-layer capacity state, indexed by layer (0 = top).
    pub layers: Vec<LeafLayer>,
    /// Per layer, the owning-connection indices of any net copper (pads)
    /// covering part of this leaf. A net whose index is *absent* from this list
    /// while the list is non-empty is blocked here (foreign copper it does not
    /// own sits in the leaf) and reads capacity 0; a net that owns all the
    /// copper present — or a leaf with no net copper — sees the full
    /// [`LeafLayer::capacity`]. Keepouts cut [`LeafLayer::capacity`] directly
    /// (they block everyone) and need no per-net entry. Sorted, deduped —
    /// deterministic.
    pub blocking: Vec<Vec<usize>>,
}

impl Leaf {
    /// The leaf's capacity on `layer` as seen by connection index `conn`.
    ///
    /// Foreign copper covering the leaf cuts `conn`'s capacity to 0 (the global
    /// router should not thread a net through a leaf already claimed by another
    /// net's pad); the net's *own* copper does not. Concretely: if any owner of
    /// the copper present in this leaf is not `conn`, return 0; otherwise the
    /// keepout-reduced [`LeafLayer::capacity`].
    pub fn capacity_for(&self, layer: usize, conn: usize) -> u32 {
        let Some(l) = self.layers.get(layer) else {
            return 0;
        };
        match self.blocking.get(layer) {
            // Foreign copper present iff some owner != conn.
            Some(b) if b.iter().any(|&owner| owner != conn) => 0,
            _ => l.capacity,
        }
    }
}

/// An adjacency edge between two leaves that share a boundary segment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeshEdge {
    /// The two leaves it joins, with `a < b` (deterministic orientation).
    pub a: LeafId,
    pub b: LeafId,
    /// Length of the shared boundary segment (mm).
    pub shared_len: f64,
    /// Per-layer crossing capacity: `floor(shared_len / track_pitch)` reduced by
    /// the obstacle-covered portion of the shared boundary on that layer. A net
    /// crossing here consumes one unit on the layer it crosses on.
    pub capacity: Vec<u32>,
}

/// The quadtree capacity mesh over a [`RouteProblem`].
///
/// Built by [`CapacityMesh::build`]. Leaves are id-ordered (== vector index);
/// edges are sorted by `(a, b)`. Everything is serde-serializable — the slice's
/// determinism tests rely on byte-stable serialization.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapacityMesh {
    /// Number of copper layers (≥ 1).
    pub layer_count: usize,
    /// The capacity unit: `min_trace_width + clearance` (mm).
    pub track_pitch: f64,
    /// Maximum quadtree depth used (leaf ≥ `4 × track_pitch` on its short side).
    pub max_depth: u32,
    /// The board rectangle the tree covers.
    pub bounds: Rect,
    /// Leaves in depth-first tree-path order; `leaves[i].id == i`.
    pub leaves: Vec<Leaf>,
    /// Adjacency edges, sorted by `(a, b)`.
    pub edges: Vec<MeshEdge>,
}

/// A transient quadtree node during construction (before flattening to leaves).
enum Node {
    Leaf {
        rect: Rect,
        depth: u32,
    },
    Inner {
        /// NW, NE, SW, SE — the fixed child order for deterministic ids.
        children: Box<[Node; 4]>,
    },
}

impl CapacityMesh {
    /// Build the capacity mesh for `problem`.
    ///
    /// Quadtree subdivision of `bounds`: a cell splits while it straddles an
    /// obstacle boundary edge and its depth is below `max_depth` (where a leaf
    /// stays ≥ `4 × track_pitch` on its short side). Leaves then get per-layer
    /// free-area fractions and capacities, per-net blocking lists, and the
    /// adjacency graph with per-layer edge capacities.
    pub fn build(problem: &RouteProblem) -> CapacityMesh {
        let layer_count = problem.layer_count.max(1) as usize;
        let track_pitch = track_pitch(problem);

        let bounds = Rect {
            min_x: problem.bounds.min_x,
            min_y: problem.bounds.min_y,
            max_x: problem.bounds.max_x,
            max_y: problem.bounds.max_y,
        };

        let max_depth = max_depth(&bounds, track_pitch);

        // Flatten obstacles to per-layer rectangles with owning-net indices.
        let name_index = name_index(problem);
        let per_layer_obstacles = flatten_obstacles(problem, layer_count, &name_index);

        // Build the tree: subdivide a cell while any obstacle boundary crosses
        // it and depth < max_depth.
        let all_obstacle_rects: Vec<Rect> = per_layer_obstacles
            .iter()
            .flat_map(|v| v.iter().map(|o| o.rect))
            .collect();
        let root = build_node(bounds, 0, max_depth, &all_obstacle_rects);

        // Flatten to id-ordered leaves (DFS, NW/NE/SW/SE order).
        let mut leaf_rects: Vec<(Rect, u32)> = Vec::new();
        collect_leaves(&root, &mut leaf_rects);

        let leaves: Vec<Leaf> = leaf_rects
            .iter()
            .enumerate()
            .map(|(id, &(rect, depth))| {
                build_leaf(id, rect, depth, layer_count, track_pitch, &per_layer_obstacles)
            })
            .collect();

        let edges = build_edges(&leaves, layer_count, track_pitch, &per_layer_obstacles);

        CapacityMesh {
            layer_count,
            track_pitch,
            max_depth,
            bounds,
            leaves,
            edges,
        }
    }

    /// The leaf containing `point` (by center-inclusive containment), clamped to
    /// the board. Used to seed a pad onto its cell for global pathing.
    pub fn cell_at(&self, point: &Point2) -> LeafId {
        // Clamp into the bounds so off-board pads still resolve to an edge leaf.
        let x = point.x.clamp(self.bounds.min_x, self.bounds.max_x);
        let y = point.y.clamp(self.bounds.min_y, self.bounds.max_y);
        for leaf in &self.leaves {
            let r = &leaf.rect;
            // Half-open on the high edges except at the board's outer max, so a
            // point on a shared boundary lands in exactly one leaf and a point
            // on the far board edge still resolves.
            let in_x = x >= r.min_x && (x < r.max_x || r.max_x == self.bounds.max_x);
            let in_y = y >= r.min_y && (y < r.max_y || r.max_y == self.bounds.max_y);
            if in_x && in_y {
                return leaf.id;
            }
        }
        // Unreachable for an in-bounds point (the tree tiles the bounds), but be
        // total: fall back to the nearest-center leaf.
        self.leaves
            .iter()
            .min_by(|p, q| {
                let dp = dist2(&p.rect.center(), &Point2 { x, y });
                let dq = dist2(&q.rect.center(), &Point2 { x, y });
                dp.partial_cmp(&dq).unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|l| l.id)
            .unwrap_or(0)
    }
}

/// The capacity unit: `min_trace_width + clearance` (mm).
pub fn track_pitch(problem: &RouteProblem) -> f64 {
    problem.min_trace_width + problem.clearance
}

/// The deepest quadtree depth at which a leaf's short side stays ≥
/// `4 × track_pitch`. At least 0 (a degenerate board is a single leaf).
pub fn max_depth(bounds: &Rect, track_pitch: f64) -> u32 {
    let short = bounds.width().min(bounds.height()).max(0.0);
    let min_leaf = 4.0 * track_pitch;
    if min_leaf <= 0.0 || short <= min_leaf {
        return 0;
    }
    // Largest d with short / 2^d >= min_leaf  ⇔  2^d <= short / min_leaf.
    let ratio = short / min_leaf;
    ratio.log2().floor().max(0.0) as u32
}

/// Connection name → dense index (connections order), first-wins (matches
/// [`crate::grid`]).
fn name_index(problem: &RouteProblem) -> BTreeMap<String, usize> {
    let mut m = BTreeMap::new();
    for (i, c) in problem.connections.iter().enumerate() {
        m.entry(c.name.clone()).or_insert(i);
    }
    m
}

/// Flatten every obstacle into per-layer rectangles tagged with owning-net
/// indices. `out[layer]` lists the obstacles on that copper layer.
fn flatten_obstacles(
    problem: &RouteProblem,
    layer_count: usize,
    name_index: &BTreeMap<String, usize>,
) -> Vec<Vec<LayerObstacle>> {
    let mut out: Vec<Vec<LayerObstacle>> = vec![Vec::new(); layer_count];
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
                out[layer as usize].push(LayerObstacle {
                    rect,
                    owners: owners.clone(),
                });
            }
        }
    }
    out
}

/// Recursively build the quadtree: subdivide while some obstacle boundary
/// crosses the cell and depth < max_depth. Children in NW, NE, SW, SE order.
fn build_node(rect: Rect, depth: u32, max_depth: u32, obstacle_rects: &[Rect]) -> Node {
    let should_split = depth < max_depth
        && obstacle_rects.iter().any(|o| rect.boundary_crosses(o));
    if !should_split {
        return Node::Leaf { rect, depth };
    }
    let mid_x = (rect.min_x + rect.max_x) / 2.0;
    let mid_y = (rect.min_y + rect.max_y) / 2.0;
    let nw = Rect {
        min_x: rect.min_x,
        min_y: rect.min_y,
        max_x: mid_x,
        max_y: mid_y,
    };
    let ne = Rect {
        min_x: mid_x,
        min_y: rect.min_y,
        max_x: rect.max_x,
        max_y: mid_y,
    };
    let sw = Rect {
        min_x: rect.min_x,
        min_y: mid_y,
        max_x: mid_x,
        max_y: rect.max_y,
    };
    let se = Rect {
        min_x: mid_x,
        min_y: mid_y,
        max_x: rect.max_x,
        max_y: rect.max_y,
    };
    Node::Inner {
        children: Box::new([
            build_node(nw, depth + 1, max_depth, obstacle_rects),
            build_node(ne, depth + 1, max_depth, obstacle_rects),
            build_node(sw, depth + 1, max_depth, obstacle_rects),
            build_node(se, depth + 1, max_depth, obstacle_rects),
        ]),
    }
}

/// Depth-first collect of leaf rects in tree-path order (NW, NE, SW, SE).
fn collect_leaves(node: &Node, out: &mut Vec<(Rect, u32)>) {
    match node {
        Node::Leaf { rect, depth } => out.push((*rect, *depth)),
        Node::Inner { children } => {
            for c in children.iter() {
                collect_leaves(c, out);
            }
        }
    }
}

/// Compute one leaf's per-layer capacity state and per-layer blocking lists.
fn build_leaf(
    id: LeafId,
    rect: Rect,
    depth: u32,
    layer_count: usize,
    track_pitch: f64,
    per_layer_obstacles: &[Vec<LayerObstacle>],
) -> Leaf {
    let leaf_area = rect.area();
    let short = rect.width().min(rect.height());
    // Tracks that fit across the leaf's short side, ignoring coverage.
    let base_slots = if track_pitch > 0.0 {
        (short / track_pitch).floor().max(0.0) as u32
    } else {
        0
    };

    let mut layers: Vec<LeafLayer> = Vec::with_capacity(layer_count);
    let mut blocking: Vec<Vec<usize>> = Vec::with_capacity(layer_count);

    for layer_obstacles in per_layer_obstacles.iter().take(layer_count) {
        // Two kinds of coverage cut capacity differently:
        //
        // - **Keepouts** (copper with no owner) block *every* net, so their
        //   covered area reduces the leaf's `free_fraction` directly — this is
        //   the capacity even a net's own copper cannot reclaim.
        // - **Foreign copper** (a pad owned by some net) blocks every *other*
        //   net but not its owner. We must NOT reduce `free_fraction` for it
        //   (that would also cut the owner, violating "own pads don't cut own
        //   capacity"); instead we record each owner in this layer's `blocking`
        //   list, and [`Leaf::capacity_for`] reads 0 for a net listed there.
        //   So a net blocked by foreign copper sees 0, its owner sees the full
        //   (keepout-only) capacity.
        //
        // Coverage area is the union of intersecting obstacle rects (not a
        // naive sum) so overlapping pads don't double-count; for the v1 fixtures
        // this also stays exact.
        let mut keepout_spans: Vec<Rect> = Vec::new();
        let mut blockers: Vec<usize> = Vec::new();

        for ob in layer_obstacles {
            if let Some(isect) = rect.intersection(&ob.rect) {
                if ob.owners.is_empty() {
                    keepout_spans.push(isect);
                } else {
                    // Foreign to every net except its owners; record owners so
                    // foreign nets read 0 capacity here, owners keep full.
                    blockers.extend(ob.owners.iter().copied());
                }
            }
        }

        blockers.sort_unstable();
        blockers.dedup();

        // Union area of keepout coverage (rect union via row-free axis-aligned
        // accumulation; fixtures' keepouts are disjoint so the simple bound is
        // exact, and an over-count only lowers capacity — conservative).
        let keepout_area = rect_union_area(&keepout_spans);

        let free_fraction = if leaf_area > 0.0 {
            (1.0 - keepout_area / leaf_area).clamp(0.0, 1.0)
        } else {
            0.0
        };

        // Generic (own-net / unblocked) capacity: base slots scaled by the
        // keepout-free fraction. A leaf fully keepout-covered reads 0 for
        // everyone; foreign-copper blocking is applied per net in `capacity_for`.
        let capacity = ((base_slots as f64) * free_fraction).floor().max(0.0) as u32;

        layers.push(LeafLayer {
            free_fraction,
            capacity,
        });
        blocking.push(blockers);
    }

    Leaf {
        id,
        rect,
        depth,
        layers,
        blocking,
    }
}

/// Build the adjacency graph between leaves, handling quadtree T-junctions (one
/// big leaf bordering several small ones). For every ordered pair whose
/// rectangles share a positive-length boundary segment, emit one edge with
/// per-layer crossing capacity. Output is sorted by `(a, b)`.
fn build_edges(
    leaves: &[Leaf],
    layer_count: usize,
    track_pitch: f64,
    per_layer_obstacles: &[Vec<LayerObstacle>],
) -> Vec<MeshEdge> {
    let mut edges: Vec<MeshEdge> = Vec::new();
    // O(n^2) over leaves: fixtures are small and this keeps the geometry simple
    // and obviously deterministic. A leaf only borders leaves it touches, so the
    // shared-segment test rejects the rest cheaply.
    for i in 0..leaves.len() {
        for j in (i + 1)..leaves.len() {
            let a = &leaves[i];
            let b = &leaves[j];
            if let Some(shared) = shared_boundary(&a.rect, &b.rect) {
                let (axis, lo, hi, coord) = shared;
                let shared_len = hi - lo;
                if shared_len <= 1e-12 {
                    continue;
                }
                let capacity = (0..layer_count)
                    .map(|layer| {
                        edge_capacity(
                            axis,
                            lo,
                            hi,
                            coord,
                            track_pitch,
                            &per_layer_obstacles[layer],
                        )
                    })
                    .collect();
                edges.push(MeshEdge {
                    a: a.id,
                    b: b.id,
                    shared_len,
                    capacity,
                });
            }
        }
    }
    edges.sort_by_key(|e| (e.a, e.b));
    edges
}

/// Which axis a shared boundary runs along.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Axis {
    /// Shared edge is vertical (leaves are left/right neighbours); `coord` is x,
    /// the segment spans `[lo, hi]` in y.
    Vertical,
    /// Shared edge is horizontal (leaves are top/bottom neighbours); `coord` is
    /// y, the segment spans `[lo, hi]` in x.
    Horizontal,
}

/// If two leaf rects abut along a shared edge with positive overlap length,
/// return `(axis, lo, hi, coord)` of the shared segment; else `None`.
fn shared_boundary(a: &Rect, b: &Rect) -> Option<(Axis, f64, f64, f64)> {
    const EPS: f64 = 1e-9;
    // Vertical shared edge: a's right == b's left (or vice versa), y-spans overlap.
    let touch_v = |left: &Rect, right: &Rect| -> Option<(Axis, f64, f64, f64)> {
        if (left.max_x - right.min_x).abs() < EPS {
            let lo = left.min_y.max(right.min_y);
            let hi = left.max_y.min(right.max_y);
            if hi - lo > EPS {
                return Some((Axis::Vertical, lo, hi, left.max_x));
            }
        }
        None
    };
    // Horizontal shared edge: a's bottom == b's top (y-down), x-spans overlap.
    let touch_h = |top: &Rect, bottom: &Rect| -> Option<(Axis, f64, f64, f64)> {
        if (top.max_y - bottom.min_y).abs() < EPS {
            let lo = top.min_x.max(bottom.min_x);
            let hi = top.max_x.min(bottom.max_x);
            if hi - lo > EPS {
                return Some((Axis::Horizontal, lo, hi, top.max_y));
            }
        }
        None
    };
    touch_v(a, b)
        .or_else(|| touch_v(b, a))
        .or_else(|| touch_h(a, b))
        .or_else(|| touch_h(b, a))
}

/// Per-layer crossing capacity of a shared boundary segment:
/// `floor(shared_len / track_pitch)` minus the obstacle-covered portion of that
/// segment (a track cannot cross where foreign copper or a keepout sits on the
/// boundary). Coverage is the union length of obstacle spans projected onto the
/// segment; the result is `floor(free_len / track_pitch)`.
fn edge_capacity(
    axis: Axis,
    lo: f64,
    hi: f64,
    coord: f64,
    track_pitch: f64,
    obstacles: &[LayerObstacle],
) -> u32 {
    if track_pitch <= 0.0 || hi - lo <= 0.0 {
        return 0;
    }
    // Collect the covered sub-intervals of [lo, hi] where an obstacle straddles
    // the boundary line `coord`.
    let mut covered: Vec<(f64, f64)> = Vec::new();
    for ob in obstacles {
        let r = &ob.rect;
        let (on_line, seg_lo, seg_hi) = match axis {
            Axis::Vertical => (
                r.min_x <= coord && coord <= r.max_x,
                r.min_y.max(lo),
                r.max_y.min(hi),
            ),
            Axis::Horizontal => (
                r.min_y <= coord && coord <= r.max_y,
                r.min_x.max(lo),
                r.max_x.min(hi),
            ),
        };
        if on_line && seg_hi > seg_lo {
            covered.push((seg_lo, seg_hi));
        }
    }
    let covered_len = union_length(&mut covered);
    let free_len = (hi - lo - covered_len).max(0.0);
    (free_len / track_pitch).floor().max(0.0) as u32
}

/// Area of the union of axis-aligned rectangles (mm²), via coordinate
/// compression. Exact, and `0` for an empty set. Used for keepout coverage,
/// where the rect count per leaf is tiny.
fn rect_union_area(rects: &[Rect]) -> f64 {
    if rects.is_empty() {
        return 0.0;
    }
    let mut xs: Vec<f64> = Vec::with_capacity(rects.len() * 2);
    let mut ys: Vec<f64> = Vec::with_capacity(rects.len() * 2);
    for r in rects {
        xs.push(r.min_x);
        xs.push(r.max_x);
        ys.push(r.min_y);
        ys.push(r.max_y);
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    xs.dedup_by(|a, b| (*a - *b).abs() < 1e-12);
    ys.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    ys.dedup_by(|a, b| (*a - *b).abs() < 1e-12);

    let mut area = 0.0;
    for xi in 0..xs.len().saturating_sub(1) {
        let (x0, x1) = (xs[xi], xs[xi + 1]);
        let cx = (x0 + x1) / 2.0;
        for yi in 0..ys.len().saturating_sub(1) {
            let (y0, y1) = (ys[yi], ys[yi + 1]);
            let cy = (y0 + y1) / 2.0;
            // The cell is covered iff its center lies in any rect.
            if rects
                .iter()
                .any(|r| cx >= r.min_x && cx <= r.max_x && cy >= r.min_y && cy <= r.max_y)
            {
                area += (x1 - x0) * (y1 - y0);
            }
        }
    }
    area
}

/// Total length covered by a set of 1-D intervals (their union), mm.
fn union_length(intervals: &mut [(f64, f64)]) -> f64 {
    if intervals.is_empty() {
        return 0.0;
    }
    intervals.sort_by(|p, q| p.0.partial_cmp(&q.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut total = 0.0;
    let (mut cur_lo, mut cur_hi) = intervals[0];
    for &(lo, hi) in &intervals[1..] {
        if lo > cur_hi {
            total += cur_hi - cur_lo;
            cur_lo = lo;
            cur_hi = hi;
        } else {
            cur_hi = cur_hi.max(hi);
        }
    }
    total += cur_hi - cur_lo;
    total
}

/// Squared euclidean distance between two points.
#[inline]
fn dist2(a: &Point2, b: &Point2) -> f64 {
    let dx = a.x - b.x;
    let dy = a.y - b.y;
    dx * dx + dy * dy
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::problem::{Bounds, Connection, LayerRef, Obstacle, Point2, RoutePoint, RouteProblem};
    use std::path::Path;

    fn base(obstacles: Vec<Obstacle>, connections: Vec<Connection>) -> RouteProblem {
        RouteProblem {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles,
            connections,
            bounds: Bounds {
                min_x: 0.0,
                max_x: 16.0,
                min_y: 0.0,
                max_y: 16.0,
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

    fn load(name: &str) -> RouteProblem {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures")
            .join(name);
        let json = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        serde_json::from_str(&json).unwrap_or_else(|e| panic!("parse {name}: {e}"))
    }

    /// Leaf rects must tile the bounds exactly: total leaf area == bounds area.
    fn assert_tiles_bounds(mesh: &CapacityMesh) {
        let total: f64 = mesh.leaves.iter().map(|l| l.rect.area()).sum();
        let want = mesh.bounds.area();
        assert!(
            (total - want).abs() < 1e-9,
            "leaves must tile bounds: got {total}, want {want}"
        );
    }

    #[test]
    fn track_pitch_is_width_plus_clearance() {
        let p = base(vec![], vec![]);
        assert!((track_pitch(&p) - 0.4).abs() < 1e-12, "0.2 + 0.2 = 0.4");
    }

    #[test]
    fn empty_board_is_a_single_leaf_with_sane_capacity() {
        // No obstacles ⇒ no boundary crossings ⇒ no subdivision.
        let p = base(vec![], vec![]);
        let mesh = CapacityMesh::build(&p);
        assert_eq!(mesh.leaves.len(), 1, "empty board is one leaf");
        assert!(mesh.edges.is_empty(), "one leaf has no adjacency edges");
        let leaf = &mesh.leaves[0];
        // 16 mm / 0.4 = 40 track slots, fully free.
        for l in &leaf.layers {
            assert!((l.free_fraction - 1.0).abs() < 1e-12);
            assert_eq!(l.capacity, 40, "16/0.4 = 40 free slots");
        }
        assert_tiles_bounds(&mesh);
    }

    #[test]
    fn central_obstacle_forces_subdivision_and_cuts_capacity() {
        // A keepout in the middle: its boundary crosses the root, forcing splits.
        let p = base(
            vec![pad(&[], (8.0, 8.0), 2.0, 2.0, &["top"])],
            vec![],
        );
        let mesh = CapacityMesh::build(&p);
        assert!(
            mesh.leaves.len() > 1,
            "a central obstacle must force subdivision, got {} leaves",
            mesh.leaves.len()
        );
        // Some top-layer leaf must have its capacity cut by the keepout.
        let cut = mesh.leaves.iter().any(|l| {
            l.layers[0].free_fraction < 1.0 - 1e-9 && l.rect.overlaps(&Rect {
                min_x: 7.0,
                min_y: 7.0,
                max_x: 9.0,
                max_y: 9.0,
            })
        });
        assert!(cut, "the keepout must reduce some leaf's free fraction");
        // The bottom layer (no obstacle) stays fully free everywhere.
        assert!(
            mesh.leaves.iter().all(|l| (l.layers[1].free_fraction - 1.0).abs() < 1e-9),
            "bottom layer has no obstacle, stays free"
        );
        assert_tiles_bounds(&mesh);
    }

    #[test]
    fn t_junction_adjacency_is_correct() {
        // One off-center small obstacle makes one quadrant refine while its
        // siblings stay big: a big leaf borders several small ones (T-junction).
        let p = base(
            vec![pad(&[], (4.0, 4.0), 1.0, 1.0, &["top"])],
            vec![],
        );
        let mesh = CapacityMesh::build(&p);
        assert!(mesh.leaves.len() > 4, "obstacle forces nested refinement");
        assert_tiles_bounds(&mesh);

        // Every edge joins leaves that truly share a positive-length boundary,
        // and every shared boundary appears as exactly one edge.
        let mut seen: std::collections::BTreeSet<(usize, usize)> = Default::default();
        for e in &mesh.edges {
            assert!(e.a < e.b, "edges are oriented a < b");
            assert!(seen.insert((e.a, e.b)), "no duplicate edges");
            assert!(e.shared_len > 0.0, "edges have positive shared length");
            let ra = &mesh.leaves[e.a].rect;
            let rb = &mesh.leaves[e.b].rect;
            assert!(
                shared_boundary(ra, rb).is_some(),
                "edge {}-{} must share a boundary",
                e.a,
                e.b
            );
        }

        // T-junction existence: at least one leaf borders 3+ neighbours (a big
        // leaf against a refined quadrant's small leaves).
        let mut degree = vec![0usize; mesh.leaves.len()];
        for e in &mesh.edges {
            degree[e.a] += 1;
            degree[e.b] += 1;
        }
        assert!(
            degree.iter().any(|&d| d >= 3),
            "a T-junction means some leaf has degree >= 3"
        );
    }

    #[test]
    fn own_net_pad_does_not_cut_its_own_capacity() {
        // A pad owned by SIG: SIG must still see capacity through that leaf, but
        // a foreign net (GND) is blocked there.
        let p = base(
            vec![pad(&["SIG"], (8.0, 8.0), 2.0, 2.0, &["top"])],
            vec![
                conn("SIG", &[(8.0, 8.0, "top")]),
                conn("GND", &[(1.0, 1.0, "top")]),
            ],
        );
        let mesh = CapacityMesh::build(&p);
        let sig = 0usize;
        let gnd = 1usize;
        // Every leaf the SIG pad's copper touches on the top layer must keep
        // SIG's own capacity nonzero while blocking the foreign GND net.
        let pad_rect = Rect {
            min_x: 7.0,
            min_y: 7.0,
            max_x: 9.0,
            max_y: 9.0,
        };
        let touched: Vec<&Leaf> = mesh
            .leaves
            .iter()
            .filter(|l| {
                l.rect
                    .intersection(&pad_rect)
                    .map(|i| i.area() > 1e-9)
                    .unwrap_or(false)
            })
            .collect();
        assert!(!touched.is_empty(), "the SIG pad must overlap some leaves");
        for leaf in touched {
            // SIG owns the copper here, so it sees the full layer capacity…
            assert!(
                leaf.capacity_for(0, sig) > 0,
                "SIG passes through its own pad's leaf {} (cap {})",
                leaf.id,
                leaf.capacity_for(0, sig)
            );
            // …while the foreign GND net is blocked there.
            assert_eq!(
                leaf.capacity_for(0, gnd),
                0,
                "GND is blocked by SIG's foreign pad copper in leaf {}",
                leaf.id
            );
            // SIG is recorded as an owner of the copper in this leaf.
            assert!(leaf.blocking[0].binary_search(&sig).is_ok());
        }
    }

    #[test]
    fn build_is_deterministic_under_serialization() {
        let p = base(
            vec![
                pad(&["SIG"], (4.0, 4.0), 1.0, 1.0, &["top"]),
                pad(&[], (11.0, 11.0), 2.0, 1.5, &["top", "bottom"]),
            ],
            vec![conn("SIG", &[(4.0, 4.0, "top")])],
        );
        let a = CapacityMesh::build(&p);
        let b = CapacityMesh::build(&p);
        let ja = serde_json::to_string(&a).unwrap();
        let jb = serde_json::to_string(&b).unwrap();
        assert_eq!(ja, jb, "two builds must serialize byte-equal");
    }

    #[test]
    fn cell_at_locates_points_in_one_leaf() {
        let p = base(
            vec![pad(&[], (8.0, 8.0), 2.0, 2.0, &["top"])],
            vec![],
        );
        let mesh = CapacityMesh::build(&p);
        // Every leaf's center resolves back to that leaf.
        for leaf in &mesh.leaves {
            let c = leaf.rect.center();
            assert_eq!(mesh.cell_at(&c), leaf.id, "center of leaf {} round-trips", leaf.id);
        }
        // Corners of the board resolve to in-range leaves.
        let _ = mesh.cell_at(&Point2 { x: 0.0, y: 0.0 });
        let _ = mesh.cell_at(&Point2 { x: 16.0, y: 16.0 });
        // Off-board clamps into the board.
        let id = mesh.cell_at(&Point2 { x: -5.0, y: 100.0 });
        assert!(id < mesh.leaves.len());
    }

    #[test]
    fn led_r_builds_a_sane_mesh() {
        assert_sane_fixture("led-r.json");
    }

    #[test]
    fn quad_builds_a_sane_mesh() {
        assert_sane_fixture("quad.json");
    }

    /// Shared "sane mesh" assertions for a fixture: tiles the bounds, every leaf
    /// has a valid per-layer capacity vector, and every pad's own net has
    /// nonzero capacity in the pad's cell.
    fn assert_sane_fixture(name: &str) {
        let p = load(name);
        let mesh = CapacityMesh::build(&p);
        let layer_count = p.layer_count.max(1) as usize;

        assert_tiles_bounds(&mesh);
        assert!(mesh.leaves.len() > 1, "{name} has obstacles, must subdivide");
        assert!(!mesh.edges.is_empty(), "{name} must have adjacency edges");

        // Leaf ids are dense and ordered.
        for (i, leaf) in mesh.leaves.iter().enumerate() {
            assert_eq!(leaf.id, i, "leaf id == index");
            assert_eq!(leaf.layers.len(), layer_count, "per-layer capacity vector");
            assert_eq!(leaf.blocking.len(), layer_count, "per-layer blocking vector");
        }
        // Edges reference valid leaves and have per-layer capacity vectors.
        for e in &mesh.edges {
            assert!(e.a < mesh.leaves.len() && e.b < mesh.leaves.len());
            assert_eq!(e.capacity.len(), layer_count);
        }

        // Every pad point's cell has nonzero capacity for its *own* net on the
        // pad's layer — a net can always start/end at its own pad.
        let name_index = name_index(&p);
        for conn in &p.connections {
            let ci = name_index[&conn.name];
            for pt in &conn.points_to_connect {
                let leaf_id = mesh.cell_at(&Point2 { x: pt.x, y: pt.y });
                let leaf = &mesh.leaves[leaf_id];
                let layer = pt.layer.index(layer_count as u32).unwrap_or(0) as usize;
                assert!(
                    leaf.capacity_for(layer, ci) > 0,
                    "{name}: net {} pad at ({},{}) on layer {layer} has zero own capacity in leaf {leaf_id}",
                    conn.name,
                    pt.x,
                    pt.y
                );
            }
        }
    }
}
