//! The discrete pass that lands same-kind parts on a shared row or column.
//!
//! [`super::cost::place_cost`] carries a continuous alignment term, but exact
//! alignment is a measure-zero set: no amount of annealing or hill-climbing over
//! a 0.5 mm grid reliably lands *on* it. This pass proposes the exact answer —
//! collapse a cluster of nearly-equal coordinates onto one — and lets the
//! objective accept or reject it. Because alignment is part of that objective,
//! "accept iff the cost drops" needs no slack constant of its own.

use super::cost::{CostTerms, place_cost};
use super::geometry::PLACEMENT_GRID;
use super::legalize::is_legal;
use crate::{LogicalNet, PlacementView};
use pcb_model::{Point2, Rect};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Axis {
    X,
    Y,
}

impl Axis {
    fn of(self, p: Point2) -> f64 {
        match self {
            Axis::X => p.x,
            Axis::Y => p.y,
        }
    }

    fn set(self, p: &mut Point2, v: f64) {
        match self {
            Axis::X => p.x = v,
            Axis::Y => p.y = v,
        }
    }

    /// Courtyard extent across the axis being collapsed. Two same-kind parts
    /// closer than one body width on this axis are already trying to share the
    /// line; further apart they are deliberately in different columns/rows.
    fn tolerance(self, half: (f64, f64), margin: f64) -> f64 {
        2.0 * match self {
            Axis::X => half.0,
            Axis::Y => half.1,
        } + margin
    }
}

/// Collapse each cluster of near-equal same-kind coordinates onto its median,
/// keeping only the collapses that are legal and lower the placement cost.
#[allow(clippy::too_many_arguments)]
pub(crate) fn snap_same_kind_axes(
    problem: &PlacementView,
    nets: &[LogicalNet],
    margin: f64,
    terms: &CostTerms,
    rotations: &[f64],
    half: &[(f64, f64)],
    copper_bbox: &[Rect],
    pos: &mut [Point2],
) {
    let mut cost = place_cost(problem, nets, half, margin, rotations, terms, pos);
    for axis in [Axis::X, Axis::Y] {
        for group in &terms.kinds {
            let movable: Vec<usize> = group
                .iter()
                .copied()
                .filter(|&i| problem.parts[i].locked.is_none())
                .collect();
            for cluster in single_linkage_clusters(axis, &movable, half, margin, pos) {
                try_collapse(
                    problem,
                    nets,
                    margin,
                    terms,
                    rotations,
                    half,
                    copper_bbox,
                    pos,
                    axis,
                    &cluster,
                    &mut cost,
                );
            }
        }
    }
}

/// Sort by coordinate and cut wherever the gap to the next part exceeds the
/// larger of the two parts' tolerances.
fn single_linkage_clusters(
    axis: Axis,
    members: &[usize],
    half: &[(f64, f64)],
    margin: f64,
    pos: &[Point2],
) -> Vec<Vec<usize>> {
    let mut sorted = members.to_vec();
    sorted.sort_by(|&a, &b| axis.of(pos[a]).total_cmp(&axis.of(pos[b])).then(a.cmp(&b)));
    let mut clusters: Vec<Vec<usize>> = Vec::new();
    for i in sorted {
        let joins = clusters.last().is_some_and(|cluster| {
            let prev = *cluster.last().expect("clusters are never empty");
            axis.of(pos[i]) - axis.of(pos[prev])
                <= axis
                    .tolerance(half[i], margin)
                    .max(axis.tolerance(half[prev], margin))
        });
        match (joins, clusters.last_mut()) {
            (true, Some(cluster)) => cluster.push(i),
            _ => clusters.push(vec![i]),
        }
    }
    clusters.retain(|cluster| cluster.len() > 1);
    clusters
}

#[allow(clippy::too_many_arguments)]
fn try_collapse(
    problem: &PlacementView,
    nets: &[LogicalNet],
    margin: f64,
    terms: &CostTerms,
    rotations: &[f64],
    half: &[(f64, f64)],
    copper_bbox: &[Rect],
    pos: &mut [Point2],
    axis: Axis,
    cluster: &[usize],
    cost: &mut f64,
) {
    // `cluster` is already sorted along the axis, so the upper median is a plain index.
    let target = PLACEMENT_GRID.snap(axis.of(pos[cluster[cluster.len() / 2]]));
    let saved: Vec<Point2> = cluster.iter().map(|&i| pos[i]).collect();
    for &i in cluster {
        axis.set(&mut pos[i], target);
    }
    let next = place_cost(problem, nets, half, margin, rotations, terms, pos);
    if next + 1e-9 < *cost && is_legal(problem, half, copper_bbox, margin, pos) {
        *cost = next;
        return;
    }
    for (&i, &old) in cluster.iter().zip(&saved) {
        pos[i] = old;
    }
}
