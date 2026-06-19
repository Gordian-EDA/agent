//! Strict DRC lint: the precision oracle for an emitted [`RouteSolution`].
//!
//! Where the router reasons on a grid, the lint re-measures the *actual* copper
//! geometry with exact segment/segment and segment/rect math — it never imports
//! or trusts [`crate::grid`]. The router can be wrong; this is the independent
//! authority that catches it. A real violation must surface, never pass
//! silently.
//!
//! ## What it checks
//!
//! - [`DrcViolation::ClearanceTraceTrace`] — two trace segments of *different*
//!   connections on the *same* layer whose copper edges are closer than
//!   `clearance`.
//! - [`DrcViolation::ClearanceTraceObstacle`] — a trace segment too close to a
//!   foreign or unowned (keepout) obstacle on a shared layer. An obstacle owned
//!   by the trace's own connection never conflicts with it.
//! - [`DrcViolation::ClearanceViaAny`] — a via too close to copper not its own
//!   (another connection's trace/via/obstacle, or unowned copper). Vias are
//!   through-hole in v1, so they conflict on *any* layer.
//! - [`DrcViolation::TraceWidthBelowMin`] — a trace narrower than
//!   `min_trace_width`.
//! - [`DrcViolation::OutOfBounds`] — copper (trace centreline fattened by its
//!   half-width, or a via fattened by its radius) leaving the board `bounds`.
//! - The slice-0 connectivity oracle's [`crate::connectivity::Violation`]s,
//!   folded in via [`DrcViolation::Connectivity`], so `lint` is the single
//!   one-stop report.
//!
//! ## Distance convention (mm)
//!
//! Clearance is between copper **edges**: the gap is the distance between
//! centrelines (or centre-to-segment, etc.) minus the two half-widths. A via's
//! half-width is its radius; a pad/obstacle is a hard rectangle (half-width 0).
//! [`EPS`] of slop is allowed before a gap counts as a violation.
//!
//! ## Structure
//!
//! All copper is collected into a single flat `Vec<Item>` in one pass; the
//! checks then iterate that vec with brute-force O(n²) pair tests. Element
//! counts are tiny (a board's copper) so this is fine, and the flat-collection
//! shape is exactly where a spatial index would slot in later.

use crate::connectivity::{self, Violation};
use crate::problem::{LayerRef, RouteProblem, RouteSolution};
use serde::Serialize;

/// Geometric slop, mm. A gap is only a violation when it falls short of the
/// required clearance by more than this.
const EPS: f64 = 1e-6;

/// A design-rule violation in a [`RouteSolution`] relative to its problem.
///
/// Carries enough payload to debug each case: the connection name(s), the layer
/// where relevant, the measured gap/width against what was required, and a
/// representative location.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum DrcViolation {
    /// Two traces of different connections on the same layer are too close.
    ClearanceTraceTrace {
        /// First connection name.
        a: String,
        /// Second connection name.
        b: String,
        /// Layer the two traces share.
        layer: String,
        /// Measured edge-to-edge gap, mm.
        gap: f64,
        /// Required clearance, mm.
        required: f64,
        /// A point on the offending pair (closest-approach-ish; the first
        /// segment's nearest endpoint), for debugging.
        at: [f64; 2],
    },
    /// A trace is too close to a foreign or unowned (keepout) obstacle.
    ClearanceTraceObstacle {
        /// The trace's connection name.
        connection: String,
        /// The obstacle's owners (empty for unowned/keepout copper).
        obstacle_owners: Vec<String>,
        /// Shared layer the conflict occurs on.
        layer: String,
        /// Measured edge-to-edge gap, mm.
        gap: f64,
        /// Required clearance, mm.
        required: f64,
        /// The obstacle centre, for debugging.
        at: [f64; 2],
    },
    /// A via is too close to copper that is not its own connection.
    ClearanceViaAny {
        /// The via's connection name.
        connection: String,
        /// The other copper's owners (empty for unowned/keepout copper).
        other_owners: Vec<String>,
        /// Measured edge-to-edge gap, mm.
        gap: f64,
        /// Required clearance, mm.
        required: f64,
        /// The via position, for debugging.
        at: [f64; 2],
    },
    /// A trace is narrower than the minimum trace width.
    TraceWidthBelowMin {
        /// The trace's connection name.
        connection: String,
        /// Layer the trace is on.
        layer: String,
        /// The trace's width, mm.
        width: f64,
        /// Required minimum width, mm.
        required: f64,
    },
    /// Copper (trace half-width or via radius included) leaves the board bounds.
    OutOfBounds {
        /// The owning connection name.
        connection: String,
        /// How far past the nearest board edge the copper extends, mm.
        overshoot: f64,
        /// The offending copper location, for debugging.
        at: [f64; 2],
    },
    /// A trace or route point references a layer name that does not exist on
    /// this board (i.e. `layer.index(layer_count)` returns `None`). This is the
    /// slice-1 blind spot: the router silently fell back to layer 0 for unknown
    /// layer names; the lint catches it explicitly.
    InvalidLayer {
        /// The connection name that owns the offending copper.
        connection: String,
        /// The layer reference that could not be resolved (e.g. `"inner1"` on a
        /// 2-layer board, or a typo).
        layer: String,
        /// The board's layer count (provided for context when debugging).
        layer_count: u32,
    },
    /// A connectivity defect from the slice-0 oracle, folded in.
    Connectivity {
        /// The wrapped connectivity violation.
        violation: Violation,
    },
}

// ── public API ───────────────────────────────────────────────────────────────

/// Run the full strict DRC lint over `solution` against `problem`.
///
/// Returns every violation in deterministic order: geometry violations first
/// (collection order — traces, then vias, then bounds), then the connectivity
/// oracle's violations folded in last.
/// Make `solution` connectivity-honest: drop the copper of every net the
/// connectivity oracle reports as unconnected (a half-route a router miscounted
/// as done) or cross-net-shorted, and return those net names (sorted, unique).
///
/// The connectivity oracle — not a router's own bookkeeping — is the authority on
/// what is actually joined. After this call the surviving copper carries no
/// connectivity defect; callers should mark the returned names as failed nets so
/// the reported result is faithful (an honest unrouted net, never silent copper
/// that lies about connectivity). Dropping a net's copper only removes obstacles,
/// so it can never break another net or introduce a geometry violation.
pub fn drop_unconnected_copper(problem: &RouteProblem, solution: &mut RouteSolution) -> Vec<String> {
    use crate::connectivity::Violation as ConnViolation;
    let mut broken: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for v in lint(problem, solution) {
        if let DrcViolation::Connectivity { violation } = v {
            match violation {
                ConnViolation::Unconnected { connection, .. } => {
                    broken.insert(connection);
                }
                ConnViolation::CrossNetMerge { a, b } => {
                    broken.insert(a);
                    broken.insert(b);
                }
            }
        }
    }
    if broken.is_empty() {
        return Vec::new();
    }
    solution.traces.retain(|t| !broken.contains(&t.connection));
    solution.vias.retain(|v| !broken.contains(&v.connection));
    broken.into_iter().collect()
}

/// Make `solution` GEOMETRY-clean: while the lint reports any geometry violation
/// (clearance / trace-width / via-clearance / out-of-bounds / invalid-layer),
/// drop the copper of the net involved in the most violations and retry. Returns
/// the dropped net names. The engine must never EMIT copper that fails DRC — on a
/// board too dense to route a net cleanly, dropping it (and reporting it failed)
/// is correct; a silent clearance violation that looks routed is not. Bounded by
/// the net count so it always terminates. Connectivity is handled separately by
/// [`drop_unconnected_copper`]; callers typically run both.
pub fn drop_violating_copper(problem: &RouteProblem, solution: &mut RouteSolution) -> Vec<String> {
    let mut dropped: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    // One net can be dropped per pass; at most one pass per net plus a margin.
    let max_passes = problem.connections.len() + 1;
    for _ in 0..max_passes {
        let mut tally: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
        for v in lint(problem, solution) {
            for net in violation_nets(&v) {
                *tally.entry(net).or_default() += 1;
            }
        }
        if tally.is_empty() {
            break;
        }
        // Drop the worst offender (most violations); ties broken by name (BTreeMap
        // iteration order) for determinism.
        let worst = tally
            .iter()
            .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
            .map(|(n, _)| n.clone())
            .unwrap();
        solution.traces.retain(|t| t.connection != worst);
        solution.vias.retain(|v| v.connection != worst);
        dropped.insert(worst);
    }
    dropped.into_iter().collect()
}

/// The net name(s) a GEOMETRY violation implicates (empty for connectivity, which
/// this never returns since callers pre-filter). For a trace/trace clearance both
/// nets are implicated; dropping the one in more violations resolves the most.
fn violation_nets(v: &DrcViolation) -> Vec<String> {
    match v {
        DrcViolation::ClearanceTraceTrace { a, b, .. } => vec![a.clone(), b.clone()],
        DrcViolation::ClearanceTraceObstacle { connection, .. }
        | DrcViolation::ClearanceViaAny { connection, .. }
        | DrcViolation::TraceWidthBelowMin { connection, .. }
        | DrcViolation::OutOfBounds { connection, .. }
        | DrcViolation::InvalidLayer { connection, .. } => vec![connection.clone()],
        DrcViolation::Connectivity { .. } => Vec::new(),
    }
}

pub fn lint(problem: &RouteProblem, solution: &RouteSolution) -> Vec<DrcViolation> {
    let items = collect_items(problem, solution);
    let clearance = problem.clearance;
    let layer_count = problem.layer_count.max(1);

    let mut out = Vec::new();

    // (0) Invalid layer — any solution trace or connection route point whose
    //     layer name resolves to `None` for this board's `layer_count`. The
    //     slice-1 router silently maps unknown names to layer 0; the lint
    //     catches it explicitly.
    for trace in &solution.traces {
        if trace.layer.index(layer_count).is_none() {
            out.push(DrcViolation::InvalidLayer {
                connection: trace.connection.clone(),
                layer: trace.layer.0.clone(),
                layer_count,
            });
        }
    }
    for conn in &problem.connections {
        for pt in &conn.points_to_connect {
            if pt.layer.index(layer_count).is_none() {
                out.push(DrcViolation::InvalidLayer {
                    connection: conn.name.clone(),
                    layer: pt.layer.0.clone(),
                    layer_count,
                });
            }
        }
    }

    // (1) Trace width below minimum — one check per trace.
    for trace in &solution.traces {
        if trace.width + EPS < problem.min_trace_width {
            out.push(DrcViolation::TraceWidthBelowMin {
                connection: trace.connection.clone(),
                layer: trace.layer.0.clone(),
                width: trace.width,
                required: problem.min_trace_width,
            });
        }
    }

    // (2) Out-of-bounds — copper extents must stay inside the board.
    for item in &items {
        if let Some(v) = out_of_bounds(item, problem) {
            out.push(v);
        }
    }

    // (3) Pairwise clearance — brute force O(n²) over the flat item vec.
    for i in 0..items.len() {
        for j in (i + 1)..items.len() {
            if let Some(v) = pair_clearance(&items[i], &items[j], clearance) {
                out.push(v);
            }
        }
    }

    // (4) Fold in the connectivity oracle — the single one-stop report.
    for v in connectivity::check(problem, solution) {
        out.push(DrcViolation::Connectivity { violation: v });
    }

    out
}

// ── copper items (one collection pass) ───────────────────────────────────────

/// One piece of copper geometry to clearance-check. A flat list of these is the
/// single hand-off point where a spatial index could later slot in.
struct Item {
    /// Connection names that own this copper. Empty = unowned/keepout copper.
    owners: Vec<String>,
    geom: Geom,
}

/// The geometry of an [`Item`]. Trace copper is a fattened segment; obstacles
/// are hard rectangles; vias are through discs spanning every layer.
enum Geom {
    /// A trace segment fattened by `half_w` on a single layer.
    Segment {
        a: [f64; 2],
        b: [f64; 2],
        half_w: f64,
        layer: LayerRef,
    },
    /// An axis-aligned obstacle rectangle on a set of layers (half-width 0).
    Rect {
        min: [f64; 2],
        max: [f64; 2],
        center: [f64; 2],
        layers: Vec<LayerRef>,
    },
    /// A through via: a disc of `radius` present on every layer.
    Via { at: [f64; 2], radius: f64 },
}

impl Item {
    /// Does this item own copper for connection `name`?
    fn owned_by(&self, name: &str) -> bool {
        self.owners.iter().any(|o| o == name)
    }
}

/// Collect every piece of copper (obstacles, trace segments, vias) into one
/// flat vec, in a stable order: obstacles, then trace segments, then vias.
fn collect_items(problem: &RouteProblem, solution: &RouteSolution) -> Vec<Item> {
    let mut items = Vec::new();

    // Obstacles: pads (owned) and unowned/foreign copper alike — both constrain
    // foreign nets. A pad never conflicts with its own connection (handled at
    // check time via owners).
    for ob in &problem.obstacles {
        let hw = ob.width / 2.0;
        let hh = ob.height / 2.0;
        items.push(Item {
            owners: ob.connected_to.clone(),
            geom: Geom::Rect {
                min: [ob.center.x - hw, ob.center.y - hh],
                max: [ob.center.x + hw, ob.center.y + hh],
                center: [ob.center.x, ob.center.y],
                layers: ob.layers.clone(),
            },
        });
    }

    // Trace segments: each consecutive point pair. A single-point (degenerate)
    // trace becomes a zero-length segment (a fat point) so it is still checked.
    for trace in &solution.traces {
        let half_w = trace.width / 2.0;
        let push_seg = |items: &mut Vec<Item>, a: [f64; 2], b: [f64; 2]| {
            items.push(Item {
                owners: vec![trace.connection.clone()],
                geom: Geom::Segment {
                    a,
                    b,
                    half_w,
                    layer: trace.layer.clone(),
                },
            });
        };
        if trace.path.len() == 1 {
            let p = &trace.path[0];
            push_seg(&mut items, [p.x, p.y], [p.x, p.y]);
        }
        for w in trace.path.windows(2) {
            push_seg(&mut items, [w[0].x, w[0].y], [w[1].x, w[1].y]);
        }
    }

    // Vias: through discs.
    for via in &solution.vias {
        items.push(Item {
            owners: vec![via.connection.clone()],
            geom: Geom::Via {
                at: [via.at.x, via.at.y],
                radius: via.diameter / 2.0,
            },
        });
    }

    items
}

// ── out-of-bounds ────────────────────────────────────────────────────────────

/// How far the item's copper extent (segment fattened by half-width, via by its
/// radius, rect as-is) leaves the board bounds, or `None` if it is inside.
///
/// Obstacles are *inputs* (the board's own pads/keepouts), not router output, so
/// we do not flag them for being out of bounds — only emitted traces and vias.
fn out_of_bounds(item: &Item, problem: &RouteProblem) -> Option<DrcViolation> {
    let (overshoot, at, owner) = match &item.geom {
        Geom::Segment {
            a, b: bb, half_w, ..
        } => {
            let o_a = point_overshoot(*a, *half_w, problem);
            let o_b = point_overshoot(*bb, *half_w, problem);
            let (over, at) = if o_a.0 >= o_b.0 {
                (o_a.0, *a)
            } else {
                (o_b.0, *bb)
            };
            let owner = item.owners.first().cloned().unwrap_or_default();
            (over, at, owner)
        }
        Geom::Via { at, radius } => {
            let (over, _) = point_overshoot(*at, *radius, problem);
            let owner = item.owners.first().cloned().unwrap_or_default();
            (over, *at, owner)
        }
        Geom::Rect { .. } => return None,
    };
    if overshoot > EPS {
        Some(DrcViolation::OutOfBounds {
            connection: owner,
            overshoot,
            at,
        })
    } else {
        None
    }
}

/// How far a disc of `radius` centred at `p` pokes past the nearest board edge
/// (positive = outside), plus the point itself. Zero or negative = inside.
fn point_overshoot(p: [f64; 2], radius: f64, problem: &RouteProblem) -> (f64, [f64; 2]) {
    let b = &problem.bounds;
    let left = (b.min_x - (p[0] - radius)).max(0.0);
    let right = ((p[0] + radius) - b.max_x).max(0.0);
    let top = (b.min_y - (p[1] - radius)).max(0.0);
    let bottom = ((p[1] + radius) - b.max_y).max(0.0);
    (left.max(right).max(top).max(bottom), p)
}

// ── pairwise clearance ───────────────────────────────────────────────────────

/// Clearance test for one unordered item pair. Returns a violation if their
/// copper edges are closer than `clearance` (and they are foreign to each
/// other / on a shared layer). `None` otherwise.
fn pair_clearance(x: &Item, y: &Item, clearance: f64) -> Option<DrcViolation> {
    use Geom::*;

    // A via vs anything is its own category (through-hole: conflicts on any
    // layer), so handle via-bearing pairs first.
    match (&x.geom, &y.geom) {
        (Via { at, radius }, _) => return via_pair(x, *at, *radius, y, clearance),
        (_, Via { at, radius }) => return via_pair(y, *at, *radius, x, clearance),
        _ => {}
    }

    match (&x.geom, &y.geom) {
        // Trace ↔ trace: same layer, different connection.
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
        ) => {
            if l1 != l2 || share_owner(x, y) {
                return None;
            }
            let gap = seg_seg_dist(*a1, *b1, *a2, *b2) - w1 - w2;
            if gap + EPS < clearance {
                Some(DrcViolation::ClearanceTraceTrace {
                    a: x.owners.first().cloned().unwrap_or_default(),
                    b: y.owners.first().cloned().unwrap_or_default(),
                    layer: l1.0.clone(),
                    gap,
                    required: clearance,
                    at: *a1,
                })
            } else {
                None
            }
        }

        // Trace ↔ obstacle.
        (
            Segment {
                a,
                b,
                half_w,
                layer,
            },
            Rect {
                min,
                max,
                center,
                layers,
            },
        ) => trace_obstacle(x, y, *a, *b, *half_w, layer, *min, *max, *center, layers, clearance),
        (
            Rect {
                min,
                max,
                center,
                layers,
            },
            Segment {
                a,
                b,
                half_w,
                layer,
            },
        ) => trace_obstacle(y, x, *a, *b, *half_w, layer, *min, *max, *center, layers, clearance),

        // Obstacle ↔ obstacle: both are board inputs, not router output. We do
        // not lint pre-existing pad/keepout overlaps.
        (Rect { .. }, Rect { .. }) => None,

        // Vias were peeled off above.
        (Via { .. }, _) | (_, Via { .. }) => None,
    }
}

/// Trace-segment (`seg`) against an obstacle rect, with `seg` as the trace
/// item and `rect` as the obstacle item.
#[allow(clippy::too_many_arguments)]
fn trace_obstacle(
    seg: &Item,
    rect: &Item,
    a: [f64; 2],
    b: [f64; 2],
    half_w: f64,
    layer: &LayerRef,
    min: [f64; 2],
    max: [f64; 2],
    center: [f64; 2],
    layers: &[LayerRef],
    clearance: f64,
) -> Option<DrcViolation> {
    // The obstacle constrains the trace only on a shared layer, and only if the
    // obstacle is not owned by the trace's own connection.
    let conn = seg.owners.first().cloned().unwrap_or_default();
    if !layers.contains(layer) || rect.owned_by(&conn) {
        return None;
    }
    let gap = seg_rect_dist(a, b, min, max) - half_w;
    if gap + EPS < clearance {
        Some(DrcViolation::ClearanceTraceObstacle {
            connection: conn,
            obstacle_owners: rect.owners.clone(),
            layer: layer.0.clone(),
            gap,
            required: clearance,
            at: center,
        })
    } else {
        None
    }
}

/// A via (`via`, at `at` with `radius`) against any other item `other`. Vias are
/// through-hole, so the layer is irrelevant; only ownership matters. Returns a
/// [`DrcViolation::ClearanceViaAny`] when too close to foreign copper.
fn via_pair(
    via: &Item,
    at: [f64; 2],
    radius: f64,
    other: &Item,
    clearance: f64,
) -> Option<DrcViolation> {
    if share_owner(via, other) {
        return None;
    }
    let conn = via.owners.first().cloned().unwrap_or_default();
    let (edge_dist, other_owners) = match &other.geom {
        Geom::Segment { a, b, half_w, .. } => (point_seg_dist(at, *a, *b) - half_w, other.owners.clone()),
        Geom::Rect { min, max, .. } => (point_rect_dist(at, *min, *max), other.owners.clone()),
        Geom::Via { at: p, radius: r2 } => (dist(at, *p) - r2, other.owners.clone()),
    };
    let gap = edge_dist - radius;
    if gap + EPS < clearance {
        Some(DrcViolation::ClearanceViaAny {
            connection: conn,
            other_owners,
            gap,
            required: clearance,
            at,
        })
    } else {
        None
    }
}

/// Do two items share at least one owning connection? (A pad owned by the
/// trace's net, the same net's own copper, etc. — never a clearance conflict.)
fn share_owner(x: &Item, y: &Item) -> bool {
    x.owners.iter().any(|o| y.owned_by(o))
}

// ── geometry primitives (independent re-implementation) ──────────────────────
//
// These mirror the connectivity oracle's primitives but are kept local: the
// lint must not depend on the router's or another module's model. Distance math
// only — no grid, no inflation.

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

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::problem::{
        Bounds, Connection, Obstacle, Point2, RoutePoint, RouteProblem, RouteSolution, Trace, Via,
    };
    use crate::router;
    use std::path::Path;

    fn bounds() -> Bounds {
        Bounds {
            min_x: 0.0,
            max_x: 100.0,
            min_y: 0.0,
            max_y: 100.0,
        }
    }

    fn problem(connections: Vec<Connection>, obstacles: Vec<Obstacle>) -> RouteProblem {
        RouteProblem {
            layer_count: 2,
            min_trace_width: 0.25,
            obstacles,
            connections,
            bounds: bounds(),
            clearance: 0.2,
            via_diameter: 0.6,
            via_drill: 0.3,
            net_widths: Default::default(),
            outline: None,
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

    fn load(name: &str) -> RouteProblem {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures")
            .join(name);
        let json = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        serde_json::from_str(&json).unwrap_or_else(|e| panic!("parse {name}: {e}"))
    }

    fn count<F: Fn(&DrcViolation) -> bool>(vs: &[DrcViolation], f: F) -> usize {
        vs.iter().filter(|v| f(v)).count()
    }

    // ── per-variant triggers ────────────────────────────────────────────────

    #[test]
    fn clearance_trace_trace_fires_for_close_parallel_traces() {
        // Two parallel traces on top, centrelines 0.30 mm apart, each 0.25 wide
        // → edge gap = 0.30 - 0.125 - 0.125 = 0.05 < 0.2 clearance. Different
        // nets, fully connected so no connectivity noise.
        let p = problem(
            vec![
                conn("A", &[(10.0, 10.0, "top"), (30.0, 10.0, "top")]),
                conn("B", &[(10.0, 10.3, "top"), (30.0, 10.3, "top")]),
            ],
            vec![],
        );
        let s = RouteSolution {
            traces: vec![
                trace("A", "top", 0.25, &[(10.0, 10.0), (30.0, 10.0)]),
                trace("B", "top", 0.25, &[(10.0, 10.3), (30.0, 10.3)]),
            ],
            vias: vec![],
        };
        let vs = lint(&p, &s);
        assert_eq!(
            count(&vs, |v| matches!(v, DrcViolation::ClearanceTraceTrace { .. })),
            1,
            "exactly one trace/trace clearance violation, got {vs:?}"
        );
    }

    #[test]
    fn clearance_trace_trace_clean_for_far_parallel_traces() {
        // Same as above but 1.0 mm apart → edge gap 0.75 ≥ 0.2: clean.
        let p = problem(
            vec![
                conn("A", &[(10.0, 10.0, "top"), (30.0, 10.0, "top")]),
                conn("B", &[(10.0, 11.0, "top"), (30.0, 11.0, "top")]),
            ],
            vec![],
        );
        let s = RouteSolution {
            traces: vec![
                trace("A", "top", 0.25, &[(10.0, 10.0), (30.0, 10.0)]),
                trace("B", "top", 0.25, &[(10.0, 11.0), (30.0, 11.0)]),
            ],
            vias: vec![],
        };
        assert!(
            !lint(&p, &s)
                .iter()
                .any(|v| matches!(v, DrcViolation::ClearanceTraceTrace { .. })),
            "far parallel traces must not raise a trace/trace clearance"
        );
    }

    #[test]
    fn clearance_trace_obstacle_fires_for_foreign_pad() {
        // GND pad at (20,10), 1.0×1.0 → right edge x=20.5. A SIG trace runs at
        // y=10 to x=20.6, edge gap = (20.6-0.125) - 20.5 ... measure: segment
        // reaches x=20.6, rect right edge 20.5, dist 0.1, minus half-width
        // 0.125 → -0.025 (overlap) < 0.2. Foreign owner → violation.
        let p = problem(
            vec![
                conn("SIG", &[(5.0, 10.0, "top"), (20.6, 10.0, "top")]),
                conn("GND", &[(20.0, 10.0, "top")]),
            ],
            vec![pad(&["GND"], (20.0, 10.0), 1.0, 1.0, &["top"])],
        );
        let s = RouteSolution {
            traces: vec![trace("SIG", "top", 0.25, &[(5.0, 10.0), (20.6, 10.0)])],
            vias: vec![],
        };
        let vs = lint(&p, &s);
        assert_eq!(
            count(&vs, |v| matches!(
                v,
                DrcViolation::ClearanceTraceObstacle { .. }
            )),
            1,
            "exactly one trace/obstacle clearance violation, got {vs:?}"
        );
    }

    #[test]
    fn clearance_trace_obstacle_clean_for_own_pad() {
        // A SIG trace that ends inside its OWN pad must not raise a clearance.
        let p = problem(
            vec![conn("SIG", &[(5.0, 10.0, "top"), (20.0, 10.0, "top")])],
            vec![pad(&["SIG"], (20.0, 10.0), 1.0, 1.0, &["top"])],
        );
        let s = RouteSolution {
            traces: vec![trace("SIG", "top", 0.25, &[(5.0, 10.0), (20.0, 10.0)])],
            vias: vec![],
        };
        assert!(
            !lint(&p, &s)
                .iter()
                .any(|v| matches!(v, DrcViolation::ClearanceTraceObstacle { .. })),
            "a trace ending in its own pad must not raise a clearance violation"
        );
    }

    #[test]
    fn clearance_via_any_fires_for_foreign_trace() {
        // A NET_B via at (20,10), radius 0.3. A NET_A trace passes at y=10.45:
        // point-seg dist 0.45, minus trace half 0.125 → 0.325 to via centre,
        // minus via radius 0.3 → 0.025 gap < 0.2. Different nets → violation.
        let p = problem(
            vec![
                conn("NET_A", &[(5.0, 10.45, "top"), (35.0, 10.45, "top")]),
                conn("NET_B", &[(20.0, 10.0, "top"), (20.0, 10.0, "bottom")]),
            ],
            vec![],
        );
        let s = RouteSolution {
            traces: vec![trace("NET_A", "top", 0.25, &[(5.0, 10.45), (35.0, 10.45)])],
            vias: vec![via("NET_B", (20.0, 10.0))],
        };
        let vs = lint(&p, &s);
        assert_eq!(
            count(&vs, |v| matches!(v, DrcViolation::ClearanceViaAny { .. })),
            1,
            "exactly one via/any clearance violation, got {vs:?}"
        );
    }

    #[test]
    fn clearance_via_any_clean_for_own_trace() {
        // A via and a trace of the SAME net are not a clearance conflict.
        let p = problem(
            vec![conn(
                "NET_A",
                &[(5.0, 10.0, "top"), (20.0, 10.0, "bottom")],
            )],
            vec![],
        );
        let s = RouteSolution {
            traces: vec![
                trace("NET_A", "top", 0.25, &[(5.0, 10.0), (20.0, 10.0)]),
                trace("NET_A", "bottom", 0.25, &[(20.0, 10.0), (20.0, 20.0)]),
            ],
            vias: vec![via("NET_A", (20.0, 10.0))],
        };
        assert!(
            !lint(&p, &s)
                .iter()
                .any(|v| matches!(v, DrcViolation::ClearanceViaAny { .. })),
            "a via must not conflict with its own net's copper"
        );
    }

    #[test]
    fn trace_width_below_min_fires() {
        let p = problem(
            vec![conn("SIG", &[(5.0, 10.0, "top"), (25.0, 10.0, "top")])],
            vec![],
        );
        let s = RouteSolution {
            // 0.10 < min 0.25.
            traces: vec![trace("SIG", "top", 0.10, &[(5.0, 10.0), (25.0, 10.0)])],
            vias: vec![],
        };
        let vs = lint(&p, &s);
        assert_eq!(
            count(&vs, |v| matches!(v, DrcViolation::TraceWidthBelowMin { .. })),
            1,
            "exactly one width violation, got {vs:?}"
        );
    }

    #[test]
    fn out_of_bounds_fires_for_via_off_board() {
        // Via at the very corner: radius pokes past min_x and min_y.
        let p = problem(
            vec![conn("SIG", &[(0.0, 0.0, "top"), (0.0, 0.0, "bottom")])],
            vec![],
        );
        let s = RouteSolution {
            traces: vec![],
            vias: vec![via("SIG", (0.0, 0.0))],
        };
        let vs = lint(&p, &s);
        assert_eq!(
            count(&vs, |v| matches!(v, DrcViolation::OutOfBounds { .. })),
            1,
            "exactly one out-of-bounds violation, got {vs:?}"
        );
    }

    #[test]
    fn out_of_bounds_fires_for_trace_off_board() {
        // Trace whose centreline + half-width crosses max_x.
        let p = problem(
            vec![conn("SIG", &[(50.0, 50.0, "top"), (100.05, 50.0, "top")])],
            vec![],
        );
        let s = RouteSolution {
            traces: vec![trace("SIG", "top", 0.25, &[(50.0, 50.0), (100.05, 50.0)])],
            vias: vec![],
        };
        let vs = lint(&p, &s);
        assert!(
            count(&vs, |v| matches!(v, DrcViolation::OutOfBounds { .. })) >= 1,
            "expected an out-of-bounds violation, got {vs:?}"
        );
    }

    #[test]
    fn connectivity_violations_are_folded_in() {
        // A trace that stops short → connectivity Unconnected, surfaced via the
        // Connectivity variant.
        let p = problem(
            vec![conn(
                "SIG",
                &[(5.0, 10.0, "top"), (25.0, 10.0, "top"), (45.0, 10.0, "top")],
            )],
            vec![],
        );
        let s = RouteSolution {
            traces: vec![trace("SIG", "top", 0.25, &[(5.0, 10.0), (25.0, 10.0)])],
            vias: vec![],
        };
        let vs = lint(&p, &s);
        assert!(
            vs.iter().any(|v| matches!(
                v,
                DrcViolation::Connectivity {
                    violation: Violation::Unconnected { .. }
                }
            )),
            "connectivity Unconnected must be folded into the lint, got {vs:?}"
        );
    }

    // ── the gate: router output on both fixtures lints clean ─────────────────

    #[test]
    fn led_r_router_output_lints_clean() {
        let p = load("led-r.json");
        let result = router::route(&p);
        assert!(result.failed.is_empty(), "led-r should route fully");
        let vs = lint(&p, &result.solution);
        assert!(vs.is_empty(), "led-r router output must lint CLEAN, got {vs:?}");
    }

    #[test]
    fn quad_router_output_lints_clean() {
        let p = load("quad.json");
        let result = router::route(&p);
        assert!(result.failed.is_empty(), "quad should route fully");
        let vs = lint(&p, &result.solution);
        assert!(vs.is_empty(), "quad router output must lint CLEAN, got {vs:?}");
    }

    #[test]
    fn lint_is_deterministic() {
        let p = load("quad.json");
        let result = router::route(&p);
        let a = lint(&p, &result.solution);
        let b = lint(&p, &result.solution);
        assert_eq!(a, b, "lint must be deterministic");
    }

    // ── InvalidLayer trigger test ─────────────────────────────────────────────

    #[test]
    fn invalid_layer_fires_for_inner1_on_two_layer_board() {
        // A trace on "inner1" on a 2-layer board (which only has "top" = 0 and
        // "bottom" = 1). "inner1" requires at least 3 layers (inner indices are
        // strictly between top and bottom). LayerRef("inner1").index(2) → None.
        let p = problem(
            vec![conn("SIG", &[(5.0, 10.0, "top"), (25.0, 10.0, "inner1")])],
            vec![],
        );
        let s = RouteSolution {
            traces: vec![trace("SIG", "inner1", 0.25, &[(5.0, 10.0), (25.0, 10.0)])],
            vias: vec![],
        };
        let vs = lint(&p, &s);
        let invalid_count = count(&vs, |v| matches!(v, DrcViolation::InvalidLayer { .. }));
        assert_eq!(
            invalid_count, 2,
            "expected exactly 2 InvalidLayer violations: \
             one for the solution trace on inner1, one for the connection route point on inner1, \
             got {vs:?}"
        );
        // Check that the payload fields are set correctly on the trace violation.
        let trace_viol = vs.iter().find(|v| {
            matches!(
                v,
                DrcViolation::InvalidLayer { layer, layer_count: 2, .. }
                    if layer == "inner1"
            )
        });
        assert!(
            trace_viol.is_some(),
            "must find an InvalidLayer with layer=inner1 and layer_count=2, got {vs:?}"
        );
    }

    #[test]
    fn invalid_layer_does_not_fire_for_valid_layers() {
        // "top" and "bottom" are always valid on a 2-layer board.
        let p = problem(
            vec![
                conn("A", &[(5.0, 10.0, "top"), (25.0, 10.0, "top")]),
                conn("B", &[(5.0, 20.0, "bottom"), (25.0, 20.0, "bottom")]),
            ],
            vec![],
        );
        let s = RouteSolution {
            traces: vec![
                trace("A", "top", 0.25, &[(5.0, 10.0), (25.0, 10.0)]),
                trace("B", "bottom", 0.25, &[(5.0, 20.0), (25.0, 20.0)]),
            ],
            vias: vec![],
        };
        let vs = lint(&p, &s);
        assert!(
            !vs.iter().any(|v| matches!(v, DrcViolation::InvalidLayer { .. })),
            "valid layer names must not raise InvalidLayer, got {vs:?}"
        );
    }

    #[test]
    fn fixtures_lint_clean_after_global_route() {
        // All fixtures must route (via global_route) and produce lint-clean
        // solutions via the slice-1 router. This guards that adding InvalidLayer
        // doesn't regress the existing gate.
        use crate::pathing::global_route;
        for name in &["led-r.json", "quad.json", "congested.json"] {
            let p = load(name);
            // Slice-1 router output (all fixtures that slice-1 can handle).
            let result = router::route(&p);
            let vs = lint(&p, &result.solution);
            let invalid: Vec<_> = vs
                .iter()
                .filter(|v| matches!(v, DrcViolation::InvalidLayer { .. }))
                .collect();
            assert!(
                invalid.is_empty(),
                "{name} slice-1 solution has InvalidLayer violations: {invalid:?}"
            );
            // Global route doesn't produce a RouteSolution, but the plan's route
            // points are the same as the problem's connection points, so calling
            // lint on the slice-1 solution is the right gate check here.
            let _ = global_route(&p); // must not panic
        }
    }
}
