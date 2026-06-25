//! `HoleClearanceRule` — KiCAD's drill-EDGE rule (hole-to-hole / track-to-hole /
//! hole-to-copper, 0.25 mm).
//!
//! The copper-clearance checks don't cover it: a via's drill sits its annular
//! INSIDE its copper, so for a small via copper clearance can still leave the
//! DRILL too close to a foreign track or another drill. Surfaced as
//! [`Finding::ClearanceViaAny`] so the drop path turns it into an honest
//! unrouted net rather than a shipped fault. Covers via↔via, via↔track, AND
//! via↔foreign-PAD copper (a via's drill must clear foreign pad copper too; a
//! same-net pad is intentional via-in-pad and skipped).

use crate::rules::geom::EPS;
use crate::{DrcCtx, Finding, Rule};

/// KiCAD's drill-edge (hole) clearance, mm.
const HOLE_CLEAR: f64 = 0.25;

/// Flags any via whose drill edge sits within `HOLE_CLEAR` of another drill, a
/// foreign track, or a foreign pad.
pub struct HoleClearanceRule;

impl Rule for HoleClearanceRule {
    fn name(&self) -> &'static str {
        "hole-clearance"
    }

    fn check(&self, ctx: &DrcCtx) -> Vec<Finding> {
        let problem = ctx.problem;
        let solution = ctx.solution;
        let mut out = Vec::new();

        for (vi, a) in solution.vias.iter().enumerate() {
            let ar = a.drill / 2.0;
            let aat = [a.at.x, a.at.y];
            // drill ↔ drill: a mechanical (drill-bit) rule, independent of net.
            for b in &solution.vias[vi + 1..] {
                let gap = a.at.dist(b.at) - ar - b.drill / 2.0;
                if gap + EPS < HOLE_CLEAR {
                    out.push(Finding::ClearanceViaAny {
                        connection: a.connection.clone(),
                        other_owners: vec![b.connection.clone()],
                        gap,
                        required: HOLE_CLEAR,
                        at: aat,
                    });
                }
            }
            // FOREIGN track copper ↔ this via's drill edge (same-net track connects to it).
            for t in &solution.traces {
                if t.connection == a.connection {
                    continue;
                }
                let hw = t.width / 2.0;
                if t.path.windows(2).any(|w| {
                    geom::Segment::new(w[0], w[1]).dist_to_point(a.at) - hw - ar + EPS < HOLE_CLEAR
                }) {
                    out.push(Finding::ClearanceViaAny {
                        connection: a.connection.clone(),
                        other_owners: vec![t.connection.clone()],
                        gap: 0.0,
                        required: HOLE_CLEAR,
                        at: aat,
                    });
                }
            }
            // FOREIGN pad copper ↔ this via's drill edge (hole-to-copper). A same-net pad is
            // via-in-pad (intentional), so skip it; any other pad must clear the drill by 0.25.
            for ob in &problem.obstacles {
                if ob.connected_to.contains(&a.connection) {
                    continue;
                }
                let dx = (a.at.x - ob.center.x).abs() - ob.width / 2.0;
                let dy = (a.at.y - ob.center.y).abs() - ob.height / 2.0;
                let gap = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt() - ar;
                if gap + EPS < HOLE_CLEAR {
                    out.push(Finding::ClearanceViaAny {
                        connection: a.connection.clone(),
                        other_owners: ob.connected_to.clone(),
                        gap,
                        required: HOLE_CLEAR,
                        at: aat,
                    });
                }
            }
        }
        out
    }
}
