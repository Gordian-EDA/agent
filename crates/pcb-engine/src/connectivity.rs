//! Connectivity oracle: does the emitted copper actually join what it should,
//! and nothing it shouldn't?
//!
//! This is the connectivity half of oracle layer 1 (the strict in-house gate).
//! It is deliberately strict: a real violation must surface as a [`Violation`],
//! never pass silently. Two complementary failures are reported:
//!
//! - [`Violation::Unconnected`] — a connection's `points_to_connect` are not all
//!   electrically joined by the solution copper.
//! - [`Violation::CrossNetMerge`] — copper from two different connections has
//!   been shorted together.
//!
//! ## How it works
//!
//! Every piece of copper becomes an *element*: pads (obstacles with a non-empty
//! `connected_to`), each segment of each trace polyline, each via, and a
//! zero-size element at each `points_to_connect`. A union-find joins any two
//! elements that physically touch (geometry below). After unioning, each
//! connection must have all its points in one component (else `Unconnected`),
//! and no component may carry copper owned by two different connection names
//! (else `CrossNetMerge`).
//!
//! ## Touch geometry (all distances in mm, slop [`EPS`])
//!
//! Copper is fattened by half its width; two elements touch when their fattened
//! shapes overlap:
//! - segment ↔ segment (same layer): segment distance ≤ (wa + wb)/2 + EPS.
//! - segment ↔ pad (pad present on the segment's layer): segment-to-rect
//!   distance ≤ wa/2 + EPS. A multi-layer (thru-hole-ish) pad is present on
//!   each layer it lists.
//! - segment ↔ point (same layer): point-to-segment distance ≤ wa/2 + EPS.
//! - pad ↔ point (same layer): point inside / within EPS of the rect.
//! - via ↔ anything (ANY layer): distance ≤ via_radius + other_half_width + EPS.
//!   A through via stitches every layer at its position (v1: through only).
//! - pad ↔ pad: rect-rect distance 0 *and* a shared layer.
//!
//! ## Cross-net merge suppression for shared pads
//!
//! An obstacle whose `connected_to` lists *multiple* connections is a
//! deliberately shared pad (e.g. a fan-out / star point) and must not, by
//! itself, be reported as a merge. Rule implemented (the simplest correct one):
//! **ownership contributed by a multi-name obstacle is ignored when computing
//! merges.** Each multi-name obstacle's names are folded into one *allowed
//! group*; a `CrossNetMerge` for a pair `(a, b)` is suppressed iff `a` and `b`
//! fall in the same allowed group. Merges driven by genuine geometric contact
//! between single-net copper are still reported.

use crate::problem::{LayerRef, RouteProblem, RouteSolution};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// Geometric slop, mm. Distances within this of a threshold count as touching.
const EPS: f64 = 1e-6;

/// A connectivity defect in a [`RouteSolution`] relative to its problem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    /// A connection's point #`point_index` is not joined to its point #0 by the
    /// emitted copper. (Connections with fewer than 2 points pass trivially.)
    Unconnected {
        /// Connection name.
        connection: String,
        /// Index into the connection's `points_to_connect` that is stranded.
        point_index: usize,
    },
    /// Copper from connections `a` and `b` is electrically shorted. Names are
    /// normalized so `a < b`; one violation is reported per unordered pair.
    CrossNetMerge {
        /// First connection name (the lexicographically smaller).
        a: String,
        /// Second connection name.
        b: String,
    },
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Violation::Unconnected {
                connection,
                point_index,
            } => write!(
                f,
                "connection \"{connection}\": point {point_index} is not connected to point 0 \
                 by the emitted copper",
            ),
            Violation::CrossNetMerge { a, b } => write!(
                f,
                "cross-net short: connections \"{a}\" and \"{b}\" are electrically merged",
            ),
        }
    }
}

/// Check that `solution`'s copper connects every connection's points and merges
/// no two distinct connections. Returns violations in deterministic order
/// (`Unconnected` first, by connection then point index; then `CrossNetMerge`
/// by name pair).
pub fn check(problem: &RouteProblem, solution: &RouteSolution) -> Vec<Violation> {
    let elements = build_elements(problem, solution);
    let mut uf = UnionFind::new(elements.len());

    // O(n^2) pairwise touch test — element counts are tiny (a board's copper),
    // and clarity beats a spatial index here.
    for i in 0..elements.len() {
        for j in (i + 1)..elements.len() {
            if touches(&elements[i], &elements[j]) {
                uf.union(i, j);
            }
        }
    }

    let mut violations = unconnected_violations(problem, &elements, &mut uf);
    violations.extend(cross_net_violations(&elements, &mut uf));
    violations
}

// ── copper elements ──────────────────────────────────────────────────────────

/// One piece of copper, tagged with its owners and a touch shape.
struct Element {
    /// Connection names this copper belongs to. Empty for a bare via/point that
    /// only borrows its connection name (which we still record below).
    owners: Vec<String>,
    /// True when this element is a multi-name shared pad (suppresses merges).
    shared_pad: bool,
    shape: Shape,
}

/// The touchable geometry of an [`Element`].
enum Shape {
    /// A fattened segment on one layer: endpoints + half-width. A zero-length
    /// segment is a fat point.
    Segment {
        a: [f64; 2],
        b: [f64; 2],
        half_w: f64,
        layer: LayerRef,
    },
    /// An axis-aligned pad rectangle present on a set of layers.
    Pad {
        min: [f64; 2],
        max: [f64; 2],
        layers: Vec<LayerRef>,
    },
    /// A zero-size copper anchor (a `points_to_connect`) on one layer.
    Point { at: [f64; 2], layer: LayerRef },
    /// A through via: a disc that stitches every layer at its position.
    Via { at: [f64; 2], radius: f64 },
}

fn build_elements(problem: &RouteProblem, solution: &RouteSolution) -> Vec<Element> {
    let mut els = Vec::new();

    // (1) Pads: obstacles owned by ≥1 connection.
    for ob in &problem.obstacles {
        if ob.connected_to.is_empty() {
            continue;
        }
        let hw = ob.width / 2.0;
        let hh = ob.height / 2.0;
        els.push(Element {
            owners: ob.connected_to.clone(),
            shared_pad: ob.connected_to.len() > 1,
            shape: Shape::Pad {
                min: [ob.center.x - hw, ob.center.y - hh],
                max: [ob.center.x + hw, ob.center.y + hh],
                layers: ob.layers.clone(),
            },
        });
    }

    // (2) Trace segments: every consecutive point pair of every polyline.
    for trace in &solution.traces {
        let half_w = trace.width / 2.0;
        if trace.path.len() == 1 {
            // Degenerate single-point trace: treat as a fat point segment.
            let p = &trace.path[0];
            els.push(Element {
                owners: vec![trace.connection.clone()],
                shared_pad: false,
                shape: Shape::Segment {
                    a: [p.x, p.y],
                    b: [p.x, p.y],
                    half_w,
                    layer: trace.layer.clone(),
                },
            });
        }
        for w in trace.path.windows(2) {
            els.push(Element {
                owners: vec![trace.connection.clone()],
                shared_pad: false,
                shape: Shape::Segment {
                    a: [w[0].x, w[0].y],
                    b: [w[1].x, w[1].y],
                    half_w,
                    layer: trace.layer.clone(),
                },
            });
        }
    }

    // (3) Vias.
    for via in &solution.vias {
        els.push(Element {
            owners: vec![via.connection.clone()],
            shared_pad: false,
            shape: Shape::Via {
                at: [via.at.x, via.at.y],
                radius: via.diameter / 2.0,
            },
        });
    }

    // (4) A zero-size copper anchor at each point to connect.
    for conn in &problem.connections {
        for p in &conn.points_to_connect {
            els.push(Element {
                owners: vec![conn.name.clone()],
                shared_pad: false,
                shape: Shape::Point {
                    at: [p.x, p.y],
                    layer: p.layer.clone(),
                },
            });
        }
    }

    els
}

// ── touch predicate ──────────────────────────────────────────────────────────

/// Do two elements electrically touch? See the module docs for the per-pair
/// rules. A via touches across *all* layers; everything else is layer-checked.
fn touches(x: &Element, y: &Element) -> bool {
    use Shape::*;
    match (&x.shape, &y.shape) {
        // via ↔ anything, any layer.
        (Via { at, radius }, other) | (other, Via { at, radius }) => {
            via_touches(*at, *radius, other)
        }

        (
            Segment {
                a: a1,
                b: b1,
                half_w: w1,
                layer: l1,
            },
            Segment {
                a: a2,
                b: b2,
                half_w: w2,
                layer: l2,
            },
        ) => l1 == l2 && seg_seg_dist(*a1, *b1, *a2, *b2) <= w1 + w2 + EPS,

        (
            Segment {
                a,
                b,
                half_w,
                layer,
            },
            Pad { min, max, layers },
        )
        | (
            Pad { min, max, layers },
            Segment {
                a,
                b,
                half_w,
                layer,
            },
        ) => layers.contains(layer) && seg_rect_dist(*a, *b, *min, *max) <= half_w + EPS,

        (
            Segment {
                a,
                b,
                half_w,
                layer,
            },
            Point { at, layer: pl },
        )
        | (
            Point { at, layer: pl },
            Segment {
                a,
                b,
                half_w,
                layer,
            },
        ) => layer == pl && point_seg_dist(*at, *a, *b) <= half_w + EPS,

        (Pad { min, max, layers }, Point { at, layer })
        | (Point { at, layer }, Pad { min, max, layers }) => {
            layers.contains(layer) && point_rect_dist(*at, *min, *max) <= EPS
        }

        (
            Pad {
                min: mn1,
                max: mx1,
                layers: l1,
            },
            Pad {
                min: mn2,
                max: mx2,
                layers: l2,
            },
        ) => l1.iter().any(|l| l2.contains(l)) && rect_rect_dist(*mn1, *mx1, *mn2, *mx2) <= EPS,

        // point ↔ point: zero-size anchors never touch each other directly;
        // they are only ever joined through real copper.
        (Point { .. }, Point { .. }) => false,
    }
}

/// A through via at `at` with `radius` touches `other` on any layer when the
/// disc reaches the other element's fattened body.
fn via_touches(at: [f64; 2], radius: f64, other: &Shape) -> bool {
    match other {
        Shape::Segment { a, b, half_w, .. } => point_seg_dist(at, *a, *b) <= radius + half_w + EPS,
        Shape::Pad { min, max, .. } => point_rect_dist(at, *min, *max) <= radius + EPS,
        Shape::Point { at: p, .. } => dist(at, *p) <= radius + EPS,
        Shape::Via { at: p, radius: r2 } => dist(at, *p) <= radius + r2 + EPS,
    }
}

// ── union-find ───────────────────────────────────────────────────────────────

struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<u8>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
            rank: vec![0; n],
        }
    }

    fn find(&mut self, mut i: usize) -> usize {
        while self.parent[i] != i {
            self.parent[i] = self.parent[self.parent[i]]; // path halving
            i = self.parent[i];
        }
        i
    }

    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra == rb {
            return;
        }
        match self.rank[ra].cmp(&self.rank[rb]) {
            std::cmp::Ordering::Less => self.parent[ra] = rb,
            std::cmp::Ordering::Greater => self.parent[rb] = ra,
            std::cmp::Ordering::Equal => {
                self.parent[rb] = ra;
                self.rank[ra] += 1;
            }
        }
    }
}

// ── violation extraction ─────────────────────────────────────────────────────

/// For each connection, the elements representing its `points_to_connect` were
/// appended in order; recover them and check they share point 0's root.
fn unconnected_violations(
    problem: &RouteProblem,
    elements: &[Element],
    uf: &mut UnionFind,
) -> Vec<Violation> {
    // Locate the point elements: they were pushed last, in connection order.
    // Recompute the index of the first point element.
    let point_start = elements
        .iter()
        .position(|e| matches!(e.shape, Shape::Point { .. }))
        .unwrap_or(elements.len());

    let mut out = Vec::new();
    let mut cursor = point_start;
    for conn in &problem.connections {
        let n = conn.points_to_connect.len();
        let idxs: Vec<usize> = (cursor..cursor + n).collect();
        cursor += n;
        if n < 2 {
            continue; // trivially connected
        }
        let root0 = uf.find(idxs[0]);
        for (point_index, &el) in idxs.iter().enumerate().skip(1) {
            if uf.find(el) != root0 {
                out.push(Violation::Unconnected {
                    connection: conn.name.clone(),
                    point_index,
                });
            }
        }
    }
    out
}

/// Report one `CrossNetMerge` per unordered pair of distinct single-net owners
/// that landed in the same component, honoring the shared-pad allowed groups.
fn cross_net_violations(elements: &[Element], uf: &mut UnionFind) -> Vec<Violation> {
    // Allowed groups: every set of names that co-own a shared pad is fused into
    // one group (transitively, in case pads chain through common names).
    let mut groups = NameGroups::default();
    for e in elements {
        if e.shared_pad {
            groups.fuse_all(&e.owners);
        }
    }

    // Collect, per component root, the set of single-net owner names. We ignore
    // ownership contributed by shared pads when seeding the conflict set, but a
    // shared pad still physically unions its component (it carries a real root).
    let mut by_root: BTreeMap<usize, BTreeSet<String>> = BTreeMap::new();
    for (i, e) in elements.iter().enumerate() {
        if e.shared_pad {
            continue;
        }
        let root = uf.find(i);
        let bucket = by_root.entry(root).or_default();
        for name in &e.owners {
            bucket.insert(name.clone());
        }
    }

    let mut pairs: BTreeSet<(String, String)> = BTreeSet::new();
    for names in by_root.values() {
        let names: Vec<&String> = names.iter().collect();
        for i in 0..names.len() {
            for j in (i + 1)..names.len() {
                if groups.same_group(names[i], names[j]) {
                    continue; // legitimately joined through a shared pad
                }
                let (a, b) = if names[i] <= names[j] {
                    (names[i].clone(), names[j].clone())
                } else {
                    (names[j].clone(), names[i].clone())
                };
                pairs.insert((a, b));
            }
        }
    }

    pairs
        .into_iter()
        .map(|(a, b)| Violation::CrossNetMerge { a, b })
        .collect()
}

/// Union-find over *connection names* used to model shared-pad allowed groups.
#[derive(Default)]
struct NameGroups {
    parent: BTreeMap<String, String>,
}

impl NameGroups {
    /// Register `name` as its own root if unseen, so every owner of a shared pad
    /// is a known node before we test group membership.
    fn ensure(&mut self, name: &str) {
        if !self.parent.contains_key(name) {
            self.parent.insert(name.to_owned(), name.to_owned());
        }
    }

    fn find(&mut self, name: &str) -> String {
        let p = self.parent.get(name).cloned();
        match p {
            // Unknown name: a singleton group identified by itself.
            None => name.to_owned(),
            Some(p) if p == name => p,
            Some(p) => {
                let root = self.find(&p);
                self.parent.insert(name.to_owned(), root.clone());
                root
            }
        }
    }

    fn fuse(&mut self, a: &str, b: &str) {
        self.ensure(a);
        self.ensure(b);
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            self.parent.insert(ra, rb);
        }
    }

    fn fuse_all(&mut self, names: &[String]) {
        for w in names.windows(2) {
            self.fuse(&w[0], &w[1]);
        }
    }

    /// True iff `a` and `b` are in the same shared-pad allowed group. A name
    /// never seen on a shared pad is a singleton, so two such distinct names are
    /// never in the same group.
    fn same_group(&mut self, a: &str, b: &str) -> bool {
        if a == b {
            return true;
        }
        // At least one must be a known shared-pad owner for them to share a group.
        if !self.parent.contains_key(a) && !self.parent.contains_key(b) {
            return false;
        }
        self.find(a) == self.find(b)
    }
}

// ── geometry primitives ──────────────────────────────────────────────────────

fn dist(a: [f64; 2], b: [f64; 2]) -> f64 {
    let dx = a[0] - b[0];
    let dy = a[1] - b[1];
    (dx * dx + dy * dy).sqrt()
}

/// Distance from point `p` to segment `ab` (a zero-length segment is a point).
fn point_seg_dist(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    let ab = [b[0] - a[0], b[1] - a[1]];
    let len2 = ab[0] * ab[0] + ab[1] * ab[1];
    if len2 <= f64::EPSILON {
        return dist(p, a);
    }
    let t = (((p[0] - a[0]) * ab[0] + (p[1] - a[1]) * ab[1]) / len2).clamp(0.0, 1.0);
    let proj = [a[0] + t * ab[0], a[1] + t * ab[1]];
    dist(p, proj)
}

/// Minimum distance between segments `ab` and `cd`. Zero when they intersect.
fn seg_seg_dist(a: [f64; 2], b: [f64; 2], c: [f64; 2], d: [f64; 2]) -> f64 {
    if segments_intersect(a, b, c, d) {
        return 0.0;
    }
    // No crossing: the minimum is an endpoint-to-other-segment distance.
    point_seg_dist(a, c, d)
        .min(point_seg_dist(b, c, d))
        .min(point_seg_dist(c, a, b))
        .min(point_seg_dist(d, a, b))
}

fn cross(o: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0])
}

/// Do segments `ab` and `cd` intersect (including touching / collinear overlap)?
fn segments_intersect(a: [f64; 2], b: [f64; 2], c: [f64; 2], d: [f64; 2]) -> bool {
    let d1 = cross(c, d, a);
    let d2 = cross(c, d, b);
    let d3 = cross(a, b, c);
    let d4 = cross(a, b, d);
    if ((d1 > 0.0 && d2 < 0.0) || (d1 < 0.0 && d2 > 0.0))
        && ((d3 > 0.0 && d4 < 0.0) || (d3 < 0.0 && d4 > 0.0))
    {
        return true;
    }
    // Collinear / touching cases.
    on_segment(c, d, a, d1)
        || on_segment(c, d, b, d2)
        || on_segment(a, b, c, d3)
        || on_segment(a, b, d, d4)
}

/// `p` lies on segment `ab` given the orientation determinant `cr` for `p`.
fn on_segment(a: [f64; 2], b: [f64; 2], p: [f64; 2], cr: f64) -> bool {
    cr.abs() <= EPS
        && p[0] >= a[0].min(b[0]) - EPS
        && p[0] <= a[0].max(b[0]) + EPS
        && p[1] >= a[1].min(b[1]) - EPS
        && p[1] <= a[1].max(b[1]) + EPS
}

/// Distance from point `p` to the axis-aligned rect `[min, max]`; 0 inside.
fn point_rect_dist(p: [f64; 2], min: [f64; 2], max: [f64; 2]) -> f64 {
    let dx = (min[0] - p[0]).max(0.0).max(p[0] - max[0]);
    let dy = (min[1] - p[1]).max(0.0).max(p[1] - max[1]);
    (dx * dx + dy * dy).sqrt()
}

/// Distance from segment `ab` to the axis-aligned rect `[min, max]`; 0 if the
/// segment enters or touches the rect.
fn seg_rect_dist(a: [f64; 2], b: [f64; 2], min: [f64; 2], max: [f64; 2]) -> f64 {
    if point_rect_dist(a, min, max) <= EPS || point_rect_dist(b, min, max) <= EPS {
        return 0.0;
    }
    // Closest approach is either an endpoint to an edge, or an intersection with
    // one of the four edges (distance 0). Test against each edge segment.
    let corners = [
        [min[0], min[1]],
        [max[0], min[1]],
        [max[0], max[1]],
        [min[0], max[1]],
    ];
    let mut best = f64::INFINITY;
    for i in 0..4 {
        let c = corners[i];
        let d = corners[(i + 1) % 4];
        best = best.min(seg_seg_dist(a, b, c, d));
    }
    best
}

/// Rect-rect distance (axis-aligned); 0 when they overlap or touch.
fn rect_rect_dist(mn1: [f64; 2], mx1: [f64; 2], mn2: [f64; 2], mx2: [f64; 2]) -> f64 {
    let dx = (mn1[0] - mx2[0]).max(0.0).max(mn2[0] - mx1[0]);
    let dy = (mn1[1] - mx2[1]).max(0.0).max(mn2[1] - mx1[1]);
    (dx * dx + dy * dy).sqrt()
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::problem::{
        Bounds, Connection, Obstacle, Point2, RoutePoint, RouteProblem, RouteSolution, Trace, Via,
    };

    fn bounds() -> Bounds {
        Bounds {
            min_x: -100.0,
            max_x: 100.0,
            min_y: -100.0,
            max_y: 100.0,
        }
    }

    fn problem(connections: Vec<Connection>, obstacles: Vec<Obstacle>) -> RouteProblem {
        RouteProblem {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles,
            connections,
            bounds: bounds(),
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

    fn trace(connection: &str, layer: &str, width: f64, path: &[(f64, f64)]) -> Trace {
        Trace {
            connection: connection.to_owned(),
            layer: LayerRef(layer.to_owned()),
            width,
            path: path.iter().map(|&(x, y)| Point2 { x, y }).collect(),
        }
    }

    fn via(connection: &str, at: (f64, f64)) -> Via {
        Via {
            connection: connection.to_owned(),
            at: Point2 { x: at.0, y: at.1 },
            diameter: 0.6,
            drill: 0.3,
        }
    }

    #[test]
    fn fully_connected_two_points_via_one_trace() {
        let p = problem(
            vec![conn("SIG", &[(0.0, 0.0, "top"), (10.0, 0.0, "top")])],
            vec![],
        );
        let s = RouteSolution {
            traces: vec![trace("SIG", "top", 0.25, &[(0.0, 0.0), (10.0, 0.0)])],
            vias: vec![],
        };
        assert_eq!(check(&p, &s), vec![]);
    }

    #[test]
    fn missing_segment_reports_unconnected_with_index() {
        // Trace stops short of the second point.
        let p = problem(
            vec![conn(
                "SIG",
                &[(0.0, 0.0, "top"), (10.0, 0.0, "top"), (20.0, 0.0, "top")],
            )],
            vec![],
        );
        let s = RouteSolution {
            // Reaches point 1 but not point 2.
            traces: vec![trace("SIG", "top", 0.25, &[(0.0, 0.0), (10.0, 0.0)])],
            vias: vec![],
        };
        assert_eq!(
            check(&p, &s),
            vec![Violation::Unconnected {
                connection: "SIG".to_owned(),
                point_index: 2,
            }]
        );
    }

    #[test]
    fn trace_touching_foreign_pad_reports_cross_net_merge() {
        // SIG's trace runs through GND's pad → short. Names normalized.
        let p = problem(
            vec![
                conn("SIG", &[(0.0, 0.0, "top"), (10.0, 0.0, "top")]),
                conn("GND", &[(5.0, 0.0, "top")]),
            ],
            vec![pad(&["GND"], (5.0, 0.0), 1.0, 1.0, &["top"])],
        );
        let s = RouteSolution {
            traces: vec![trace("SIG", "top", 0.25, &[(0.0, 0.0), (10.0, 0.0)])],
            vias: vec![],
        };
        let v = check(&p, &s);
        assert!(
            v.contains(&Violation::CrossNetMerge {
                a: "GND".to_owned(),
                b: "SIG".to_owned(),
            }),
            "expected normalized (GND, SIG) merge, got {v:?}"
        );
    }

    #[test]
    fn two_layer_connection_joined_only_by_via() {
        // Point 0 on top, point 1 on bottom, each reached by a same-layer trace;
        // a via at the crossover stitches the layers.
        let p = problem(
            vec![conn("SIG", &[(0.0, 0.0, "top"), (10.0, 0.0, "bottom")])],
            vec![],
        );
        let with_via = RouteSolution {
            traces: vec![
                trace("SIG", "top", 0.25, &[(0.0, 0.0), (5.0, 0.0)]),
                trace("SIG", "bottom", 0.25, &[(5.0, 0.0), (10.0, 0.0)]),
            ],
            vias: vec![via("SIG", (5.0, 0.0))],
        };
        assert_eq!(check(&p, &with_via), vec![]);

        // Remove the via: top and bottom copper no longer connect.
        let without_via = RouteSolution {
            vias: vec![],
            ..with_via
        };
        assert_eq!(
            check(&p, &without_via),
            vec![Violation::Unconnected {
                connection: "SIG".to_owned(),
                point_index: 1,
            }]
        );
    }

    #[test]
    fn multi_name_shared_pad_does_not_merge_by_itself() {
        // A single pad legitimately owned by two connections: no merge, and each
        // connection's single point is trivially connected.
        let p = problem(
            vec![
                conn("VCC", &[(0.0, 0.0, "top")]),
                conn("VCC_SENSE", &[(0.0, 0.0, "top")]),
            ],
            vec![pad(&["VCC", "VCC_SENSE"], (0.0, 0.0), 1.0, 1.0, &["top"])],
        );
        let s = RouteSolution {
            traces: vec![],
            vias: vec![],
        };
        assert_eq!(check(&p, &s), vec![]);
    }

    #[test]
    fn shared_pad_suppresses_only_its_own_pair() {
        // VCC + VCC_SENSE share a pad (allowed). A foreign GND trace also touches
        // that pad → GND must still merge with both.
        let p = problem(
            vec![
                conn("VCC", &[(0.0, 0.0, "top")]),
                conn("VCC_SENSE", &[(0.0, 0.0, "top")]),
                conn("GND", &[(5.0, 0.0, "top")]),
            ],
            vec![pad(&["VCC", "VCC_SENSE"], (0.0, 0.0), 1.0, 1.0, &["top"])],
        );
        let s = RouteSolution {
            traces: vec![trace("GND", "top", 0.25, &[(5.0, 0.0), (0.0, 0.0)])],
            vias: vec![],
        };
        let v = check(&p, &s);
        assert!(v.contains(&Violation::CrossNetMerge {
            a: "GND".to_owned(),
            b: "VCC".to_owned(),
        }));
        assert!(v.contains(&Violation::CrossNetMerge {
            a: "GND".to_owned(),
            b: "VCC_SENSE".to_owned(),
        }));
        // The legitimate pair is NOT reported.
        assert!(!v.contains(&Violation::CrossNetMerge {
            a: "VCC".to_owned(),
            b: "VCC_SENSE".to_owned(),
        }));
    }

    #[test]
    fn violations_are_sorted_and_stable() {
        // Two unconnected points across two connections plus a cross-net merge:
        // Unconnected first (by connection, then index), then CrossNetMerge.
        let p = problem(
            vec![
                conn("AAA", &[(0.0, 0.0, "top"), (50.0, 0.0, "top")]),
                conn("BBB", &[(0.0, 50.0, "top"), (50.0, 50.0, "top")]),
            ],
            vec![],
        );
        // No copper at all → both connections unconnected; no merges.
        let s = RouteSolution {
            traces: vec![],
            vias: vec![],
        };
        let v = check(&p, &s);
        assert_eq!(
            v,
            vec![
                Violation::Unconnected {
                    connection: "AAA".to_owned(),
                    point_index: 1,
                },
                Violation::Unconnected {
                    connection: "BBB".to_owned(),
                    point_index: 1,
                },
            ]
        );

        // Determinism: identical inputs give identical output.
        assert_eq!(check(&p, &s), v);
    }

    #[test]
    fn display_messages_name_the_culprit() {
        let u = Violation::Unconnected {
            connection: "SIG".to_owned(),
            point_index: 3,
        };
        assert!(u.to_string().contains("SIG"));
        assert!(u.to_string().contains("point 3"));

        let m = Violation::CrossNetMerge {
            a: "GND".to_owned(),
            b: "SIG".to_owned(),
        };
        assert!(m.to_string().contains("GND"));
        assert!(m.to_string().contains("SIG"));
    }
}
