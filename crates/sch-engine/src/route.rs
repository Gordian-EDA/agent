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

    #[test]
    fn vertical_lead_straight_and_l() {
        let p = elbow([0.0, 10.0], Dir::North, [0.0, 0.0]);
        assert_eq!(p, vec![[0.0, 10.0], [0.0, 0.0]]);
        let p = elbow([0.0, 10.0], Dir::North, [6.0, 2.0]);
        assert_eq!(p, vec![[0.0, 10.0], [0.0, 2.0], [6.0, 2.0]]);
        assert_axis_aligned(&p);
    }
}
