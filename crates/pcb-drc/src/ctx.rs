//! The shared context every [`Rule`](crate::Rule) reads.
//!
//! [`DrcCtx`] bundles the routed problem/solution with the one-pass copper
//! collection ([`collect_copper`]) the geometry rules iterate. Building it once
//! per [`DrcSuite::run`](crate::DrcSuite::run) means every rule — in-house or
//! third-party — measures the same copper, and a custom rule can reuse the same
//! collection instead of re-deriving it.

use pcb_model::{LayerRef, RouteSolution, RoutingView};

/// The context a [`Rule`](crate::Rule) reads: the routed board plus the shared
/// copper collection.
///
/// Holds borrows of the `problem` and `solution`, so it lives only for the
/// duration of a [`DrcSuite::run`](crate::DrcSuite::run).
pub struct DrcCtx<'a> {
    /// The routing problem (board bounds, obstacles, clearance, …).
    pub problem: &'a RoutingView,
    /// The routed solution under test (traces, vias).
    pub solution: &'a RouteSolution,
    /// Every piece of copper, flattened in a stable order (obstacles, then trace
    /// segments, then vias) — the single hand-off point a spatial index could
    /// later slot into.
    pub copper: Vec<CopperItem>,
}

impl<'a> DrcCtx<'a> {
    /// Build the context, running [`collect_copper`] once.
    pub fn build(problem: &'a RoutingView, solution: &'a RouteSolution) -> Self {
        Self {
            problem,
            solution,
            copper: collect_copper(problem, solution),
        }
    }
}

/// One piece of copper geometry to clearance-check, tagged with its owners.
///
/// A flat list of these (see [`DrcCtx::copper`]) is the single hand-off point
/// where a spatial index could later slot in. Public so third-party rules can
/// reuse the same collection.
pub struct CopperItem {
    /// Connection names that own this copper. Empty = unowned/keepout copper.
    pub owners: Vec<String>,
    /// The piece's geometry.
    pub geom: CopperGeom,
}

/// The geometry of a [`CopperItem`]. Trace copper is a fattened segment;
/// obstacles are hard rectangles; vias are through discs spanning every layer.
pub enum CopperGeom {
    /// A trace segment fattened by `half_w` on a single layer.
    Segment {
        segment: geom::Segment,
        half_w: f64,
        layer: LayerRef,
    },
    /// An axis-aligned obstacle rectangle on a set of layers (half-width 0).
    Rect {
        rect: geom::Rect,
        layers: Vec<LayerRef>,
    },
    /// A through via: a disc of `radius` present on every layer.
    Via { at: geom::Point2, radius: f64 },
}

impl CopperItem {
    /// Does this item own copper for connection `name`?
    pub fn owned_by(&self, name: &str) -> bool {
        self.owners.iter().any(|o| o == name)
    }

    /// The item's first owner, or the empty string for unowned copper. The
    /// representative connection name the geometry rules report.
    pub fn first_owner(&self) -> String {
        self.owners.first().cloned().unwrap_or_default()
    }
}

/// Collect every piece of copper (obstacles, trace segments, vias) into one
/// flat vec, in a stable order: obstacles, then trace segments, then vias.
///
/// Public so rules and third parties reuse the same collection instead of
/// re-deriving it from the raw `problem`/`solution`.
pub fn collect_copper(problem: &RoutingView, solution: &RouteSolution) -> Vec<CopperItem> {
    let mut items = Vec::new();

    // Obstacles: pads (owned) and unowned/foreign copper alike — both constrain
    // foreign nets. A pad never conflicts with its own connection (handled at
    // check time via owners).
    for ob in &problem.obstacles {
        let hw = ob.width / 2.0;
        let hh = ob.height / 2.0;
        items.push(CopperItem {
            owners: ob.connected_to.clone(),
            geom: CopperGeom::Rect {
                rect: geom::Rect::from_center_half(ob.center, (hw, hh)),
                layers: ob.layers.clone(),
            },
        });
    }

    // Trace segments: each consecutive point pair. A single-point (degenerate)
    // trace becomes a zero-length segment (a fat point) so it is still checked.
    for trace in &solution.traces {
        let half_w = trace.width / 2.0;
        let push_seg = |items: &mut Vec<CopperItem>, a: geom::Point2, b: geom::Point2| {
            items.push(CopperItem {
                owners: vec![trace.connection.clone()],
                geom: CopperGeom::Segment {
                    segment: geom::Segment::new(a, b),
                    half_w,
                    layer: trace.layer.clone(),
                },
            });
        };
        if trace.path.len() == 1 {
            let p = trace.path[0];
            push_seg(&mut items, p, p);
        }
        for w in trace.path.windows(2) {
            push_seg(&mut items, w[0], w[1]);
        }
    }

    // Vias: through discs.
    for via in &solution.vias {
        items.push(CopperItem {
            owners: vec![via.connection.clone()],
            geom: CopperGeom::Via {
                at: via.at,
                radius: via.diameter / 2.0,
            },
        });
    }

    items
}
