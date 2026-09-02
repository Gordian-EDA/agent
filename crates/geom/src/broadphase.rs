//! Broad-phase pair filtering: which boxes are close enough to be worth an
//! exact test?
//!
//! Every pairwise geometry check in the stack (clearance, connectivity) shares
//! one structure: a pair can only matter when the two shapes' bounding boxes,
//! inflated by the check's reach, meet. [`candidate_pairs`] exploits that to
//! replace an `O(n²)` double loop with a uniform hash grid, returning the
//! surviving pairs in ascending `(i, j)` order — the order the double loop
//! visited them, so a caller's output ordering is unchanged.
//!
//! The filter is deliberately *conservative*: boxes that merely touch are kept
//! (unlike [`Rect::overlaps`], which is open), so a pair sitting exactly on a
//! clearance boundary always reaches the exact test.

use crate::{EPS, Rect};
use std::collections::HashMap;

/// Below this many boxes the grid costs more than the pairs it saves.
const DIRECT_PAIR_LIMIT: usize = 48;

/// A box spanning more than this many cells per axis is compared against every
/// other box instead of being scattered over the grid (a board-wide keepout).
const MAX_SPAN: i64 = 16;

/// Do two boxes meet? Closed and `EPS`-widened, so this never rejects a pair an
/// exact test might still accept.
#[inline]
pub fn boxes_meet(a: &Rect, b: &Rect) -> bool {
    a.min_x <= b.max_x + EPS
        && a.max_x >= b.min_x - EPS
        && a.min_y <= b.max_y + EPS
        && a.max_y >= b.min_y - EPS
}

/// Every index pair `(i, j)`, `i < j`, whose boxes meet, in ascending order.
///
/// Equivalent to filtering the full `i < j` double loop by [`boxes_meet`], but
/// near-linear in the number of surviving pairs for the spatially sparse box
/// sets a board's copper produces.
pub fn candidate_pairs(boxes: &[Rect]) -> Vec<(usize, usize)> {
    if boxes.len() <= DIRECT_PAIR_LIMIT {
        return direct_pairs(boxes);
    }
    let cell = cell_size(boxes);
    let mut buckets: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
    let mut oversized: Vec<usize> = Vec::new();

    for (i, b) in boxes.iter().enumerate() {
        let (lo, hi) = cell_span(b, cell);
        if hi.0 - lo.0 > MAX_SPAN || hi.1 - lo.1 > MAX_SPAN {
            oversized.push(i);
            continue;
        }
        for cx in lo.0..=hi.0 {
            for cy in lo.1..=hi.1 {
                buckets.entry((cx, cy)).or_default().push(i);
            }
        }
    }

    let mut pairs = Vec::new();
    for members in buckets.values() {
        for (a, &i) in members.iter().enumerate() {
            for &j in &members[a + 1..] {
                if boxes_meet(&boxes[i], &boxes[j]) {
                    pairs.push((i.min(j), i.max(j)));
                }
            }
        }
    }
    for &i in &oversized {
        for j in 0..boxes.len() {
            if i != j && boxes_meet(&boxes[i], &boxes[j]) {
                pairs.push((i.min(j), i.max(j)));
            }
        }
    }

    pairs.sort_unstable();
    pairs.dedup();
    pairs
}

fn direct_pairs(boxes: &[Rect]) -> Vec<(usize, usize)> {
    let mut pairs = Vec::new();
    for i in 0..boxes.len() {
        for j in (i + 1)..boxes.len() {
            if boxes_meet(&boxes[i], &boxes[j]) {
                pairs.push((i, j));
            }
        }
    }
    pairs
}

/// The mean box extent. At this cell size a typical box touches `O(1)` cells
/// and a cell holds `O(local density)` boxes, which is what makes the sweep
/// near-linear. A degenerate (all-point) input falls back to a unit cell.
fn cell_size(boxes: &[Rect]) -> f64 {
    let total: f64 = boxes.iter().map(|b| b.width().max(b.height())).sum();
    let mean = total / boxes.len() as f64;
    if mean > EPS { mean } else { 1.0 }
}

fn cell_span(b: &Rect, cell: f64) -> ((i64, i64), (i64, i64)) {
    let idx = |v: f64| (v / cell).floor() as i64;
    (
        (idx(b.min_x - EPS), idx(b.min_y - EPS)),
        (idx(b.max_x + EPS), idx(b.max_y + EPS)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lattice(n: i32, size: f64, step: f64) -> Vec<Rect> {
        let mut v = Vec::new();
        for i in 0..n {
            for j in 0..n {
                let (x, y) = (i as f64 * step, j as f64 * step);
                v.push(Rect::new(x, y, x + size, y + size));
            }
        }
        v
    }

    #[test]
    fn matches_brute_force_on_a_dense_lattice() {
        let boxes = lattice(12, 1.5, 1.0);
        assert!(boxes.len() > DIRECT_PAIR_LIMIT);
        assert_eq!(candidate_pairs(&boxes), direct_pairs(&boxes));
    }

    #[test]
    fn matches_brute_force_when_boxes_are_disjoint() {
        let boxes = lattice(10, 0.5, 4.0);
        assert_eq!(candidate_pairs(&boxes), direct_pairs(&boxes));
        assert!(candidate_pairs(&boxes).is_empty());
    }

    #[test]
    fn exactly_touching_boxes_survive_the_filter() {
        let mut boxes = lattice(10, 1.0, 4.0);
        boxes.push(Rect::new(1.0, 0.0, 2.0, 1.0));
        let pairs = candidate_pairs(&boxes);
        assert_eq!(pairs, direct_pairs(&boxes));
        assert!(pairs.contains(&(0, boxes.len() - 1)));
    }

    #[test]
    fn a_board_wide_box_still_pairs_with_everything() {
        let mut boxes = lattice(10, 0.5, 4.0);
        boxes.push(Rect::new(-10.0, -10.0, 100.0, 100.0));
        assert_eq!(candidate_pairs(&boxes), direct_pairs(&boxes));
        assert_eq!(candidate_pairs(&boxes).len(), boxes.len() - 1);
    }

    #[test]
    fn degenerate_point_boxes_do_not_divide_by_zero() {
        let boxes = vec![Rect::zero(); DIRECT_PAIR_LIMIT + 5];
        assert_eq!(candidate_pairs(&boxes), direct_pairs(&boxes));
    }

    #[test]
    fn small_inputs_take_the_direct_path_and_agree() {
        let boxes = lattice(4, 1.5, 1.0);
        assert_eq!(candidate_pairs(&boxes), direct_pairs(&boxes));
    }
}
