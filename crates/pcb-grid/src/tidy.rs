//! Post-emission trace tidying, shared by both routing engines.
//!
//! An 8-way A* on a lattice does not draw a diagonal — it draws a *staircase*
//! that approximates one, alternating axial and diagonal cells. The staircase
//! has exactly the same copper length as the smooth run it approximates, so a
//! shortcut pass gated on wirelength can never remove a single one of its
//! corners. Corners are what the eye reads, so [`straighten_traces`] gates on
//! the corner count instead and accepts any replacement that does not lengthen
//! copper.
//!
//! Every replacement is checked against the design-rule oracle and kept only if
//! it introduces no new finding, so tidying can never trade correctness for
//! looks.

use geom::Point2;
use pcb_model::{Drc, Finding, RouteSolution, RoutingView, octilinear_path};

/// How many oracle calls one pass may spend before giving up on a solution.
const LINT_BUDGET: usize = 256;

/// Slop for "no longer than before", in mm.
const LENGTH_EPS: f64 = 1e-9;

/// Does `candidate` report any finding `baseline` did not? The acceptance test
/// every tidying pass shares: a pass may leave pre-existing findings alone, but
/// never add one.
pub fn introduces_new_findings(baseline: &[Finding], candidate: &[Finding]) -> bool {
    candidate
        .iter()
        .any(|finding| !baseline.iter().any(|known| known == finding))
}

/// Replace lattice staircases and detours with the two-segment octilinear form
/// a board editor would draw, wherever that costs no extra copper and no new
/// design-rule finding.
///
/// Greedy and monotone: each accepted replacement strictly reduces the trace's
/// vertex count, so the loop terminates.
pub fn straighten_traces(
    drc: &dyn Drc,
    problem: &RoutingView,
    solution: &mut RouteSolution,
    corners: Corners,
) {
    if !solution.traces.iter().any(|t| t.path.len() >= 3) {
        return;
    }
    let mut baseline = drc.check(problem, solution);
    let mut budget = LINT_BUDGET;

    while budget > 0 {
        let Some((accepted, findings)) =
            best_replacement(drc, problem, solution, &baseline, &mut budget, corners)
        else {
            return;
        };
        *solution = accepted;
        baseline = findings;
    }
}

/// The first accepted single-subpath replacement, scanning longest span first so
/// one accepted candidate removes as many corners as possible.
fn best_replacement(
    drc: &dyn Drc,
    problem: &RoutingView,
    solution: &RouteSolution,
    baseline: &[Finding],
    budget: &mut usize,
    corners: Corners,
) -> Option<(RouteSolution, Vec<Finding>)> {
    for ti in 0..solution.traces.len() {
        let path = &solution.traces[ti].path;
        let n = path.len();
        if n < 3 {
            continue;
        }
        for span in (2..n).rev() {
            for i in 0..(n - span) {
                let j = i + span;
                let old = &path[i..=j];
                for interior in replacements(old[0], old[span], corners) {
                    if interior.len() + 2 >= old.len() || longer(&interior, old) {
                        continue;
                    }
                    if *budget == 0 {
                        return None;
                    }
                    *budget -= 1;
                    let mut candidate = solution.clone();
                    candidate.traces[ti].path.splice(i + 1..j, interior);
                    let merged = geom::Polyline::new(std::mem::take(&mut candidate.traces[ti].path))
                        .simplify()
                        .into_points();
                    candidate.traces[ti].path = merged;
                    let findings = drc.check(problem, &candidate);
                    if !introduces_new_findings(baseline, &findings) {
                        return Some((candidate, findings));
                    }
                }
            }
        }
    }
    None
}

/// Which corner shapes a tidying pass may draw — the routing style the engine
/// was asked for, not a preference: straightening an orthogonal-only route into
/// 45-degree legs would change its character.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Corners {
    /// Axial runs joined by 45-degree legs.
    Octilinear,
    /// Axial runs only.
    Orthogonal,
}

/// The polyline a router emits from `from` to `to` when it must land exactly on
/// both — a pad centre at one end, a lattice cell centre at the other.
///
/// Octilinear style leaves `from` along an axis and turns 45° onto `to`;
/// orthogonal style turns the same offset into an axial L. Either way every
/// segment is octilinear, so a router never emits an arbitrary-angle leg just
/// because its endpoints did not line up.
pub fn leg(from: Point2, to: Point2, corners: Corners) -> Vec<Point2> {
    match corners {
        Corners::Octilinear => octilinear_path(from, to),
        Corners::Orthogonal => {
            if (from.x - to.x).abs() < geom::EPS || (from.y - to.y).abs() < geom::EPS {
                vec![from, to]
            } else {
                vec![from, Point2::new(to.x, from.y), to]
            }
        }
    }
}

/// The interior vertices of each way to get from `a` to `b`, best first: the two
/// 45°-knee forms (shortest, when allowed), then the two Manhattan L-corners,
/// which can still win on a genuine detour.
fn replacements(a: Point2, b: Point2, corners: Corners) -> Vec<Vec<Point2>> {
    let interior = |from: Point2, to: Point2| octilinear_path(from, to)[1..].to_vec();
    let mut out = Vec::new();
    if corners == Corners::Octilinear {
        let forward = interior(a, b);
        out.push(forward[..forward.len() - 1].to_vec());
        let backward = interior(b, a);
        out.push(backward[..backward.len() - 1].iter().rev().copied().collect());
    }
    out.push(vec![Point2::new(a.x, b.y)]);
    out.push(vec![Point2::new(b.x, a.y)]);
    out.dedup();
    out
}

/// Would routing `a -> interior -> b` lay more copper than the existing `old`
/// subpath?
fn longer(interior: &[Point2], old: &[Point2]) -> bool {
    let a = old[0];
    let b = old[old.len() - 1];
    let mut pts = Vec::with_capacity(interior.len() + 2);
    pts.push(a);
    pts.extend_from_slice(interior);
    pts.push(b);
    path_len(&pts) > path_len(old) + LENGTH_EPS
}

fn path_len(path: &[Point2]) -> f64 {
    path.windows(2).map(|w| w[0].dist(w[1])).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcb_model::{Connection, Findings, LayerRef, Obstacle, Rect, Trace};

    /// An oracle that never objects, so the tests measure the geometry rule
    /// rather than the design rules.
    struct Permissive;
    impl Drc for Permissive {
        fn name(&self) -> &'static str {
            "permissive"
        }
        fn check(&self, _: &RoutingView, _: &RouteSolution) -> Findings {
            Vec::new()
        }
    }

    /// An oracle that rejects any solution whose copper leaves the `x >= y`
    /// half-plane, so a test can force the fallback candidates.
    struct BelowDiagonal;
    impl Drc for BelowDiagonal {
        fn name(&self) -> &'static str {
            "below-diagonal"
        }
        fn check(&self, _: &RoutingView, solution: &RouteSolution) -> Findings {
            solution
                .traces
                .iter()
                .flat_map(|t| t.path.iter())
                .filter(|p| p.x + 1e-9 < p.y)
                .map(|p| Finding::OutOfBounds {
                    connection: String::new(),
                    overshoot: p.y - p.x,
                    at: *p,
                })
                .collect()
        }
    }

    fn problem() -> RoutingView {
        RoutingView {
            layer_count: 2,
            min_trace_width: 0.2,
            obstacles: Vec::<Obstacle>::new(),
            connections: Vec::<Connection>::new(),
            bounds: Rect {
                min_x: -100.0,
                max_x: 100.0,
                min_y: -100.0,
                max_y: 100.0,
            },
            clearance: 0.2,
            via_diameter: 0.6,
            via_drill: 0.3,
            net_widths: Default::default(),
            outline: None,
            escape_layers: Default::default(),
            plane_nets: Default::default(),
            fixed_copper: Default::default(),
            nets: None,
        }
    }

    fn solution(path: Vec<Point2>) -> RouteSolution {
        RouteSolution {
            traces: vec![Trace {
                connection: "N".into(),
                layer: LayerRef::top(),
                width: 0.2,
                path,
            }],
            vias: Vec::new(),
        }
    }

    /// A staircase: alternating axial and 45° steps toward the same corner. It
    /// is exactly as long as the smooth run, which is why a length-gated pass
    /// leaves it alone and this one must not.
    fn staircase(steps: usize) -> Vec<Point2> {
        let mut path = vec![Point2::new(0.0, 0.0)];
        for k in 0..steps {
            let x = k as f64 * 2.0;
            path.push(Point2::new(x + 1.0, k as f64 + 1.0));
            path.push(Point2::new(x + 2.0, k as f64 + 1.0));
        }
        path
    }

    #[test]
    fn a_staircase_collapses_to_two_segments() {
        let mut s = solution(staircase(6));
        let before = s.metrics();
        straighten_traces(&Permissive, &problem(), &mut s, Corners::Octilinear);
        let after = s.metrics();
        assert_eq!(s.traces[0].path.len(), 3, "{:?}", s.traces[0].path);
        assert!(after.bend_count < before.bend_count);
        assert!(after.wirelength <= before.wirelength + 1e-6);
        assert_eq!(after.off_angle_segments, 0);
    }

    #[test]
    fn endpoints_never_move() {
        let path = staircase(5);
        let (first, last) = (path[0], *path.last().unwrap());
        let mut s = solution(path);
        straighten_traces(&Permissive, &problem(), &mut s, Corners::Octilinear);
        assert_eq!(s.traces[0].path[0], first);
        assert_eq!(*s.traces[0].path.last().unwrap(), last);
    }

    #[test]
    fn a_detour_is_pulled_straight() {
        let mut s = solution(vec![
            Point2::new(0.0, 0.0),
            Point2::new(0.0, 20.0),
            Point2::new(10.0, 20.0),
            Point2::new(10.0, 4.0),
        ]);
        let before = s.metrics().wirelength;
        straighten_traces(&Permissive, &problem(), &mut s, Corners::Octilinear);
        assert!(s.metrics().wirelength < before);
        assert_eq!(s.metrics().off_angle_segments, 0);
    }

    #[test]
    fn an_already_straight_trace_is_untouched() {
        let path = vec![Point2::new(0.0, 0.0), Point2::new(5.0, 5.0)];
        let mut s = solution(path.clone());
        straighten_traces(&Permissive, &problem(), &mut s, Corners::Octilinear);
        assert_eq!(s.traces[0].path, path);
    }

    /// A rejected replacement must leave the copper exactly as it was.
    #[test]
    fn an_oracle_veto_is_respected() {
        let path = staircase(4);
        let mut s = solution(path.clone());
        straighten_traces(&BelowDiagonal, &problem(), &mut s, Corners::Octilinear);
        for p in &s.traces[0].path {
            assert!(p.x + 1e-9 >= p.y, "{p:?}");
        }
    }

    #[test]
    fn tidying_never_lengthens_copper_or_adds_corners() {
        for steps in 1..12 {
            let mut s = solution(staircase(steps));
            let before = s.metrics();
            straighten_traces(&Permissive, &problem(), &mut s, Corners::Octilinear);
            let after = s.metrics();
            assert!(after.wirelength <= before.wirelength + 1e-6, "{steps}");
            assert!(after.bend_count <= before.bend_count, "{steps}");
            assert_eq!(after.off_angle_segments, 0, "{steps}");
        }
    }

    #[test]
    fn an_orthogonal_leg_never_turns_45_degrees() {
        let leg = leg(Point2::new(0.0, 0.0), Point2::new(3.0, 7.0), Corners::Orthogonal);
        assert_eq!(leg.len(), 3);
        for w in leg.windows(2) {
            assert!(
                (w[0].x - w[1].x).abs() < 1e-9 || (w[0].y - w[1].y).abs() < 1e-9,
                "{:?}",
                w
            );
        }
    }

    #[test]
    fn an_orthogonal_route_is_never_straightened_into_a_diagonal() {
        let mut s = solution(vec![
            Point2::new(0.0, 0.0),
            Point2::new(0.0, 9.0),
            Point2::new(4.0, 9.0),
            Point2::new(4.0, 3.0),
            Point2::new(9.0, 3.0),
        ]);
        straighten_traces(&Permissive, &problem(), &mut s, Corners::Orthogonal);
        for w in s.traces[0].path.windows(2) {
            assert!(
                (w[0].x - w[1].x).abs() < 1e-9 || (w[0].y - w[1].y).abs() < 1e-9,
                "{:?}",
                w
            );
        }
    }
}
