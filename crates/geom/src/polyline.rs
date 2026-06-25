//! Ordered polyline with simplification + bbox queries.

use crate::consts::EPS;
use crate::point::Point2;
use crate::rect::Rect;
use crate::segment::Segment;

/// An ordered polyline (mm, y-down): a routed copper/wire path or a net's
/// terminal set. Owns simplification and bbox queries.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Polyline(Vec<Point2>);

impl Polyline {
    #[inline]
    pub fn new(points: Vec<Point2>) -> Self {
        Self(points)
    }

    #[inline]
    pub fn points(&self) -> &[Point2] {
        &self.0
    }

    #[inline]
    pub fn into_points(self) -> Vec<Point2> {
        self.0
    }

    /// Tight bounding box, or `None` if empty.
    #[inline]
    pub fn bbox(&self) -> Option<Rect> {
        Rect::bounding(&self.0)
    }

    /// Half-perimeter (w + h) of the bbox; 0 if empty. The HPWL/net-order term.
    #[inline]
    pub fn half_perimeter(&self) -> f64 {
        self.bbox().map_or(0.0, |r| r.half_perimeter())
    }

    /// Chain connected unordered segments into one path.
    pub fn from_unordered_segments(segments: impl IntoIterator<Item = Segment>) -> Option<Self> {
        let mut unused: Vec<Segment> = segments.into_iter().collect();
        let first = unused.first().copied()?;
        unused.remove(0);

        let mut chain = vec![first.a, first.b];
        while !unused.is_empty() {
            let tail = *chain.last()?;
            let Some((idx, next)) = unused.iter().enumerate().find_map(|(idx, segment)| {
                if tail.near_eq(segment.a, EPS) {
                    Some((idx, segment.b))
                } else if tail.near_eq(segment.b, EPS) {
                    Some((idx, segment.a))
                } else {
                    None
                }
            }) else {
                break;
            };
            unused.remove(idx);
            if next.near_eq(chain[0], EPS) {
                break;
            }
            chain.push(next);
        }

        unused.is_empty().then_some(Self(chain))
    }

    /// Dedup consecutive coincident points and merge forward collinear runs.
    pub fn simplify(self) -> Polyline {
        let mut deduped: Vec<Point2> = Vec::with_capacity(self.0.len());
        for p in self.0 {
            match deduped.last() {
                Some(last) if (last.x - p.x).abs() < EPS && (last.y - p.y).abs() < EPS => {}
                _ => deduped.push(p),
            }
        }
        let mut out: Vec<Point2> = Vec::with_capacity(deduped.len());
        for p in deduped {
            if out.len() >= 2 {
                let a = out[out.len() - 2];
                let b = out[out.len() - 1];
                let v1 = (b.x - a.x, b.y - a.y);
                let v2 = (p.x - b.x, p.y - b.y);
                let cross = v1.0 * v2.1 - v1.1 * v2.0;
                let dot = v1.0 * v2.0 + v1.1 * v2.1;
                // Collinear and not reversing → extend the current run.
                if cross.abs() < EPS && dot > 0.0 {
                    *out.last_mut().unwrap() = p;
                    continue;
                }
            }
            out.push(p);
        }
        Polyline(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simplify_merges_collinear_and_dedups() {
        let path = vec![
            Point2::new(0.0, 0.0),
            Point2::new(0.0, 0.0), // dup
            Point2::new(1.0, 0.0),
            Point2::new(2.0, 0.0), // collinear → dropped
            Point2::new(2.0, 1.0),
        ];
        let out = Polyline::new(path).simplify().into_points();
        assert_eq!(
            out,
            vec![
                Point2::new(0.0, 0.0),
                Point2::new(2.0, 0.0),
                Point2::new(2.0, 1.0)
            ]
        );
    }

    #[test]
    fn simplify_keeps_45_degree_collinear() {
        // A straight 45° run collapses to its endpoints.
        let path = vec![
            Point2::new(0.0, 0.0),
            Point2::new(1.0, 1.0),
            Point2::new(2.0, 2.0),
        ];
        let out = Polyline::new(path).simplify().into_points();
        assert_eq!(out, vec![Point2::new(0.0, 0.0), Point2::new(2.0, 2.0)]);
    }

    #[test]
    fn half_perimeter_of_bbox() {
        let pl = Polyline::new(vec![Point2::new(0.0, 0.0), Point2::new(3.0, 4.0)]);
        assert!((pl.half_perimeter() - 7.0).abs() < 1e-9);
        assert_eq!(Polyline::default().half_perimeter(), 0.0);
    }

    #[test]
    fn chains_unordered_segments_without_repeating_closure() {
        let chain = Polyline::from_unordered_segments([
            Segment::new(Point2::new(1.0, 0.0), Point2::new(1.0, 1.0)),
            Segment::new(Point2::new(0.0, 0.0), Point2::new(1.0, 0.0)),
            Segment::new(Point2::new(0.0, 1.0), Point2::new(1.0, 1.0)),
            Segment::new(Point2::new(0.0, 1.0), Point2::new(0.0, 0.0)),
        ])
        .unwrap();
        assert_eq!(
            chain.into_points(),
            vec![
                Point2::new(1.0, 0.0),
                Point2::new(1.0, 1.0),
                Point2::new(0.0, 1.0),
                Point2::new(0.0, 0.0),
            ]
        );
    }

    #[test]
    fn disconnected_segments_do_not_chain() {
        assert!(
            Polyline::from_unordered_segments([
                Segment::new(Point2::new(0.0, 0.0), Point2::new(1.0, 0.0)),
                Segment::new(Point2::new(3.0, 0.0), Point2::new(4.0, 0.0)),
            ])
            .is_none()
        );
    }
}
