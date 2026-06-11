//! Elbow router: Manhattan wires between net terminals.
//!
//! Pure geometry — no I/O, no KiCAD environment. The approach (adapted from
//! tscircuit's schematic-trace-solver, per the aesthetics spec §3): start
//! with the simplest orientation-aware elbow between two terminals, then
//! repair collisions by shifting one interior segment at a time to candidate
//! offsets, best-first by total path length, until the path is collision-free
//! or an expansion cap is hit. Outputs stay "schematic-shaped" (2–4 segments)
//! by construction; a failed route falls back to label connectivity at the
//! call site — never an error.

use crate::emit::Dir;

pub(crate) type Pt = [f64; 2];

/// A polyline path of axis-aligned segments (consecutive points).
pub(crate) type Path = Vec<Pt>;

/// Minimum lead length out of a pin before the first turn, mm.
const LEAD_MM: f64 = 2.54;

/// Drop zero-length segments and merge collinear runs.
fn simplify(mut path: Path) -> Path {
    path.dedup_by(|a, b| (a[0] - b[0]).abs() < 1e-9 && (a[1] - b[1]).abs() < 1e-9);
    let mut out: Path = Vec::with_capacity(path.len());
    for p in path {
        if out.len() >= 2 {
            let a = out[out.len() - 2];
            let b = out[out.len() - 1];
            let collinear_x = (a[0] - b[0]).abs() < 1e-9 && (b[0] - p[0]).abs() < 1e-9;
            let collinear_y = (a[1] - b[1]).abs() < 1e-9 && (b[1] - p[1]).abs() < 1e-9;
            if collinear_x || collinear_y {
                *out.last_mut().unwrap() = p;
                continue;
            }
        }
        out.push(p);
    }
    out
}

/// 2–4 point Manhattan elbow from `a` (leaving along `dir_a` for at least
/// [`LEAD_MM`]) to `b`: straight when the lead axis lines up, else one L or
/// one Z. Every segment is axis-aligned.
pub(crate) fn elbow(a: Pt, dir_a: Dir, b: Pt) -> Path {
    let v = dir_a.vec();
    let lead = [a[0] + v[0] * LEAD_MM, a[1] + v[1] * LEAD_MM];
    let path = match dir_a {
        Dir::East | Dir::West => {
            // Horizontal lead. Prefer extending the lead all the way to b.x
            // when b is "ahead" of the lead, then one vertical to b.
            let ahead = match dir_a {
                Dir::East => b[0] >= lead[0],
                _ => b[0] <= lead[0],
            };
            if ahead {
                vec![a, [b[0], a[1]], b]
            } else {
                // b behind the lead: out to the lead, vertical to b.y, back to b.
                vec![a, lead, [lead[0], b[1]], b]
            }
        }
        Dir::North | Dir::South => {
            let ahead = match dir_a {
                Dir::North => b[1] <= lead[1],
                _ => b[1] >= lead[1],
            };
            if ahead {
                vec![a, [a[0], b[1]], b]
            } else {
                vec![a, lead, [b[0], lead[1]], b]
            }
        }
    };
    simplify(path)
}

/// Routing obstacles, all coordinates sheet mm.
pub(crate) struct RouteScene {
    /// Solid rects `[min_x, min_y, max_x, max_y]` (symbol bodies): a path
    /// segment may not pass through one (edge-touching is tolerated).
    pub solids: Vec<[f64; 4]>,
    /// Net-anchor points with their net: a segment may not pass through a
    /// point of a DIFFERENT net (touching it would merge the nets).
    pub points: Vec<(Pt, String)>,
    /// Existing segments with their net (cluster wires, stubs, prior routes).
    /// Any touch with a different net's segment — shared point, collinear
    /// overlap, endpoint-on-segment — is forbidden; a strictly-interior
    /// perpendicular crossing is fine (KiCAD draws no connection there).
    pub segments: Vec<(Pt, Pt, String)>,
}

const EPS: f64 = 1e-6;

/// Whether an axis-aligned segment passes through a solid rect (open
/// intervals: running flush along a rect edge is tolerated).
fn seg_hits_rect(a: Pt, b: Pt, r: &[f64; 4]) -> bool {
    let (lo_x, hi_x) = (a[0].min(b[0]), a[0].max(b[0]));
    let (lo_y, hi_y) = (a[1].min(b[1]), a[1].max(b[1]));
    lo_x < r[2] - EPS && r[0] + EPS < hi_x && lo_y < r[3] - EPS && r[1] + EPS < hi_y
}

/// How two axis-aligned segments interact for routing purposes.
fn segments_conflict(a1: Pt, a2: Pt, b1: Pt, b2: Pt) -> bool {
    let a_horiz = (a1[1] - a2[1]).abs() < EPS;
    let b_horiz = (b1[1] - b2[1]).abs() < EPS;
    if a_horiz == b_horiz {
        // Parallel: conflict only when collinear AND the spans overlap
        // (closed — endpoint touch already merges).
        if a_horiz {
            (a1[1] - b1[1]).abs() < EPS && {
                let (alo, ahi) = (a1[0].min(a2[0]), a1[0].max(a2[0]));
                let (blo, bhi) = (b1[0].min(b2[0]), b1[0].max(b2[0]));
                alo <= bhi + EPS && blo <= ahi + EPS
            }
        } else {
            (a1[0] - b1[0]).abs() < EPS && {
                let (alo, ahi) = (a1[1].min(a2[1]), a1[1].max(a2[1]));
                let (blo, bhi) = (b1[1].min(b2[1]), b1[1].max(b2[1]));
                alo <= bhi + EPS && blo <= ahi + EPS
            }
        }
    } else {
        // Perpendicular: candidate crossing point.
        let (h1, h2, v1, v2) = if a_horiz { (a1, a2, b1, b2) } else { (b1, b2, a1, a2) };
        let p = [v1[0], h1[1]];
        let on_h = crate::emit::point_on_segment(p, h1, h2);
        let on_v = crate::emit::point_on_segment(p, v1, v2);
        if !(on_h && on_v) {
            return false;
        }
        // Touch at an ENDPOINT of either segment is a T/corner join -> merge.
        let is_end = |q: Pt, s1: Pt, s2: Pt| {
            ((q[0] - s1[0]).abs() < EPS && (q[1] - s1[1]).abs() < EPS)
                || ((q[0] - s2[0]).abs() < EPS && (q[1] - s2[1]).abs() < EPS)
        };
        is_end(p, h1, h2) || is_end(p, v1, v2)
    }
}

/// Whether `path` can be drawn for `net` without entering a body, touching a
/// foreign net's anchor point, or merging with a foreign net's segment.
pub(crate) fn path_ok(path: &Path, net: &str, scene: &RouteScene) -> bool {
    for w in path.windows(2) {
        let (a, b) = (w[0], w[1]);
        if scene.solids.iter().any(|r| seg_hits_rect(a, b, r)) {
            return false;
        }
        if scene
            .points
            .iter()
            .any(|(p, n)| n != net && crate::emit::point_on_segment(*p, a, b))
        {
            return false;
        }
        if scene
            .segments
            .iter()
            .any(|(s1, s2, n)| n != net && segments_conflict(a, b, *s1, *s2))
        {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_axis_aligned(path: &Path) {
        for w in path.windows(2) {
            assert!(
                (w[0][0] - w[1][0]).abs() < 1e-9 || (w[0][1] - w[1][1]).abs() < 1e-9,
                "segment not axis-aligned: {w:?}"
            );
        }
    }

    #[test]
    fn straight_east() {
        let p = elbow([0.0, 0.0], Dir::East, [10.0, 0.0]);
        assert_eq!(p, vec![[0.0, 0.0], [10.0, 0.0]]);
    }

    #[test]
    fn l_shape_when_target_is_ahead_and_offset() {
        // b northeast of a, leaving East: horizontal then vertical.
        let p = elbow([0.0, 0.0], Dir::East, [10.0, -5.0]);
        assert_eq!(p, vec![[0.0, 0.0], [10.0, 0.0], [10.0, -5.0]]);
        assert_axis_aligned(&p);
    }

    #[test]
    fn z_shape_when_target_is_behind() {
        // b WEST of a but we must leave East: lead out, vertical, back.
        let p = elbow([0.0, 0.0], Dir::East, [-10.0, -5.0]);
        assert_eq!(
            p,
            vec![[0.0, 0.0], [2.54, 0.0], [2.54, -5.0], [-10.0, -5.0]]
        );
        assert_axis_aligned(&p);
    }

    fn scene(
        solids: Vec<[f64; 4]>,
        points: Vec<(Pt, &str)>,
        segments: Vec<(Pt, Pt, &str)>,
    ) -> RouteScene {
        RouteScene {
            solids,
            points: points.into_iter().map(|(p, n)| (p, n.to_string())).collect(),
            segments: segments
                .into_iter()
                .map(|(a, b, n)| (a, b, n.to_string()))
                .collect(),
        }
    }

    #[test]
    fn path_through_solid_is_rejected() {
        let s = scene(vec![[4.0, -2.0, 6.0, 2.0]], vec![], vec![]);
        let p = vec![[0.0, 0.0], [10.0, 0.0]];
        assert!(!path_ok(&p, "A", &s));
        // Flush along the rect's top edge is tolerated.
        let p = vec![[0.0, -2.0], [10.0, -2.0]];
        assert!(path_ok(&p, "A", &s));
    }

    #[test]
    fn path_through_foreign_point_is_rejected() {
        let s = scene(vec![], vec![([5.0, 0.0], "B"), ([7.0, 0.0], "A")], vec![]);
        let p = vec![[0.0, 0.0], [10.0, 0.0]];
        assert!(!path_ok(&p, "A", &s), "foreign point on the wire merges nets");
        let s2 = scene(vec![], vec![([7.0, 0.0], "A")], vec![]);
        assert!(path_ok(&p, "A", &s2), "own-net point is a deliberate join");
    }

    #[test]
    fn collinear_foreign_overlap_rejected_perpendicular_crossing_ok() {
        // Collinear overlap with a foreign horizontal wire.
        let s = scene(vec![], vec![], vec![([3.0, 0.0], [12.0, 0.0], "B")]);
        let p = vec![[0.0, 0.0], [10.0, 0.0]];
        assert!(!path_ok(&p, "A", &s));
        // Perpendicular foreign wire crossing strictly mid-to-mid: fine.
        let s = scene(vec![], vec![], vec![([5.0, -4.0], [5.0, 4.0], "B")]);
        assert!(path_ok(&p, "A", &s));
        // Same crossing but the foreign wire ENDS on our path: merge.
        let s = scene(vec![], vec![], vec![([5.0, -4.0], [5.0, 0.0], "B")]);
        assert!(!path_ok(&p, "A", &s));
        // Our segment ENDING on a foreign wire: merge.
        let p2 = vec![[0.0, 0.0], [5.0, 0.0]];
        let s = scene(vec![], vec![], vec![([5.0, -4.0], [5.0, 4.0], "B")]);
        assert!(!path_ok(&p2, "A", &s));
        // Same-net touches are deliberate joins.
        let s = scene(vec![], vec![], vec![([5.0, -4.0], [5.0, 0.0], "A")]);
        assert!(path_ok(&p, "A", &s));
    }

    #[test]
    fn vertical_lead_straight_and_l() {
        let p = elbow([0.0, 10.0], Dir::North, [0.0, 0.0]);
        assert_eq!(p, vec![[0.0, 10.0], [0.0, 0.0]]);
        let p = elbow([0.0, 10.0], Dir::North, [6.0, 2.0]);
        assert_eq!(p, vec![[0.0, 10.0], [0.0, 2.0], [6.0, 2.0]]);
        assert_axis_aligned(&p);
    }
}
