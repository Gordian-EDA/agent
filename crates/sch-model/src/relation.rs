//! `place::relation` — measuring and REPAIRING the author's relational layout intent
//! ([`crate::ir::Relation`]).
//!
//! Two surfaces, mirroring how `grid` is handled: a violation COUNT an engine folds into
//! its objective ([`relation_viol`], [`relation_group_spread`]), and a deterministic
//! PROJECTION onto the feasible set ([`repair_relations`]) that seeds a search inside the
//! constraint polytope so a hard move-rejection rule can keep it there.
//!
//! Every comparison is on part origins in the same convention as `grid_order_viol`:
//! `x` grows right, `y` grows down, so "above" is the smaller `y`. A refdes with several
//! items (a multi-unit part) is represented by their centroid.

use std::collections::{BTreeMap, BTreeSet};

use geom::{EPS, Point2, Rect};

use crate::ir::{Axis, LayoutIr, Relation, Side};
use crate::item::Item;

use crate::geometry::item_rect;

/// Tolerance for "on the same line" in an [`Relation::Align`], and the clearance the
/// order repair leaves between two parts it separates.
const TOL: f64 = geom::GRID_50_MIL.pitch();

/// Which coordinate index an axis constrains: 0 for `x`, 1 for `y`.
fn axis_index(axis: Axis) -> usize {
    match axis {
        // Members share a horizontal line ⇒ their `y` is what must agree.
        Axis::Horizontal => 1,
        Axis::Vertical => 0,
    }
}

/// `(coordinate index, sign)` a side imposes: the member must sit at
/// `sign * coord < sign * anchor_coord`.
fn side_axis(side: Side) -> (usize, f64) {
    match side {
        Side::Left => (0, 1.0),
        Side::Right => (0, -1.0),
        Side::Top => (1, 1.0),
        Side::Bottom => (1, -1.0),
    }
}

/// Refdes → the centroid of every item placing it.
fn centroids(items: &[Item]) -> BTreeMap<&str, Point2> {
    let mut sum: BTreeMap<&str, (f64, f64, f64)> = BTreeMap::new();
    for it in items {
        let e = sum.entry(it.refdes.as_str()).or_insert((0.0, 0.0, 0.0));
        e.0 += it.at[0];
        e.1 += it.at[1];
        e.2 += 1.0;
    }
    sum.into_iter()
        .map(|(r, (x, y, n))| (r, Point2::new(x / n, y / n)))
        .collect()
}

/// Half the extent of `refdes` along `axis_ix`, from its widest placed item.
fn half_extent(items: &[Item], refdes: &str, axis_ix: usize) -> f64 {
    items
        .iter()
        .filter(|it| it.refdes == refdes)
        .map(|it| {
            let r = item_rect(it, it.at);
            if axis_ix == 0 {
                r.width() / 2.0
            } else {
                r.height() / 2.0
            }
        })
        .fold(0.0, f64::max)
}

/// Bounding box of every item placing one of `members`.
fn members_bbox(items: &[Item], members: &BTreeSet<&str>) -> Option<Rect> {
    let mut corners = Vec::new();
    for it in items
        .iter()
        .filter(|it| members.contains(it.refdes.as_str()))
    {
        let r = item_rect(it, it.at);
        corners.push(Point2::new(r.min_x, r.min_y));
        corners.push(Point2::new(r.max_x, r.max_y));
    }
    Rect::bounding(&corners)
}

/// The lower median of a coordinate sample — the line an [`Relation::Align`] snaps to.
fn median(mut vals: Vec<f64>) -> Option<f64> {
    if vals.is_empty() {
        return None;
    }
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap());
    Some(vals[(vals.len() - 1) / 2])
}

/// Count of unsatisfied relational statements — the engine-facing correctness term,
/// weighted like `grid_order` in every objective.
///
/// - An ordering relation contributes 1 when the pair is not strictly ordered.
/// - A [`Relation::Group`] contributes 1 per FOREIGN part sitting inside the group's
///   bounding box (the cluster is not cohesive) plus 1 per member on the wrong side of
///   its anchor.
/// - A [`Relation::Align`] contributes 1 per member off the shared line by more than one
///   grid step.
///
/// A relation naming a refdes that is not placed is skipped, not counted.
pub fn relation_viol(items: &[Item], ir: &LayoutIr) -> usize {
    if ir.relations.is_empty() {
        return 0;
    }
    let pos = centroids(items);
    let mut viol = 0usize;
    for rel in &ir.relations {
        match rel {
            Relation::LeftOf { a, b }
            | Relation::RightOf { a, b }
            | Relation::Above { a, b }
            | Relation::Below { a, b } => {
                let (Some(pa), Some(pb)) = (pos.get(a.as_str()), pos.get(b.as_str())) else {
                    continue;
                };
                let (ix, first, second) = match rel {
                    Relation::LeftOf { .. } => (0, pa, pb),
                    Relation::RightOf { .. } => (0, pb, pa),
                    Relation::Above { .. } => (1, pa, pb),
                    _ => (1, pb, pa),
                };
                if first[ix] >= second[ix] - EPS {
                    viol += 1;
                }
            }
            Relation::Group {
                members,
                side,
                anchor,
                ..
            } => {
                let set: BTreeSet<&str> = members.iter().map(String::as_str).collect();
                if let Some(bbox) = members_bbox(items, &set) {
                    viol += items
                        .iter()
                        .filter(|it| !set.contains(it.refdes.as_str()) && bbox.contains(it.at))
                        .count();
                }
                if let Some((side, Some(anchor))) =
                    crate::ir::group_placement(side.as_ref(), anchor.as_deref())
                    && let Some(pa) = pos.get(anchor)
                {
                    let (ix, sign) = side_axis(side);
                    viol += members
                        .iter()
                        .filter_map(|m| pos.get(m.as_str()))
                        .filter(|p| sign * p[ix] >= sign * pa[ix] - EPS)
                        .count();
                }
            }
            Relation::Align { members, axis } => {
                let ix = axis_index(*axis);
                let coords: Vec<f64> = members
                    .iter()
                    .filter_map(|m| pos.get(m.as_str()))
                    .map(|p| p[ix])
                    .collect();
                let Some(line) = median(coords.clone()) else {
                    continue;
                };
                viol += coords.iter().filter(|c| (*c - line).abs() > TOL).count();
            }
        }
    }
    viol
}

/// Total bounding-box half-perimeter of every [`Relation::Group`] — the SOFT cohesion
/// pull that makes a group tighten rather than merely stay un-intruded.
pub fn relation_group_spread(items: &[Item], ir: &LayoutIr) -> f64 {
    ir.relations
        .iter()
        .filter_map(|rel| match rel {
            Relation::Group { members, .. } => {
                let set: BTreeSet<&str> = members.iter().map(String::as_str).collect();
                members_bbox(items, &set).map(|r| r.half_perimeter())
            }
            _ => None,
        })
        .sum()
}

/// Translate every non-frozen item of `refdes` by `d` along `axis_ix`, snapped to grid.
fn shift(items: &mut [Item], refdes: &str, axis_ix: usize, d: f64) {
    for it in items
        .iter_mut()
        .filter(|it| it.refdes == refdes && !it.frozen)
    {
        let mut at: [f64; 2] = it.at.into();
        at[axis_ix] = geom::GRID_50_MIL.snap(at[axis_ix] + d);
        it.at = at.into();
    }
}

/// Whether a refdes has any movable item.
fn is_movable(items: &[Item], refdes: &str) -> bool {
    items.iter().any(|it| it.refdes == refdes && !it.frozen)
}

/// Push each group onto the requested side of its anchor, moving the group RIGIDLY (one
/// shared delta) so satisfying the side never scatters the members.
fn repair_group_sides(items: &mut [Item], ir: &LayoutIr) {
    for rel in &ir.relations {
        let Relation::Group {
            members,
            side,
            anchor,
            ..
        } = rel
        else {
            continue;
        };
        let Some((side, Some(anchor))) =
            crate::ir::group_placement(side.as_ref(), anchor.as_deref())
        else {
            continue;
        };
        let pos = centroids(items);
        let Some(pa) = pos.get(anchor).copied() else {
            continue;
        };
        let (ix, sign) = side_axis(side);
        let anchor_half = half_extent(items, anchor, ix);
        let mut delta: f64 = 0.0;
        for m in members {
            let Some(pm) = pos.get(m.as_str()) else {
                continue;
            };
            let want = pa[ix] - sign * (anchor_half + half_extent(items, m, ix) + TOL);
            let need = want - pm[ix];
            // `sign > 0` means the member must DECREASE along the axis, so the binding
            // requirement is the most negative delta (and the mirror image for `sign < 0`).
            if sign * need < sign * delta {
                delta = need;
            }
        }
        if delta.abs() <= EPS {
            continue;
        }
        for m in members {
            shift(items, m, ix, delta);
        }
    }
}

/// Longest-path projection of the ordering relations on one axis: walk the constraint DAG
/// in topological order and pull each part just far enough along the axis to clear its
/// predecessors. A frozen part holds its coordinate (it may leave the relation
/// unsatisfiable, which the violation count then reports); a CYCLE means contradictory
/// intent, so the axis is left untouched rather than resolved arbitrarily.
fn repair_axis(items: &mut [Item], ir: &LayoutIr, axis_ix: usize) {
    let mut preds: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    let mut nodes: BTreeSet<&str> = BTreeSet::new();
    for rel in &ir.relations {
        let (first, second) = match (rel, axis_ix) {
            (Relation::LeftOf { a, b }, 0) | (Relation::Above { a, b }, 1) => (a, b),
            (Relation::RightOf { a, b }, 0) | (Relation::Below { a, b }, 1) => (b, a),
            _ => continue,
        };
        nodes.insert(first);
        nodes.insert(second);
        preds.entry(second).or_default().insert(first);
    }
    let pos = centroids(items);
    nodes.retain(|n| pos.contains_key(n));
    if nodes.is_empty() {
        return;
    }

    let mut order: Vec<&str> = Vec::new();
    let mut placed: BTreeSet<&str> = BTreeSet::new();
    while order.len() < nodes.len() {
        let ready: Vec<&str> = nodes
            .iter()
            .copied()
            .filter(|n| !placed.contains(n))
            .filter(|n| {
                preds
                    .get(n)
                    .is_none_or(|p| p.iter().all(|q| placed.contains(q) || !nodes.contains(q)))
            })
            .collect();
        if ready.is_empty() {
            return; // contradictory intent
        }
        for n in ready {
            placed.insert(n);
            order.push(n);
        }
    }

    let mut succs: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for (n, ps) in &preds {
        for p in ps {
            succs.entry(p).or_default().insert(n);
        }
    }
    let mut coord: BTreeMap<&str, f64> = nodes.iter().map(|n| (*n, pos[n][axis_ix])).collect();
    let movable: BTreeSet<&str> = nodes
        .iter()
        .copied()
        .filter(|n| is_movable(items, n))
        .collect();

    // Relax both ways: the forward sweep pushes a part clear of its predecessors, the
    // backward sweep pulls it clear of its successors. One direction alone cannot satisfy a
    // relation whose other side is frozen. Alternating converges (each sweep is monotone in
    // its own direction) and is capped so a contradiction can never spin.
    const SWEEPS: usize = 8;
    for _ in 0..SWEEPS {
        let mut moved = false;
        for n in order.iter().copied().filter(|n| movable.contains(n)) {
            let mut want = coord[n];
            for p in preds
                .get(n)
                .into_iter()
                .flatten()
                .filter(|p| nodes.contains(*p))
            {
                want = want.max(coord[p] + separation(items, p, n, axis_ix));
            }
            moved |= set_coord(&mut coord, n, want);
        }
        for n in order.iter().rev().copied().filter(|n| movable.contains(n)) {
            let mut want = coord[n];
            for q in succs
                .get(n)
                .into_iter()
                .flatten()
                .filter(|q| nodes.contains(*q))
            {
                want = want.min(coord[q] - separation(items, n, q, axis_ix));
            }
            moved |= set_coord(&mut coord, n, want);
        }
        if !moved {
            break;
        }
    }
    let deltas: Vec<(String, f64)> = movable
        .iter()
        .map(|n| ((*n).to_owned(), coord[n] - pos[n][axis_ix]))
        .filter(|(_, d)| d.abs() > EPS)
        .collect();
    for (n, d) in deltas {
        shift(items, &n, axis_ix, d);
    }
}

/// Clearance between two parts ordered along `axis_ix`: their half-extents plus a step.
fn separation(items: &[Item], a: &str, b: &str, axis_ix: usize) -> f64 {
    half_extent(items, a, axis_ix) + half_extent(items, b, axis_ix) + TOL
}

/// Write `want` if it differs; reports whether it moved.
fn set_coord(coord: &mut BTreeMap<&str, f64>, n: &str, want: f64) -> bool {
    let moved = (coord[n] - want).abs() > EPS;
    if moved {
        *coord.get_mut(n).unwrap() = want;
    }
    moved
}

/// Snap each aligned member onto the shared line (the members' median coordinate).
fn repair_aligns(items: &mut [Item], ir: &LayoutIr) {
    for rel in &ir.relations {
        let Relation::Align { members, axis } = rel else {
            continue;
        };
        let ix = axis_index(*axis);
        let pos = centroids(items);
        let coords: Vec<f64> = members
            .iter()
            .filter_map(|m| pos.get(m.as_str()))
            .map(|p| p[ix])
            .collect();
        let Some(line) = median(coords) else {
            continue;
        };
        let deltas: Vec<(String, f64)> = members
            .iter()
            .filter_map(|m| pos.get(m.as_str()).map(|p| (m.clone(), line - p[ix])))
            .collect();
        for (m, d) in deltas {
            shift(items, &m, ix, d);
        }
    }
}

/// Project `items` onto the relational constraints: sides first (a rigid group move),
/// then the per-axis ordering DAG, then the alignment lines. Only non-frozen items move.
/// Returns whether anything moved, so a caller can skip a re-legalisation pass.
///
/// This is a PROJECTION, not a search: it makes a placement feasible so a search can start
/// inside the constraint set. It does not resolve overlaps it may create — run the shared
/// `decongest` afterwards, then re-measure.
pub fn repair_relations(items: &mut [Item], ir: &LayoutIr) -> bool {
    if ir.relations.is_empty() {
        return false;
    }
    let before: Vec<Point2> = items.iter().map(|it| it.at).collect();
    repair_group_sides(items, ir);
    repair_axis(items, ir, 0);
    repair_axis(items, ir, 1);
    repair_aligns(items, ir);
    items
        .iter()
        .zip(before)
        .any(|(it, b)| (it.at[0] - b[0]).abs() > EPS || (it.at[1] - b[1]).abs() > EPS)
}
