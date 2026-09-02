//! `PairClearanceRule` — copper-edge clearance between every unordered pair of
//! copper items.
//!
//! Pairs come from [`DrcCtx::pairs_within`], so only copper whose bounding boxes
//! fall inside the clearance reach is tested — in the same order the exhaustive
//! `i < j` loop visited it. Three conflict categories: trace↔trace (same layer, foreign nets),
//! trace↔obstacle (shared layer, foreign owner), and via↔anything (through-hole,
//! so any layer). Same-owner pairs and obstacle↔obstacle pairs never conflict.

use crate::ctx::{CopperGeom, CopperItem};
use crate::rules::geom::{EPS, share_owner};
use crate::{DrcCtx, Rule};
use pcb_model::Finding;
use pcb_model::LayerRef;

/// Flags any foreign copper pair whose edges are closer than `problem.clearance`.
pub struct PairClearanceRule;

impl Rule for PairClearanceRule {
    fn name(&self) -> &'static str {
        "pair-clearance"
    }

    fn check(&self, ctx: &DrcCtx) -> Vec<Finding> {
        let clearance = ctx.problem.clearance;
        findings_over(&ctx.copper, clearance, &ctx.pairs_within(clearance))
    }
}

/// Every clearance finding over the given index pairs, in pair order.
fn findings_over(items: &[CopperItem], clearance: f64, pairs: &[(usize, usize)]) -> Vec<Finding> {
    pairs
        .iter()
        .filter_map(|&(i, j)| pair_clearance(&items[i], &items[j], clearance))
        .collect()
}

/// Clearance test for one unordered item pair. Returns a finding if their copper
/// edges are closer than `clearance` (and they are foreign to each other / on a
/// shared layer). `None` otherwise.
fn pair_clearance(x: &CopperItem, y: &CopperItem, clearance: f64) -> Option<Finding> {
    use CopperGeom::*;

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
                segment: s1,
                half_w: w1,
                layer: l1,
            },
            Segment {
                segment: s2,
                half_w: w2,
                layer: l2,
            },
        ) => {
            if l1 != l2 || share_owner(x, y) {
                return None;
            }
            let gap = s1.dist_to_segment(*s2) - w1 - w2;
            if gap + EPS < clearance {
                Some(Finding::ClearanceTraceTrace {
                    a: x.first_owner().to_string(),
                    b: y.first_owner().to_string(),
                    layer: l1.0.clone(),
                    gap,
                    required: clearance,
                    at: s1.a,
                })
            } else {
                None
            }
        }

        // Trace ↔ obstacle.
        (
            Segment {
                segment,
                half_w,
                layer,
            },
            Rect { rect, layers },
        ) => trace_obstacle(x, y, *segment, *half_w, layer, rect, layers, clearance),
        (
            Rect { rect, layers },
            Segment {
                segment,
                half_w,
                layer,
            },
        ) => trace_obstacle(y, x, *segment, *half_w, layer, rect, layers, clearance),

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
    seg: &CopperItem,
    rect: &CopperItem,
    segment: geom::Segment,
    half_w: f64,
    layer: &LayerRef,
    bounds: &geom::Rect,
    layers: &[LayerRef],
    clearance: f64,
) -> Option<Finding> {
    // The obstacle constrains the trace only on a shared layer, and only if the
    // obstacle is not owned by the trace's own connection.
    let conn = seg.first_owner();
    if !layers.contains(layer) || rect.owned_by(conn) {
        return None;
    }
    let gap = segment.dist_to_rect(bounds) - half_w;
    if gap + EPS < clearance {
        Some(Finding::ClearanceTraceObstacle {
            connection: conn.to_string(),
            obstacle_owners: rect.owners.to_vec(),
            layer: layer.0.clone(),
            gap,
            required: clearance,
            at: bounds.center(),
        })
    } else {
        None
    }
}

/// A via (`via`, at `at` with `radius`) against any other item `other`. Vias are
/// through-hole, so the layer is irrelevant; only ownership matters. Returns a
/// [`Finding::ClearanceViaAny`] when too close to foreign copper.
fn via_pair(
    via: &CopperItem,
    at: geom::Point2,
    radius: f64,
    other: &CopperItem,
    clearance: f64,
) -> Option<Finding> {
    if share_owner(via, other) {
        return None;
    }
    let conn = via.first_owner();
    let (edge_dist, other_owners) = match &other.geom {
        CopperGeom::Segment {
            segment, half_w, ..
        } => (segment.dist_to_point(at) - half_w, other.owners.to_vec()),
        CopperGeom::Rect { rect, .. } => (rect.dist_to_point(at), other.owners.to_vec()),
        CopperGeom::Via { at: p, radius: r2 } => (at.dist(*p) - r2, other.owners.to_vec()),
    };
    let gap = edge_dist - radius;
    if gap + EPS < clearance {
        Some(Finding::ClearanceViaAny {
            connection: conn.to_string(),
            other_owners,
            gap,
            required: clearance,
            at,
        })
    } else {
        None
    }
}

#[cfg(test)]
mod broad_phase_equivalence {
    use super::*;
    use crate::goldens;
    use pcb_model::Drc;

    /// The bounding-box filter must not change the report: over real boards and
    /// tilings large enough to take the hash-grid path, the filtered findings
    /// equal the exhaustive `i < j` loop's, in the same order.
    #[test]
    fn filtered_and_exhaustive_agree() {
        for (name, view, solution) in goldens::boards() {
            for n in [1, 4] {
                let (view, solution) = goldens::tiled(&view, &solution, n);
                let ctx = DrcCtx::build(&view, &solution);
                let clearance = view.clearance;
                let exhaustive: Vec<_> = (0..ctx.copper.len())
                    .flat_map(|i| ((i + 1)..ctx.copper.len()).map(move |j| (i, j)))
                    .collect();
                assert_eq!(
                    PairClearanceRule.check(&ctx),
                    findings_over(&ctx.copper, clearance, &exhaustive),
                    "{name} tiled {n}x{n}"
                );
            }
        }
    }

    /// Tiling multiplies the copper without changing any tile's verdict, so a
    /// clean board stays clean at scale — the property the filter must preserve.
    #[test]
    fn tiling_a_clean_board_stays_clean() {
        for (name, view, solution) in goldens::boards() {
            if !crate::StandardDrc.check(&view, &solution).is_empty() {
                continue;
            }
            let (view, solution) = goldens::tiled(&view, &solution, 4);
            assert_eq!(crate::StandardDrc.check(&view, &solution), vec![], "{name}");
        }
    }
}
