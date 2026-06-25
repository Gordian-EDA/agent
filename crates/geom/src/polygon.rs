//! Closed polygon geometry.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{Point2, Rect, Segment};

/// A closed polygon with at least three vertices.
#[derive(Debug, Clone, PartialEq)]
pub struct Polygon(Vec<Point2>);

impl Polygon {
    pub fn new(points: Vec<Point2>) -> Result<Self, String> {
        if points.len() < 3 {
            return Err("polygon requires at least three points".to_owned());
        }
        Ok(Self(points))
    }

    #[inline]
    pub fn points(&self) -> &[Point2] {
        &self.0
    }

    #[inline]
    pub fn into_points(self) -> Vec<Point2> {
        self.0
    }

    pub fn bbox(&self) -> Rect {
        Rect::bounding(&self.0).expect("Polygon::new requires at least three points")
    }

    pub fn edges(&self) -> impl Iterator<Item = Segment> + '_ {
        let n = self.0.len();
        self.0
            .iter()
            .copied()
            .enumerate()
            .map(move |(i, a)| Segment::new(a, self.0[(i + 1) % n]))
    }

    /// Is `pt` inside or on this polygon?
    pub fn contains_point(&self, pt: Point2) -> bool {
        if self.edges().any(|edge| edge.contains_point(pt)) {
            return true;
        }
        let mut inside = false;
        let mut j = self.0.len() - 1;
        for i in 0..self.0.len() {
            let (pi, pj) = (self.0[i], self.0[j]);
            if (pi.y > pt.y) != (pj.y > pt.y) {
                let x_int = pi.x + (pt.y - pi.y) / (pj.y - pi.y) * (pj.x - pi.x);
                if pt.x < x_int {
                    inside = !inside;
                }
            }
            j = i;
        }
        inside
    }

    pub fn dist_to_edge(&self, pt: Point2) -> f64 {
        self.edges()
            .map(|edge| edge.dist_to_point(pt))
            .fold(f64::INFINITY, f64::min)
    }

    pub fn segment_dist_to_edge(&self, seg: Segment) -> f64 {
        self.edges()
            .map(|edge| seg.dist_to_segment(edge))
            .fold(f64::INFINITY, f64::min)
    }
}

impl TryFrom<Vec<Point2>> for Polygon {
    type Error = String;

    fn try_from(points: Vec<Point2>) -> Result<Self, Self::Error> {
        Self::new(points)
    }
}

impl Serialize for Polygon {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Polygon {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Vec::<Point2>::deserialize(deserializer)
            .and_then(|points| Self::new(points).map_err(serde::de::Error::custom))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square() -> Polygon {
        Polygon::new(vec![
            Point2::new(0.0, 0.0),
            Point2::new(10.0, 0.0),
            Point2::new(10.0, 10.0),
            Point2::new(0.0, 10.0),
        ])
        .unwrap()
    }

    #[test]
    fn contains_points_by_even_odd_rule() {
        let poly = square();
        assert!(poly.contains_point(Point2::new(5.0, 5.0)));
        assert!(poly.contains_point(Point2::new(10.0, 5.0)));
        assert!(!poly.contains_point(Point2::new(15.0, 5.0)));
    }

    #[test]
    fn rejects_short_polygons() {
        assert!(Polygon::new(Vec::new()).is_err());
        assert!(Polygon::new(vec![Point2::new(0.0, 0.0), Point2::new(1.0, 0.0)]).is_err());
    }

    #[test]
    fn point_distance_to_edges() {
        let poly = square();
        assert!((poly.dist_to_edge(Point2::new(5.0, 7.0)) - 3.0).abs() < 1e-9);
        assert!((poly.dist_to_edge(Point2::new(12.0, 5.0)) - 2.0).abs() < 1e-9);
    }

    #[test]
    fn segment_distance_to_edges() {
        let poly = square();
        let near = Segment::new(Point2::new(12.0, 2.0), Point2::new(12.0, 8.0));
        assert!((poly.segment_dist_to_edge(near) - 2.0).abs() < 1e-9);
        let crossing = Segment::new(Point2::new(-1.0, 5.0), Point2::new(11.0, 5.0));
        assert_eq!(poly.segment_dist_to_edge(crossing), 0.0);
    }

    #[test]
    fn serde_preserves_point_array_shape() {
        let poly = square();
        let json = serde_json::to_string(&poly).unwrap();
        assert!(json.starts_with("["));
        assert_eq!(serde_json::from_str::<Polygon>(&json).unwrap(), poly);
        assert!(serde_json::from_str::<Polygon>("[]").is_err());
    }
}
