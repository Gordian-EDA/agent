//! The elbow [`SchRouter`] leaf: Manhattan wires between net terminals.
//!
//! Pure geometry — no I/O, no KiCAD environment. The approach (adapted from
//! tscircuit's schematic-trace-solver, per the aesthetics spec §3): start
//! with the simplest orientation-aware elbow between two terminals, then
//! repair collisions by shifting one interior segment at a time to candidate
//! offsets, best-first by [`geom::RouteShape`] — detour, bends and crossings —
//! until the path reads clean. Outputs stay "schematic-shaped" (2–4 segments)
//! by construction; a failed route falls back to label connectivity at the
//! call site — never an error.

use geom::Dir;
use geom::{EPS, Point2, Polyline, RouteShape};
use sch_model::route::{RouteScene, SchRouter, path_crossings, path_ok};

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

/// Clearance candidates keep this far off obstacle edges, mm.
const CLEAR_MM: f64 = 2.54;

/// Total Manhattan length of a path.
fn path_len(p: &[Point2]) -> f64 {
    p.windows(2).map(|w| w[0].manhattan(w[1])).sum()
}

/// Route one edge from `a` (a pin, leaving along `dir_a`) to `b` (any
/// terminal). The plain elbow wins outright when it is legal and clean; a
/// colliding OR CROSSING elbow sends the search into the canonical 3/4-segment
/// Manhattan families, with detour coordinates derived from obstacle edges
/// (±[`CLEAR_MM`]), grid-snapped, picking the path of least
/// [`RouteShape`] — a couple of millimetres of detour to miss a foreign wire
/// beats the shorter path that draws over it. Ties: shorter, then fewer bends,
/// then smaller coordinates (deterministic). Returns None when nothing in the
/// family fits — the caller falls back to label connectivity.
pub fn route_edge(
    a: Point2,
    dir_a: Dir,
    b: Point2,
    net: &str,
    scene: &RouteScene,
) -> Option<Vec<Point2>> {
    let quick = elbow(a, dir_a, b);
    if path_ok(&quick, net, scene) && path_crossings(&quick, net, scene) == 0 {
        return Some(quick);
    }

    // Candidate detour coordinates: obstacle edges +- clearance (snapped AWAY
    // from the edge so snapping never re-enters the obstacle), the lead
    // coordinates, terminal coordinates, and the midline (snapped nearest).
    let grid = geom::GRID_50_MIL;
    let mut xs: Vec<f64> = Vec::new();
    let mut ys: Vec<f64> = Vec::new();
    for r in &scene.solids {
        xs.push(grid.snap_down(r.min_x - CLEAR_MM));
        xs.push(grid.snap_up(r.max_x + CLEAR_MM));
        ys.push(grid.snap_down(r.min_y - CLEAR_MM));
        ys.push(grid.snap_up(r.max_y + CLEAR_MM));
    }
    // Detour lanes around foreign port-label boxes too, so a wire skirts a
    // pennant instead of being rejected and falling back to a bare label.
    for (r, n) in &scene.label_solids {
        if n == net {
            continue;
        }
        xs.push(grid.snap_down(r.min_x - CLEAR_MM));
        xs.push(grid.snap_up(r.max_x + CLEAR_MM));
        ys.push(grid.snap_down(r.min_y - CLEAR_MM));
        ys.push(grid.snap_up(r.max_y + CLEAR_MM));
    }
    xs.push(grid.snap((a.x + b.x) / 2.0));
    ys.push(grid.snap((a.y + b.y) / 2.0));
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

    type Ranked = (f64, f64, usize, Vec<Point2>);
    let mut best: Option<Ranked> = None;
    let consider = |raw: Vec<Point2>, best: &mut Option<Ranked>| {
        let p = Polyline::new(raw).simplify().into_points();
        if p.len() < 2 || !path_ok(&p, net, scene) {
            return;
        }
        let shape = RouteShape::of(&p, path_crossings(&p, net, scene));
        let key = (shape.cost(), path_len(&p), p.len());
        match best {
            Some((c, l, n, _)) if (*c, *l, *n) <= key => {}
            _ => *best = Some((key.0, key.1, key.2, p)),
        }
    };
    consider(quick, &mut best);

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
    best.map(|(_, _, _, p)| p)
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
        for (i, &i_in_tree) in in_tree.iter().enumerate() {
            if !i_in_tree {
                continue;
            }
            for (j, &j_in_tree) in in_tree.iter().enumerate() {
                if j_in_tree {
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

/// Orientation-aware elbows with best-first collision repair.
pub struct ElbowRouter;

impl SchRouter for ElbowRouter {
    fn name(&self) -> &'static str {
        "elbow"
    }

    fn tree_edges(&self, terminals: &[Point2]) -> Vec<(usize, usize)> {
        mst_edges(terminals)
    }

    fn route_edge(
        &self,
        a: Point2,
        dir_a: Dir,
        b: Point2,
        net: &str,
        scene: &RouteScene,
    ) -> Option<Vec<Point2>> {
        route_edge(a, dir_a, b, net, scene)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geom::Rect;
    use sch_model::route::{NetSegment, path_crossings};

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
        segments: Vec<NetSegment>,
    ) -> RouteScene {
        RouteScene {
            solids,
            points: points
                .into_iter()
                .map(|(p, n)| (p, n.to_string()))
                .collect(),
            segments,
            label_solids: Vec::new(),
        }
    }

    fn net_segment(a: Point2, b: Point2, net: &str) -> NetSegment {
        NetSegment::new(a, b, net)
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
            vec![net_segment(
                Point2::new(3.0, 0.0),
                Point2::new(12.0, 0.0),
                "B",
            )],
        );
        let p = vec![Point2::new(0.0, 0.0), Point2::new(10.0, 0.0)];
        assert!(!path_ok(&p, "A", &s));
        // Perpendicular foreign wire crossing strictly mid-to-mid: fine.
        let s = scene(
            vec![],
            vec![],
            vec![net_segment(
                Point2::new(5.0, -4.0),
                Point2::new(5.0, 4.0),
                "B",
            )],
        );
        assert!(path_ok(&p, "A", &s));
        // Same crossing but the foreign wire ENDS on our path: merge.
        let s = scene(
            vec![],
            vec![],
            vec![net_segment(
                Point2::new(5.0, -4.0),
                Point2::new(5.0, 0.0),
                "B",
            )],
        );
        assert!(!path_ok(&p, "A", &s));
        // Our segment ENDING on a foreign wire: merge.
        let p2 = vec![Point2::new(0.0, 0.0), Point2::new(5.0, 0.0)];
        let s = scene(
            vec![],
            vec![],
            vec![net_segment(
                Point2::new(5.0, -4.0),
                Point2::new(5.0, 4.0),
                "B",
            )],
        );
        assert!(!path_ok(&p2, "A", &s));
        // Same-net touches are deliberate joins.
        let s = scene(
            vec![],
            vec![],
            vec![net_segment(
                Point2::new(5.0, -4.0),
                Point2::new(5.0, 0.0),
                "A",
            )],
        );
        assert!(path_ok(&p, "A", &s));
    }

    #[test]
    fn path_crossings_counts_foreign_overpasses_only() {
        // One foreign vertical wire crossing our horizontal path strictly mid-to-mid.
        let s = scene(
            vec![],
            vec![],
            vec![net_segment(
                Point2::new(5.0, -4.0),
                Point2::new(5.0, 4.0),
                "B",
            )],
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
            vec![net_segment(
                Point2::new(5.0, -4.0),
                Point2::new(5.0, 0.0),
                "B",
            )],
        );
        assert_eq!(path_crossings(&p, "A", &s), 0);
        // Two foreign over-passes -> count 2.
        let s = scene(
            vec![],
            vec![],
            vec![
                net_segment(Point2::new(3.0, -4.0), Point2::new(3.0, 4.0), "B"),
                net_segment(Point2::new(7.0, -4.0), Point2::new(7.0, 4.0), "C"),
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
