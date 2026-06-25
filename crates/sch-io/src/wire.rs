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

use geom::{EPS, Point2, Polyline, Rect, Segment};
use sch_place::geom::Dir;

/// Minimum lead length out of a pin before the first turn, mm.
const LEAD_MM: f64 = 2.54;

/// 2–4 point Manhattan elbow from `a` (leaving along `dir_a` for at least
/// [`LEAD_MM`]) to `b`: straight when the lead axis lines up, else one L or
/// one Z. Every segment is axis-aligned.
pub fn elbow(a: Point2, dir_a: Dir, b: Point2) -> Vec<Point2> {
    let v = dir_a.vec();
    let lead = Point2::new(a.x + v.x * LEAD_MM, a.y + v.y * LEAD_MM);
    let path = match dir_a {
        Dir::East | Dir::West => {
            // Horizontal lead. Prefer extending the lead all the way to b.x
            // when b is "ahead" of the lead, then one vertical to b.
            let ahead = match dir_a {
                Dir::East => b.x >= lead.x,
                _ => b.x <= lead.x,
            };
            if ahead {
                vec![a, Point2::new(b.x, a.y), b]
            } else {
                // b behind the lead: out to the lead, vertical to b.y, back to b.
                vec![a, lead, Point2::new(lead.x, b.y), b]
            }
        }
        Dir::North | Dir::South => {
            let ahead = match dir_a {
                Dir::North => b.y <= lead.y,
                _ => b.y >= lead.y,
            };
            if ahead {
                vec![a, Point2::new(a.x, b.y), b]
            } else {
                vec![a, lead, Point2::new(b.x, lead.y), b]
            }
        }
    };
    Polyline::new(path).simplify().into_points()
}

/// Routing obstacles, all coordinates sheet mm.
pub struct RouteScene {
    /// Solid rects (symbol bodies): a path segment may not pass through one
    /// (edge-touching is tolerated).
    pub solids: Vec<Rect>,
    /// Net-anchor points with their net: a segment may not pass through a
    /// point of a DIFFERENT net (touching it would merge the nets).
    pub points: Vec<(Point2, String)>,
    /// Existing segments with their net (cluster wires, stubs, prior routes).
    /// Any touch with a different net's segment — shared point, collinear
    /// overlap, endpoint-on-segment — is forbidden; a strictly-interior
    /// perpendicular crossing is fine (KiCAD draws no connection there).
    pub segments: Vec<(Point2, Point2, String)>,
    /// Port-label (global-tag pennant) boxes with their OWN net. A FOREIGN net's
    /// wire may not pass through one — that draws a wire straight across someone
    /// else's edge tag (the inverting-input net bisecting a VIN pennant). The
    /// label's own net wire DOES reach it (the pennant connects there), so the
    /// box is net-tagged rather than a solid.
    pub label_solids: Vec<(Rect, String)>,
}

/// Whether an axis-aligned segment passes through a solid rect (open
/// intervals: running flush along a rect edge is tolerated).
fn seg_hits_rect(a: Point2, b: Point2, r: &Rect) -> bool {
    let (lo_x, hi_x) = (a.x.min(b.x), a.x.max(b.x));
    let (lo_y, hi_y) = (a.y.min(b.y), a.y.max(b.y));
    lo_x < r.max_x - EPS && r.min_x + EPS < hi_x && lo_y < r.max_y - EPS && r.min_y + EPS < hi_y
}

/// How two axis-aligned segments interact for routing purposes. Public to the
/// crate so the refinement scorer can reuse it to detect net merges (two
/// different-net segments that touch in a connecting way).
pub fn segments_conflict(a1: Point2, a2: Point2, b1: Point2, b2: Point2) -> bool {
    let a_horiz = (a1.y - a2.y).abs() < EPS;
    let b_horiz = (b1.y - b2.y).abs() < EPS;
    if a_horiz == b_horiz {
        // Parallel: conflict only when collinear AND the spans overlap
        // (closed — endpoint touch already merges).
        if a_horiz {
            (a1.y - b1.y).abs() < EPS && {
                let (alo, ahi) = (a1.x.min(a2.x), a1.x.max(a2.x));
                let (blo, bhi) = (b1.x.min(b2.x), b1.x.max(b2.x));
                alo <= bhi + EPS && blo <= ahi + EPS
            }
        } else {
            (a1.x - b1.x).abs() < EPS && {
                let (alo, ahi) = (a1.y.min(a2.y), a1.y.max(a2.y));
                let (blo, bhi) = (b1.y.min(b2.y), b1.y.max(b2.y));
                alo <= bhi + EPS && blo <= ahi + EPS
            }
        }
    } else {
        // Perpendicular: candidate crossing point.
        let (h1, h2, v1, v2) = if a_horiz {
            (a1, a2, b1, b2)
        } else {
            (b1, b2, a1, a2)
        };
        let p = Point2::new(v1.x, h1.y);
        let on_h = Segment::new(h1, h2).contains_point(p);
        let on_v = Segment::new(v1, v2).contains_point(p);
        if !(on_h && on_v) {
            return false;
        }
        // Touch at an ENDPOINT of either segment is a T/corner join -> merge.
        let is_end = |q: Point2, s1: Point2, s2: Point2| {
            ((q.x - s1.x).abs() < EPS && (q.y - s1.y).abs() < EPS)
                || ((q.x - s2.x).abs() < EPS && (q.y - s2.y).abs() < EPS)
        };
        is_end(p, h1, h2) || is_end(p, v1, v2)
    }
}

/// Whether `path` can be drawn for `net` without entering a body, touching a
/// foreign net's anchor point, or merging with a foreign net's segment.
pub fn path_ok(path: &[Point2], net: &str, scene: &RouteScene) -> bool {
    for w in path.windows(2) {
        let (a, b) = (w[0], w[1]);
        if scene.solids.iter().any(|r| seg_hits_rect(a, b, r)) {
            return false;
        }
        if scene
            .points
            .iter()
            .any(|(p, n)| n != net && Segment::new(a, b).contains_point(*p))
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
        if scene
            .label_solids
            .iter()
            .any(|(r, n)| n != net && seg_hits_rect(a, b, r))
        {
            return false;
        }
    }
    true
}

/// Number of VISUAL crossings `path` (drawn for `net`) would add against the
/// scene's already-committed foreign-net segments: a perpendicular pair (one
/// horizontal, one vertical) meeting at a point INTERIOR to both — the same
/// over-pass clutter `score::count_crossings` and the corpus oracle count. Used
/// by the router's wire-vs-label decision: a crossing-heavy hop is better named
/// (the human idiom) than drawn as a literal wire that reads as spaghetti.
pub fn path_crossings(path: &[Point2], net: &str, scene: &RouteScene) -> usize {
    let horiz = |a: &Point2, b: &Point2| (a.y - b.y).abs() < EPS;
    let vert = |a: &Point2, b: &Point2| (a.x - b.x).abs() < EPS;
    let interior = |v: f64, lo: f64, hi: f64| v > lo + EPS && v < hi - EPS;
    let mut n = 0;
    for w in path.windows(2) {
        let (a1, a2) = (w[0], w[1]);
        for (b1, b2, bn) in &scene.segments {
            if bn == net {
                continue; // same net: a deliberate join, not a crossing
            }
            let (h, v) = if horiz(&a1, &a2) && vert(b1, b2) {
                ((a1, a2), (*b1, *b2))
            } else if vert(&a1, &a2) && horiz(b1, b2) {
                ((*b1, *b2), (a1, a2))
            } else {
                continue; // parallel
            };
            let (hy, vx) = (h.0.y, v.0.x);
            let (hx_lo, hx_hi) = (h.0.x.min(h.1.x), h.0.x.max(h.1.x));
            let (vy_lo, vy_hi) = (v.0.y.min(v.1.y), v.0.y.max(v.1.y));
            if interior(vx, hx_lo, hx_hi) && interior(hy, vy_lo, vy_hi) {
                n += 1;
            }
        }
    }
    n
}

/// Clearance candidates keep this far off obstacle edges, mm.
const CLEAR_MM: f64 = 2.54;

/// Total Manhattan length of a path.
fn path_len(p: &[Point2]) -> f64 {
    p.windows(2).map(|w| w[0].manhattan(w[1])).sum()
}

/// Route one edge from `a` (a pin, leaving along `dir_a`) to `b` (any
/// terminal). Tries the plain elbow first; on collision, searches the
/// canonical 3/4-segment Manhattan families with detour coordinates derived
/// from obstacle edges (±[`CLEAR_MM`]), grid-snapped, picking the shortest
/// valid path (ties: fewer bends, then smaller coordinates — deterministic).
/// Returns None when nothing in the family fits — the caller falls back to
/// label connectivity.
pub fn route_edge(
    a: Point2,
    dir_a: Dir,
    b: Point2,
    net: &str,
    scene: &RouteScene,
) -> Option<Vec<Point2>> {
    let quick = elbow(a, dir_a, b);
    if path_ok(&quick, net, scene) {
        return Some(quick);
    }

    // Candidate detour coordinates: obstacle edges +- clearance (snapped AWAY
    // from the edge so snapping never re-enters the obstacle), the lead
    // coordinates, terminal coordinates, and the midline (snapped nearest).
    let snap_dn = |v: f64| (v / 1.27).floor() * 1.27;
    let snap_up = |v: f64| (v / 1.27).ceil() * 1.27;
    let snap_nr = |v: f64| (v / 1.27).round() * 1.27;
    let mut xs: Vec<f64> = Vec::new();
    let mut ys: Vec<f64> = Vec::new();
    for r in &scene.solids {
        xs.push(snap_dn(r.min_x - CLEAR_MM));
        xs.push(snap_up(r.max_x + CLEAR_MM));
        ys.push(snap_dn(r.min_y - CLEAR_MM));
        ys.push(snap_up(r.max_y + CLEAR_MM));
    }
    // Detour lanes around foreign port-label boxes too, so a wire skirts a
    // pennant instead of being rejected and falling back to a bare label.
    for (r, n) in &scene.label_solids {
        if n == net {
            continue;
        }
        xs.push(snap_dn(r.min_x - CLEAR_MM));
        xs.push(snap_up(r.max_x + CLEAR_MM));
        ys.push(snap_dn(r.min_y - CLEAR_MM));
        ys.push(snap_up(r.max_y + CLEAR_MM));
    }
    xs.push(snap_nr((a.x + b.x) / 2.0));
    ys.push(snap_nr((a.y + b.y) / 2.0));
    xs.push(a.x + LEAD_MM);
    xs.push(a.x - LEAD_MM);
    ys.push(a.y + LEAD_MM);
    ys.push(a.y - LEAD_MM);
    xs.push(b.x);
    ys.push(b.y);
    let dedup_sorted = |mut v: Vec<f64>| {
        v.sort_by(|p, q| p.partial_cmp(q).unwrap());
        v.dedup_by(|p, q| (*p - *q).abs() < EPS);
        v
    };
    let xs = dedup_sorted(xs);
    let ys = dedup_sorted(ys);

    let lead_ok = |x1: f64, y1: f64| match dir_a {
        Dir::East => x1 >= a.x + LEAD_MM - EPS,
        Dir::West => x1 <= a.x - LEAD_MM + EPS,
        Dir::North => y1 <= a.y - LEAD_MM + EPS,
        Dir::South => y1 >= a.y + LEAD_MM - EPS,
    };

    let mut best: Option<(f64, usize, Vec<Point2>)> = None;
    let consider = |raw: Vec<Point2>, best: &mut Option<(f64, usize, Vec<Point2>)>| {
        let p = Polyline::new(raw).simplify().into_points();
        if p.len() < 2 || !path_ok(&p, net, scene) {
            return;
        }
        let key = (path_len(&p), p.len());
        match best {
            Some((l, n, _)) if (*l, *n) <= key => {}
            _ => *best = Some((key.0, key.1, p)),
        }
    };

    match dir_a {
        Dir::East | Dir::West => {
            for &x1 in &xs {
                if !lead_ok(x1, 0.0) {
                    continue;
                }
                consider(
                    vec![a, Point2::new(x1, a.y), Point2::new(x1, b.y), b],
                    &mut best,
                );
                for &y1 in &ys {
                    consider(
                        vec![
                            a,
                            Point2::new(x1, a.y),
                            Point2::new(x1, y1),
                            Point2::new(b.x, y1),
                            b,
                        ],
                        &mut best,
                    );
                }
            }
        }
        Dir::North | Dir::South => {
            for &y1 in &ys {
                if !lead_ok(0.0, y1) {
                    continue;
                }
                consider(
                    vec![a, Point2::new(a.x, y1), Point2::new(b.x, y1), b],
                    &mut best,
                );
                for &x1 in &xs {
                    consider(
                        vec![
                            a,
                            Point2::new(a.x, y1),
                            Point2::new(x1, y1),
                            Point2::new(x1, b.y),
                            b,
                        ],
                        &mut best,
                    );
                }
            }
        }
    }
    best.map(|(_, _, p)| p)
}

/// Minimum-spanning-tree edges over terminals by Manhattan distance (Prim's,
/// deterministic: ties broken by smaller terminal index).
pub fn mst_edges(terminals: &[Point2]) -> Vec<(usize, usize)> {
    let n = terminals.len();
    if n < 2 {
        return Vec::new();
    }
    let dist = |i: usize, j: usize| terminals[i].manhattan(terminals[j]);
    let mut in_tree = vec![false; n];
    in_tree[0] = true;
    let mut edges = Vec::with_capacity(n - 1);
    for _ in 1..n {
        let mut best: Option<(f64, usize, usize)> = None;
        for i in 0..n {
            if !in_tree[i] {
                continue;
            }
            for j in 0..n {
                if in_tree[j] {
                    continue;
                }
                let d = dist(i, j);
                let key = (d, i, j);
                if best.is_none_or(|b| key < b) {
                    best = Some(key);
                }
            }
        }
        let (_, i, j) = best.unwrap();
        in_tree[j] = true;
        edges.push((i, j));
    }
    edges
}

/// Junction dots for one net's emitted paths: every point where >= 3 segment
/// ENDS meet (a T or X formed by deliberate same-net joins).
pub fn junction_points(paths: &[Vec<Point2>]) -> Vec<Point2> {
    let mut counts: std::collections::BTreeMap<(u64, u64), (Point2, usize)> =
        std::collections::BTreeMap::new();
    for path in paths {
        for w in path.windows(2) {
            for p in [w[0], w[1]] {
                let key = (p.x.to_bits(), p.y.to_bits());
                counts.entry(key).or_insert((p, 0)).1 += 1;
            }
        }
    }
    // Interior vertices of one polyline count twice (end of one segment,
    // start of the next) without being junctions; >= 3 distinct segment ends
    // at a point only happens where separate runs join.
    counts
        .into_values()
        .filter(|(_, c)| *c >= 3)
        .map(|(p, _)| p)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_axis_aligned(path: &[Point2]) {
        for w in path.windows(2) {
            assert!(
                (w[0].x - w[1].x).abs() < EPS || (w[0].y - w[1].y).abs() < EPS,
                "segment not axis-aligned: {w:?}"
            );
        }
    }

    #[test]
    fn straight_east() {
        let p = elbow(Point2::new(0.0, 0.0), Dir::East, Point2::new(10.0, 0.0));
        assert_eq!(p, vec![Point2::new(0.0, 0.0), Point2::new(10.0, 0.0)]);
    }

    #[test]
    fn l_shape_when_target_is_ahead_and_offset() {
        // b northeast of a, leaving East: horizontal then vertical.
        let p = elbow(Point2::new(0.0, 0.0), Dir::East, Point2::new(10.0, -5.0));
        assert_eq!(
            p,
            vec![
                Point2::new(0.0, 0.0),
                Point2::new(10.0, 0.0),
                Point2::new(10.0, -5.0)
            ]
        );
        assert_axis_aligned(&p);
    }

    #[test]
    fn z_shape_when_target_is_behind() {
        // b WEST of a but we must leave East: lead out, vertical, back.
        let p = elbow(Point2::new(0.0, 0.0), Dir::East, Point2::new(-10.0, -5.0));
        assert_eq!(
            p,
            vec![
                Point2::new(0.0, 0.0),
                Point2::new(2.54, 0.0),
                Point2::new(2.54, -5.0),
                Point2::new(-10.0, -5.0)
            ]
        );
        assert_axis_aligned(&p);
    }

    fn scene(
        solids: Vec<Rect>,
        points: Vec<(Point2, &str)>,
        segments: Vec<(Point2, Point2, &str)>,
    ) -> RouteScene {
        RouteScene {
            solids,
            points: points
                .into_iter()
                .map(|(p, n)| (p, n.to_string()))
                .collect(),
            segments: segments
                .into_iter()
                .map(|(a, b, n)| (a, b, n.to_string()))
                .collect(),
            label_solids: Vec::new(),
        }
    }

    #[test]
    fn mst_connects_collinear_terminals_without_redundancy() {
        let t = [
            Point2::new(0.0, 0.0),
            Point2::new(10.0, 0.0),
            Point2::new(20.0, 0.0),
        ];
        let edges = mst_edges(&t);
        assert_eq!(edges.len(), 2);
        // Adjacent pairs, never the redundant 0-2 long edge.
        assert!(edges.contains(&(0, 1)));
        assert!(edges.contains(&(1, 2)));
    }

    #[test]
    fn junctions_at_three_way_meets_only() {
        // A horizontal run plus a vertical drop ending mid-run: the meet point
        // collects 3 segment ends -> junction. The plain corner of an L does
        // not (2 ends).
        let paths = vec![
            vec![Point2::new(0.0, 0.0), Point2::new(10.0, 0.0)],
            vec![
                Point2::new(5.0, -5.0),
                Point2::new(5.0, 0.0),
                Point2::new(8.0, 0.0),
            ],
        ];
        // ...but if the drop TERMINATES on the run, the run is split at the
        // tap in real emission. Model that split:
        let split = vec![
            vec![Point2::new(0.0, 0.0), Point2::new(5.0, 0.0)],
            vec![Point2::new(5.0, 0.0), Point2::new(10.0, 0.0)],
            vec![Point2::new(5.0, -5.0), Point2::new(5.0, 0.0)],
        ];
        assert_eq!(junction_points(&paths), Vec::<Point2>::new());
        assert_eq!(junction_points(&split), vec![Point2::new(5.0, 0.0)]);
    }

    #[test]
    fn route_edge_clear_field_returns_elbow() {
        let s = scene(vec![], vec![], vec![]);
        let p = route_edge(
            Point2::new(0.0, 0.0),
            Dir::East,
            Point2::new(10.0, -5.0),
            "A",
            &s,
        )
        .unwrap();
        assert_eq!(
            p,
            elbow(Point2::new(0.0, 0.0), Dir::East, Point2::new(10.0, -5.0))
        );
    }

    #[test]
    fn route_edge_detours_around_a_rect() {
        // Block the straight east run with a body; the route must detour and
        // stay valid.
        let s = scene(vec![Rect::new(4.0, -2.0, 6.0, 2.0)], vec![], vec![]);
        let p = route_edge(
            Point2::new(0.0, 0.0),
            Dir::East,
            Point2::new(12.7, 0.0),
            "A",
            &s,
        )
        .unwrap();
        assert!(path_ok(&p, "A", &s));
        assert_eq!(p.first(), Some(&Point2::new(0.0, 0.0)));
        assert_eq!(p.last(), Some(&Point2::new(12.7, 0.0)));
        // Deterministic.
        assert_eq!(
            p,
            route_edge(
                Point2::new(0.0, 0.0),
                Dir::East,
                Point2::new(12.7, 0.0),
                "A",
                &s
            )
            .unwrap()
        );
    }

    #[test]
    fn foreign_wire_detours_around_port_label_but_owner_passes() {
        // A "VIN" port pennant box straddling a horizontal run at y=0.
        let mut s = scene(vec![], vec![], vec![]);
        s.label_solids = vec![(Rect::new(4.0, -3.0, 8.0, 3.0), "VIN".to_string())];
        // A straight run THROUGH the box: rejected for a foreign net, fine for the owner.
        let straight = vec![Point2::new(0.0, 0.0), Point2::new(12.0, 0.0)];
        assert!(!path_ok(&straight, "FB", &s));
        assert!(path_ok(&straight, "VIN", &s));
        // route_edge: a foreign net detours and stays valid; the owner gets the
        // straight elbow (its own pennant never blocks it).
        let foreign = route_edge(
            Point2::new(0.0, 0.0),
            Dir::East,
            Point2::new(12.0, 0.0),
            "FB",
            &s,
        )
        .unwrap();
        assert!(path_ok(&foreign, "FB", &s));
        assert_eq!(
            route_edge(
                Point2::new(0.0, 0.0),
                Dir::East,
                Point2::new(12.0, 0.0),
                "VIN",
                &s
            )
            .unwrap(),
            elbow(Point2::new(0.0, 0.0), Dir::East, Point2::new(12.0, 0.0))
        );
    }

    #[test]
    fn every_wire_detours_around_a_no_connect_keepout() {
        // A no-connect X glyph is owned by the sentinel net `\0no_connect`, which no
        // real wire ever carries — so UNLIKE a port pennant there is no owner that may
        // pass through; every net detours, keeping the X off all wires.
        const NC: &str = "\0no_connect";
        let mut s = scene(vec![], vec![], vec![]);
        s.label_solids = vec![(Rect::new(4.0, -1.27, 6.54, 1.27), NC.to_string())];
        let straight = vec![Point2::new(0.0, 0.0), Point2::new(12.0, 0.0)];
        // Every real net is foreign to the sentinel, so the straight run is rejected.
        assert!(!path_ok(&straight, "FB", &s));
        assert!(!path_ok(&straight, "GND", &s));
        // A foreign run detours and stays valid (the glyph never sits on the wire).
        let routed = route_edge(
            Point2::new(0.0, 0.0),
            Dir::East,
            Point2::new(12.0, 0.0),
            "FB",
            &s,
        )
        .unwrap();
        assert!(path_ok(&routed, "FB", &s));
        assert_ne!(
            routed,
            elbow(Point2::new(0.0, 0.0), Dir::East, Point2::new(12.0, 0.0))
        );
    }

    #[test]
    fn route_edge_walled_in_returns_none() {
        // b is enclosed by a ring of solids covering all detour candidates.
        let s = scene(
            vec![
                Rect::new(8.0, -20.0, 10.0, 20.0),   // wall east of a
                Rect::new(-20.0, -10.0, 20.0, -8.0), // wall north
                Rect::new(-20.0, 8.0, 20.0, 10.0),   // wall south
                Rect::new(-10.0, -20.0, -8.0, 20.0), // wall west
            ],
            vec![],
            vec![],
        );
        assert!(
            route_edge(
                Point2::new(0.0, 0.0),
                Dir::East,
                Point2::new(30.0, 0.0),
                "A",
                &s
            )
            .is_none()
        );
    }

    #[test]
    fn path_through_solid_is_rejected() {
        let s = scene(vec![Rect::new(4.0, -2.0, 6.0, 2.0)], vec![], vec![]);
        let p = vec![Point2::new(0.0, 0.0), Point2::new(10.0, 0.0)];
        assert!(!path_ok(&p, "A", &s));
        // Flush along the rect's top edge is tolerated.
        let p = vec![Point2::new(0.0, -2.0), Point2::new(10.0, -2.0)];
        assert!(path_ok(&p, "A", &s));
    }

    #[test]
    fn path_through_foreign_point_is_rejected() {
        let s = scene(
            vec![],
            vec![(Point2::new(5.0, 0.0), "B"), (Point2::new(7.0, 0.0), "A")],
            vec![],
        );
        let p = vec![Point2::new(0.0, 0.0), Point2::new(10.0, 0.0)];
        assert!(
            !path_ok(&p, "A", &s),
            "foreign point on the wire merges nets"
        );
        let s2 = scene(vec![], vec![(Point2::new(7.0, 0.0), "A")], vec![]);
        assert!(path_ok(&p, "A", &s2), "own-net point is a deliberate join");
    }

    #[test]
    fn collinear_foreign_overlap_rejected_perpendicular_crossing_ok() {
        // Collinear overlap with a foreign horizontal wire.
        let s = scene(
            vec![],
            vec![],
            vec![(Point2::new(3.0, 0.0), Point2::new(12.0, 0.0), "B")],
        );
        let p = vec![Point2::new(0.0, 0.0), Point2::new(10.0, 0.0)];
        assert!(!path_ok(&p, "A", &s));
        // Perpendicular foreign wire crossing strictly mid-to-mid: fine.
        let s = scene(
            vec![],
            vec![],
            vec![(Point2::new(5.0, -4.0), Point2::new(5.0, 4.0), "B")],
        );
        assert!(path_ok(&p, "A", &s));
        // Same crossing but the foreign wire ENDS on our path: merge.
        let s = scene(
            vec![],
            vec![],
            vec![(Point2::new(5.0, -4.0), Point2::new(5.0, 0.0), "B")],
        );
        assert!(!path_ok(&p, "A", &s));
        // Our segment ENDING on a foreign wire: merge.
        let p2 = vec![Point2::new(0.0, 0.0), Point2::new(5.0, 0.0)];
        let s = scene(
            vec![],
            vec![],
            vec![(Point2::new(5.0, -4.0), Point2::new(5.0, 4.0), "B")],
        );
        assert!(!path_ok(&p2, "A", &s));
        // Same-net touches are deliberate joins.
        let s = scene(
            vec![],
            vec![],
            vec![(Point2::new(5.0, -4.0), Point2::new(5.0, 0.0), "A")],
        );
        assert!(path_ok(&p, "A", &s));
    }

    #[test]
    fn path_crossings_counts_foreign_overpasses_only() {
        // One foreign vertical wire crossing our horizontal path strictly mid-to-mid.
        let s = scene(
            vec![],
            vec![],
            vec![(Point2::new(5.0, -4.0), Point2::new(5.0, 4.0), "B")],
        );
        let p = vec![Point2::new(0.0, 0.0), Point2::new(10.0, 0.0)];
        assert_eq!(path_crossings(&p, "A", &s), 1);
        // SAME-net crossing is a deliberate join, not clutter — not counted.
        assert_eq!(path_crossings(&p, "B", &s), 0);
        // A shared ENDPOINT (the foreign wire ends ON our path) is a T-join, not an
        // over-pass: the crossing point is not interior to the vertical wire.
        let s = scene(
            vec![],
            vec![],
            vec![(Point2::new(5.0, -4.0), Point2::new(5.0, 0.0), "B")],
        );
        assert_eq!(path_crossings(&p, "A", &s), 0);
        // Two foreign over-passes -> count 2.
        let s = scene(
            vec![],
            vec![],
            vec![
                (Point2::new(3.0, -4.0), Point2::new(3.0, 4.0), "B"),
                (Point2::new(7.0, -4.0), Point2::new(7.0, 4.0), "C"),
            ],
        );
        assert_eq!(path_crossings(&p, "A", &s), 2);
    }

    #[test]
    fn vertical_lead_straight_and_l() {
        let p = elbow(Point2::new(0.0, 10.0), Dir::North, Point2::new(0.0, 0.0));
        assert_eq!(p, vec![Point2::new(0.0, 10.0), Point2::new(0.0, 0.0)]);
        let p = elbow(Point2::new(0.0, 10.0), Dir::North, Point2::new(6.0, 2.0));
        assert_eq!(
            p,
            vec![
                Point2::new(0.0, 10.0),
                Point2::new(0.0, 2.0),
                Point2::new(6.0, 2.0)
            ]
        );
        assert_axis_aligned(&p);
    }
}
