//! The shared context every [`Rule`](crate::Rule) reads.
//!
//! [`DrcCtx`] bundles the routed problem/solution with the one-pass copper
//! collection ([`collect_copper`]) the geometry rules iterate. Building it once
//! per [`DrcSuite::run`](crate::DrcSuite::run) means every rule — in-house or
//! third-party — measures the same copper, and a custom rule can reuse the same
//! collection instead of re-deriving it.
//!
//! Items borrow their owner names and layer lists from the problem/solution, so
//! collecting the copper allocates nothing per item beyond the vec itself —
//! cleanup passes re-run the whole suite hundreds of times per board.

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
    /// segments, then vias).
    pub copper: Vec<CopperItem<'a>>,
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

    /// The index pairs `(i, j)`, `i < j`, whose copper could be closer than
    /// `reach` — every pair the `i < j` double loop would have visited, minus
    /// those the bounding boxes already rule out, in the same ascending order.
    ///
    /// Sound because a shape sits inside its bounding box, so the box gap never
    /// exceeds the copper gap.
    pub fn pairs_within(&self, reach: f64) -> Vec<(usize, usize)> {
        let boxes: Vec<geom::Rect> = self
            .copper
            .iter()
            .map(|item| item.bounds().inflate(reach / 2.0))
            .collect();
        geom::candidate_pairs(&boxes)
    }
}

/// One piece of copper geometry to clearance-check, tagged with its owners.
///
/// Public so third-party rules can reuse the same collection.
pub struct CopperItem<'a> {
    /// Connection names that own this copper. Empty = unowned/keepout copper.
    pub owners: &'a [String],
    /// The piece's geometry.
    pub geom: CopperGeom<'a>,
}

/// The geometry of a [`CopperItem`]. Trace copper is a fattened segment;
/// obstacles are hard rectangles; vias are through discs spanning every layer.
pub enum CopperGeom<'a> {
    /// A trace segment fattened by `half_w` on a single layer.
    Segment {
        segment: geom::Segment,
        half_w: f64,
        layer: &'a LayerRef,
    },
    /// An axis-aligned obstacle rectangle on a set of layers (half-width 0).
    Rect {
        rect: geom::Rect,
        layers: &'a [LayerRef],
    },
    /// A through via: a disc of `radius` present on every layer.
    Via { at: geom::Point2, radius: f64 },
}

impl CopperItem<'_> {
    /// Does this item own copper for connection `name`?
    pub fn owned_by(&self, name: &str) -> bool {
        self.owners.iter().any(|o| o == name)
    }

    /// The item's first owner, or `""` for unowned copper. The representative
    /// connection name the geometry rules report.
    pub fn first_owner(&self) -> &str {
        self.owners.first().map(String::as_str).unwrap_or_default()
    }

    /// The copper's bounding box, fattened to its true extent (trace half-width,
    /// via radius).
    pub fn bounds(&self) -> geom::Rect {
        match &self.geom {
            CopperGeom::Segment {
                segment, half_w, ..
            } => geom::Rect::from_points(segment.a, segment.b).inflate(*half_w),
            CopperGeom::Rect { rect, .. } => *rect,
            CopperGeom::Via { at, radius } => geom::Rect::from_center_half(*at, (*radius, *radius)),
        }
    }
}

/// Collect every piece of copper (obstacles, trace segments, vias) into one
/// flat vec, in a stable order: obstacles, then trace segments, then vias.
///
/// Public so rules and third parties reuse the same collection instead of
/// re-deriving it from the raw `problem`/`solution`.
pub fn collect_copper<'a>(
    problem: &'a RoutingView,
    solution: &'a RouteSolution,
) -> Vec<CopperItem<'a>> {
    let mut items = Vec::new();

    // Obstacles: pads (owned) and unowned/foreign copper alike — both constrain
    // foreign nets. A pad never conflicts with its own connection (handled at
    // check time via owners).
    for ob in &problem.obstacles {
        items.push(CopperItem {
            owners: &ob.connected_to,
            geom: CopperGeom::Rect {
                rect: geom::Rect::from_center_half(ob.center, (ob.width / 2.0, ob.height / 2.0)),
                layers: &ob.layers,
            },
        });
    }

    // Trace segments: each consecutive point pair. A single-point (degenerate)
    // trace becomes a zero-length segment (a fat point) so it is still checked.
    for trace in &solution.traces {
        let half_w = trace.width / 2.0;
        let mut push_seg = |a: geom::Point2, b: geom::Point2| {
            items.push(CopperItem {
                owners: std::slice::from_ref(&trace.connection),
                geom: CopperGeom::Segment {
                    segment: geom::Segment::new(a, b),
                    half_w,
                    layer: &trace.layer,
                },
            });
        };
        if trace.path.len() == 1 {
            let p = trace.path[0];
            push_seg(p, p);
        }
        for w in trace.path.windows(2) {
            push_seg(w[0], w[1]);
        }
    }

    // Vias: through discs.
    for via in &solution.vias {
        items.push(CopperItem {
            owners: std::slice::from_ref(&via.connection),
            geom: CopperGeom::Via {
                at: via.at,
                radius: via.diameter / 2.0,
            },
        });
    }

    items
}
