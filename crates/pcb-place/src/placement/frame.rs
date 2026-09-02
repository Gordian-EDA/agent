//! Whole-board framing: where the finished cluster sits inside the outline, and
//! which parts belong on its edges.
//!
//! Both passes run on [`super::route::place_tuned`]'s OUTPUT, so they hold
//! whichever path produced it — the structured fan-out fast path and the
//! force+anneal path alike. Edge seating in particular is a property of the
//! placer, not of one branch inside it.

use super::cost::{CostTerms, place_cost};
use super::geometry::{
    PLACEMENT_GRID, courtyard_margin, part_placement_bounds_envelope, placement_envelope_at,
    rotated_copper_bbox, rotated_courtyard_half,
};
use super::legalize::is_legal;
use super::route::{edge_seek_position_candidates, unique_position_candidates};
use crate::{LogicalNet, PlaceResult, PlacementHints, PlacementView, derive_nets};
use pcb_model::{Point2, Rect};

/// Everything the framing passes recompute from a finished [`PlaceResult`].
struct Frame {
    rotations: Vec<f64>,
    half: Vec<(f64, f64)>,
    copper_bbox: Vec<Rect>,
    pos: Vec<Point2>,
    margin: f64,
    nets: Vec<LogicalNet>,
    terms: CostTerms,
}

impl Frame {
    fn new(problem: &PlacementView, hints: &PlacementHints, result: &PlaceResult) -> Self {
        let rotations: Vec<f64> = result.placements.iter().map(|p| p.rotation).collect();
        Self {
            half: problem
                .parts
                .iter()
                .zip(&rotations)
                .map(|(p, &r)| rotated_courtyard_half(p, r))
                .collect(),
            copper_bbox: problem
                .parts
                .iter()
                .zip(&rotations)
                .map(|(p, &r)| rotated_copper_bbox(p, r))
                .collect(),
            pos: result.placements.iter().map(|p| p.at).collect(),
            margin: courtyard_margin(problem.clearance),
            nets: derive_nets(problem),
            terms: CostTerms::new(problem, hints, crate::decoupling_pairs(problem)),
            rotations,
        }
    }

    fn legal(&self, problem: &PlacementView) -> bool {
        is_legal(
            problem,
            &self.half,
            &self.copper_bbox,
            self.margin,
            &self.pos,
        )
    }

    fn cost(&self, problem: &PlacementView) -> f64 {
        place_cost(
            problem,
            &self.nets,
            &self.half,
            self.margin,
            &self.rotations,
            &self.terms,
            &self.pos,
        )
    }

    fn write_back(self, result: &mut PlaceResult) {
        for (placement, at) in result.placements.iter_mut().zip(self.pos) {
            placement.at = at;
        }
    }
}

/// Centre the whole placement in the board bounds by RIGID TRANSLATION.
///
/// A rigid translation preserves every relative distance, so courtyard overlap,
/// silk gaps, wirelength and ratline crossings are all invariant: only bounds
/// containment and the absolute terms can change. That makes it the one
/// whole-board move a finished placement can still afford, and it is what fixes
/// "the cluster is stranded in a corner". Keep-outs and a custom outline ARE
/// absolute, so the result is re-verified by [`is_legal`]; when the full shift
/// does not hold, the placement goes as far towards the centre as does.
///
/// [`seat_edge_seek_parts`] runs after this and puts the edge seekers back on
/// their edges, so translating them along here costs nothing.
///
/// Skipped when a locked part pins the frame (the local-edit case, where the
/// frame is exactly what must not move) or when a group prescribes an absolute
/// `region` (translation is not cost-invariant against a fixed rectangle).
pub(crate) fn center_placement(
    problem: &PlacementView,
    hints: &PlacementHints,
    result: &mut PlaceResult,
) {
    if !result.legal
        || problem.parts.iter().any(|p| p.locked.is_some())
        || hints.groups.iter().any(|g| g.region.is_some())
    {
        return;
    }
    let mut frame = Frame::new(problem, hints, result);
    let Some(placed) = union_envelope(problem, &frame) else {
        return;
    };
    let b = &problem.bounds;
    let shift = |lo: f64, hi: f64, span_lo: f64, span_hi: f64| {
        let (min_shift, max_shift) = (lo - span_lo, hi - span_hi);
        if min_shift > max_shift {
            return 0.0;
        }
        let center = (lo + hi) / 2.0 - (span_lo + span_hi) / 2.0;
        PLACEMENT_GRID.snap(center).clamp(min_shift, max_shift)
    };
    let dx = shift(b.min_x, b.max_x, placed.min_x, placed.max_x);
    let dy = shift(b.min_y, b.max_y, placed.min_y, placed.max_y);
    let seated = frame.pos.clone();
    // A keep-out or a concave outline can block the full shift; then go as far
    // towards the centre as stays legal rather than abandoning the move entirely.
    for step in grid_fractions(dx.abs().max(dy.abs())) {
        for (at, &from) in frame.pos.iter_mut().zip(&seated) {
            *at = Point2 {
                x: PLACEMENT_GRID.snap(from.x + dx * step),
                y: PLACEMENT_GRID.snap(from.y + dy * step),
            };
        }
        if frame.legal(problem) {
            frame.write_back(result);
            return;
        }
    }
}

/// Descending fractions of a shift, stopping once the remaining step is smaller
/// than the placement grid can express.
fn grid_fractions(magnitude: f64) -> impl Iterator<Item = f64> {
    std::iter::successors(Some(1.0_f64), |step| Some(step / 2.0))
        .take_while(move |step| step * magnitude >= PLACEMENT_GRID.pitch())
}

/// The bounding box of every part's placement envelope — what actually has to
/// stay inside the board.
fn union_envelope(problem: &PlacementView, frame: &Frame) -> Option<Rect> {
    (0..problem.parts.len())
        .map(|i| {
            let envelope = part_placement_bounds_envelope(
                &problem.parts[i],
                frame.half[i],
                frame.copper_bbox[i],
            );
            placement_envelope_at(frame.pos[i], envelope)
        })
        .reduce(|a, b| {
            Rect::new(
                a.min_x.min(b.min_x),
                a.min_y.min(b.min_y),
                a.max_x.max(b.max_x),
                a.max_y.max(b.max_y),
            )
        })
}

/// Seat every edge-seeking part on a board edge, whatever produced the placement.
///
/// The candidates are the same edge seats the position polish proposes; the
/// difference is that this runs LAST, so it also repairs the edge affinity a
/// preceding whole-board translation gave up.
///
/// `edge_seek` is an INTENT, not a preference, so the choice here is only WHICH
/// legal edge seat — the cheapest one — and never whether to take one at all;
/// this is the same contract [`super::route::seat_corner_seek_parts`] already
/// holds for mounting holes. Leaving it to the cost is what strands a connector
/// mid-board: once the cluster is centred, the wirelength of walking back out to
/// the edge outweighs the edge term, and the intent silently loses.
pub(crate) fn seat_edge_seek_parts(
    problem: &PlacementView,
    hints: &PlacementHints,
    result: &mut PlaceResult,
) {
    if !result.legal {
        return;
    }
    let mut frame = Frame::new(problem, hints, result);
    let mut seekers: Vec<usize> = frame
        .terms
        .edge_seek
        .iter()
        .chain(frame.terms.edge_of.iter().map(|(part, _)| part))
        .copied()
        .filter(|&i| problem.parts[i].locked.is_none())
        .collect();
    seekers.sort_unstable();
    seekers.dedup();
    if seekers.is_empty() {
        return;
    }

    for i in seekers {
        let old = frame.pos[i];
        let candidates = edge_seek_position_candidates(
            problem,
            &frame.half,
            frame.rotations[i],
            &frame.pos,
            i,
            &frame.terms,
        );
        let mut best = (frame.cost(problem), old);
        let mut seated = false;
        for candidate in unique_position_candidates(
            problem,
            i,
            frame.rotations[i],
            frame.half[i],
            frame.copper_bbox[i],
            old,
            candidates,
        ) {
            frame.pos[i] = candidate;
            if !frame.legal(problem) {
                continue;
            }
            let next = frame.cost(problem);
            if !seated || next + 1e-9 < best.0 {
                best = (next, candidate);
                seated = true;
            }
        }
        frame.pos[i] = best.1;
    }
    frame.write_back(result);
}
