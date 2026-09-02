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
//! ## Touch geometry (all distances in mm, slop `EPS`)
//!
//! Copper is fattened by half its width; two elements touch when their fattened
//! shapes overlap:
//! - segment ↔ segment (same layer): segment distance ≤ (wa + wb)/2 + EPS.
//! - segment ↔ pad (pad present on the segment's layer): segment-to-pad
//!   distance ≤ wa/2 + EPS. A multi-layer (thru-hole-ish) pad is present on
//!   each layer it lists.
//! - segment ↔ point (same layer): point-to-segment distance ≤ wa/2 + EPS.
//! - pad ↔ point (same layer): point inside / within EPS of the pad.
//! - via ↔ anything (ANY layer): distance ≤ via_radius + other_half_width + EPS.
//!   A through via stitches every layer at its position (v1: through only).
//! - pad ↔ pad: pad-pad distance 0 *and* a shared layer.
//!
//! ## Two fits, because the two answers are unsafe in opposite directions
//!
//! [`Obstacle`] records a pad only as its bounding box, which a round or oval
//! pad does not fill: its box corners are bare laminate. Believing them joins
//! copper that is physically apart — exactly how a trace that changes layer in a
//! pad's *corner* with no via can look connected. So the union-find is built
//! twice over the same elements ([`Fit`]):
//!
//! - [`Fit::Proven`] shrinks each pad to its inscribed capsule and answers
//!   [`Violation::Unconnected`], so a connection is only ever claimed on copper
//!   every plausible pad shape carries.
//! - [`Fit::Bounding`] keeps the box and answers [`Violation::CrossNetMerge`],
//!   so no short can hide in the slack.
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

use geom::EPS;
use pcb_model::{Capsule, LayerRef, RouteSolution, RoutingView, Violation};
use std::collections::{BTreeMap, BTreeSet};

/// Check that `solution`'s copper connects every connection's points and merges
/// no two distinct connections. Returns violations in deterministic order
/// (`Unconnected` first, by connection then point index; then `CrossNetMerge`
/// by name pair).
pub fn check(problem: &RoutingView, solution: &RouteSolution) -> Vec<Violation> {
    let elements = build_elements(problem, solution);
    let mut proven = UnionFind::new(elements.len());
    let mut bounding = UnionFind::new(elements.len());

    // O(n^2) pairwise touch test — element counts are tiny (a board's copper),
    // and clarity beats a spatial index here.
    for i in 0..elements.len() {
        for j in (i + 1)..elements.len() {
            if touches(&elements[i], &elements[j], Fit::Proven) {
                proven.union(i, j);
            }
            if touches(&elements[i], &elements[j], Fit::Bounding) {
                bounding.union(i, j);
            }
        }
    }

    for uf in [&mut proven, &mut bounding] {
        stitch_planes(problem, &elements, uf);
    }

    let mut violations = unconnected_violations(problem, &elements, &mut proven);
    violations.extend(cross_net_violations(&elements, &mut bounding));
    violations
}

/// Plane stitching: a net carried by a solid inner plane joins every through via
/// and every pad present on that plane (notably through-hole pads). Foreign
/// copper is relieved by anti-pads on a real board, so planes contribute only
/// same-net unions, never merges.
fn stitch_planes(problem: &RoutingView, elements: &[Element], uf: &mut UnionFind) {
    for (net, plane_layer) in &problem.plane_nets {
        let mut first: Option<usize> = None;
        for (idx, el) in elements.iter().enumerate() {
            if !el.owners.iter().any(|owner| owner == net) {
                continue;
            }
            let reaches_plane = match &el.shape {
                Shape::Via { .. } => true,
                Shape::Pad { layers, .. } => {
                    layers
                        .iter()
                        .any(|layer| layer.index(problem.layer_count) == Some(*plane_layer))
                        // Placement represents a plated through-hole pad by
                        // its outer copper faces; the barrel implicitly spans
                        // every inner plane between them.
                        || (layers
                            .iter()
                            .any(|layer| layer.index(problem.layer_count) == Some(0))
                            && layers.iter().any(|layer| {
                                layer.index(problem.layer_count)
                                    == problem.layer_count.checked_sub(1)
                            }))
                }
                _ => false,
            };
            if !reaches_plane {
                continue;
            }
            match first {
                None => first = Some(idx),
                Some(f) => {
                    uf.union(f, idx);
                }
            }
        }
    }
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
        segment: geom::Segment,
        half_w: f64,
        layer: LayerRef,
    },
    /// A pad present on a set of layers, in both fits (see [`Fit`]).
    Pad {
        rect: geom::Rect,
        capsule: Capsule,
        layers: Vec<LayerRef>,
    },
    /// A zero-size copper anchor (a `points_to_connect`) on one layer.
    Point { at: geom::Point2, layer: LayerRef },
    /// A through via: a disc that stitches every layer at its position.
    Via { at: geom::Point2, radius: f64 },
}

fn build_elements(problem: &RoutingView, solution: &RouteSolution) -> Vec<Element> {
    let mut els = Vec::new();

    // (1) Pads: obstacles owned by ≥1 connection.
    for ob in &problem.obstacles {
        if ob.connected_to.is_empty() {
            continue;
        }
        els.push(Element {
            owners: ob.connected_to.clone(),
            shared_pad: ob.connected_to.len() > 1,
            shape: Shape::Pad {
                rect: ob.bounds(),
                capsule: ob.proven_capsule(),
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
                    segment: geom::Segment::new(*p, *p),
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
                    segment: geom::Segment::new(w[0], w[1]),
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
                at: via.at,
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
                    at: geom::Point2::new(p.x, p.y),
                    layer: p.layer.clone(),
                },
            });
        }
    }

    els
}

// ── touch predicate ──────────────────────────────────────────────────────────

/// Which pad shape a touch test uses. See the module docs: a bounding box that
/// a rounded pad does not fill must not be believed when the answer is "these
/// are connected", and must not be shrunk when the answer is "these are apart".
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fit {
    /// Inscribed capsule — the copper every plausible pad shape carries.
    Proven,
    /// Bounding box — the copper no pad shape exceeds.
    Bounding,
}

/// Do two elements electrically touch under `fit`? See the module docs for the
/// per-pair rules. A via touches across *all* layers; everything else is
/// layer-checked.
fn touches(x: &Element, y: &Element, fit: Fit) -> bool {
    use Shape::*;
    match (&x.shape, &y.shape) {
        // via ↔ anything, any layer.
        (Via { at, radius }, other) | (other, Via { at, radius }) => {
            via_touches(*at, *radius, other, fit)
        }

        (
            Segment {
                segment: s1,
                half_w: w1,
                layer: l1,
            },
            Segment {
                segment: s2,
                half_w: w2,
                layer: l2,
            },
        ) => l1 == l2 && s1.dist_to_segment(*s2) <= w1 + w2 + EPS,

        (
            Segment {
                segment,
                half_w,
                layer,
            },
            pad @ Pad { layers, .. },
        )
        | (
            pad @ Pad { layers, .. },
            Segment {
                segment,
                half_w,
                layer,
            },
        ) => layers.contains(layer) && pad_dist_to_segment(pad, *segment, fit) <= half_w + EPS,

        (
            Segment {
                segment,
                half_w,
                layer,
            },
            Point { at, layer: pl },
        )
        | (
            Point { at, layer: pl },
            Segment {
                segment,
                half_w,
                layer,
            },
        ) => layer == pl && segment.dist_to_point(*at) <= half_w + EPS,

        (pad @ Pad { layers, .. }, Point { at, layer })
        | (Point { at, layer }, pad @ Pad { layers, .. }) => {
            layers.contains(layer) && pad_dist_to_point(pad, *at, fit) <= EPS
        }

        (
            Pad {
                rect: r1,
                capsule: c1,
                layers: l1,
            },
            Pad {
                rect: r2,
                capsule: c2,
                layers: l2,
            },
        ) => {
            l1.iter().any(|l| l2.contains(l))
                && match fit {
                    Fit::Proven => c1.dist_to_capsule(c2),
                    Fit::Bounding => r1.dist_to_rect(r2),
                } <= EPS
        }

        // point ↔ point: zero-size anchors never touch each other directly;
        // they are only ever joined through real copper.
        (Point { .. }, Point { .. }) => false,
    }
}

/// Distance from a [`Shape::Pad`]'s copper to `p` under `fit`; `f64::MAX` for
/// any other shape, so a caller that mismatched the arm can never report a
/// touch.
fn pad_dist_to_point(pad: &Shape, p: geom::Point2, fit: Fit) -> f64 {
    match (pad, fit) {
        (Shape::Pad { capsule, .. }, Fit::Proven) => capsule.dist_to_point(p),
        (Shape::Pad { rect, .. }, Fit::Bounding) => rect.dist_to_point(p),
        _ => f64::MAX,
    }
}

fn pad_dist_to_segment(pad: &Shape, s: geom::Segment, fit: Fit) -> f64 {
    match (pad, fit) {
        (Shape::Pad { capsule, .. }, Fit::Proven) => capsule.dist_to_segment(s),
        (Shape::Pad { rect, .. }, Fit::Bounding) => s.dist_to_rect(rect),
        _ => f64::MAX,
    }
}

/// A through via at `at` with `radius` touches `other` on any layer when the
/// disc reaches the other element's fattened body.
fn via_touches(at: geom::Point2, radius: f64, other: &Shape, fit: Fit) -> bool {
    match other {
        Shape::Segment {
            segment, half_w, ..
        } => segment.dist_to_point(at) <= radius + half_w + EPS,
        pad @ Shape::Pad { .. } => pad_dist_to_point(pad, at, fit) <= radius + EPS,
        Shape::Point { at: p, .. } => at.dist(*p) <= radius + EPS,
        Shape::Via { at: p, radius: r2 } => at.dist(*p) <= radius + r2 + EPS,
    }
}

// The indexed disjoint-set forest lives in `pcb_model` (union by rank, path
// halving), shared with any other PCB caller.
use pcb_model::UnionFind;

// ── violation extraction ─────────────────────────────────────────────────────

/// For each connection, the elements representing its `points_to_connect` were
/// appended in order; recover them and check they share point 0's root.
fn unconnected_violations(
    problem: &RoutingView,
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

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use pcb_model::{
        Connection, Obstacle, Point2, Rect, RoutePoint, RouteSolution, RoutingView, Trace, Via,
        ViaSpan,
    };

    fn bounds() -> Rect {
        Rect {
            min_x: -100.0,
            max_x: 100.0,
            min_y: -100.0,
            max_y: 100.0,
        }
    }

    fn problem(connections: Vec<Connection>, obstacles: Vec<Obstacle>) -> RoutingView {
        RoutingView {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles,
            connections,
            bounds: bounds(),
            clearance: 0.2,
            via_diameter: 0.6,
            via_drill: 0.3,
            net_widths: Default::default(),
            outline: None,
            escape_layers: Default::default(),
            plane_nets: Default::default(),
            fixed_copper: Default::default(),
            nets: None,
        }
    }

    #[test]
    fn plane_joins_through_hole_pad_to_fanout_via() {
        let mut p = problem(
            vec![conn("GND", &[(2.0, 2.0, "top"), (8.0, 2.0, "top")])],
            vec![
                pad(
                    &["GND"],
                    (2.0, 2.0),
                    1.0,
                    1.0,
                    // The plated barrel is implicit when both outer copper
                    // faces are present; inner layers are not enumerated.
                    &["top", "bottom"],
                ),
                pad(&["GND"], (8.0, 2.0), 1.0, 1.0, &["top"]),
            ],
        );
        p.layer_count = 4;
        p.plane_nets.insert("GND".to_owned(), 1);
        let solution = RouteSolution {
            traces: vec![],
            vias: vec![Via {
                connection: "GND".to_owned(),
                at: Point2 { x: 8.0, y: 2.0 },
                diameter: 0.6,
                drill: 0.3,
                span: ViaSpan::Through,
            }],
        };

        assert!(check(&p, &solution).is_empty());
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
            span: ViaSpan::Through,
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
    fn layer_change_in_a_round_pads_box_corner_is_unconnected() {
        // A 1.7mm round thru-hole pad drawn as its 1.7x1.7 bounding box. The
        // route leaves the pad on top, hops to bottom at (16.325, 18.875) — a
        // box corner, 1.066mm from the centre, so outside the real 0.85mm
        // copper — and carries on. Nothing stitches the layers there.
        let p = problem(
            vec![conn(
                "SIG",
                &[(15.5, 19.55, "top"), (19.625, 18.425, "bottom")],
            )],
            vec![pad(&["SIG"], (15.5, 19.55), 1.7, 1.7, &["top", "bottom"])],
        );
        let unstitched = RouteSolution {
            traces: vec![
                trace("SIG", "top", 0.15, &[(15.5, 19.55), (16.325, 18.875)]),
                trace("SIG", "bottom", 0.15, &[(16.325, 18.875), (19.625, 18.425)]),
            ],
            vias: vec![],
        };
        assert_eq!(
            check(&p, &unstitched),
            vec![Violation::Unconnected {
                connection: "SIG".to_owned(),
                point_index: 1,
            }]
        );

        // The via the router owed us makes it whole.
        assert_eq!(
            check(
                &p,
                &RouteSolution {
                    vias: vec![via("SIG", (16.325, 18.875))],
                    ..unstitched
                }
            ),
            vec![]
        );
    }

    #[test]
    fn foreign_copper_in_a_pads_box_corner_still_merges() {
        // The other direction: a short must never hide in the slack between a
        // pad's real shape and its box, so merges keep the bounding fit.
        let p = problem(
            vec![
                conn("SIG", &[(0.0, 0.0, "top")]),
                conn("GND", &[(5.0, 5.0, "top")]),
            ],
            vec![pad(&["SIG"], (0.0, 0.0), 1.7, 1.7, &["top"])],
        );
        let s = RouteSolution {
            traces: vec![trace("GND", "top", 0.15, &[(5.0, 5.0), (0.825, 0.825)])],
            vias: vec![],
        };
        assert!(check(&p, &s).contains(&Violation::CrossNetMerge {
            a: "GND".to_owned(),
            b: "SIG".to_owned(),
        }));
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
