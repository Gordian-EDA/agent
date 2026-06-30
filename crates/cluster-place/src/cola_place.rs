//! Constraint-based placement via the `cola` stress solver — the wire-first objective the SA
//! only approximates and converges to inconsistently.
//!
//! Every signal-net pin pair is a spring at ideal length [`IDEAL`]; VPSC keeps bodies from
//! overlapping. Minimising the graph stress pulls connected parts ADJACENT, so their connections
//! are short and the router draws WIRES instead of net-labels — the human idiom. Unlike the
//! per-part SA (a local search that can strand a weakly-connected part far from its one neighbour),
//! every edge here is an active spring, so lone parts get pulled in too.

use geom::Point2;
use sch_place::ir::LayoutIr;
use sch_place::item::{Incidence, Item};
use sch_place::netclass::is_power_net;

/// Target length of a directly-connected edge (~10 grid) — short enough that the connection
/// renders as a wire, long enough to clear the bodies.
const IDEAL: f64 = 12.7;

/// Re-place `items` by constrained stress majorization, warm-started from their current
/// (SA-seeded) positions. Signal nets couple parts; rails/power are skipped (they couple
/// everything). Positions are snapped back to the 1.27 mm grid.
pub fn cola_place(items: &mut [Item], inc: &Incidence, ir: &LayoutIr) {
    let n = items.len();
    if n < 2 {
        return;
    }
    let mut edges: Vec<(usize, usize)> = Vec::new();
    for (net, pins) in inc {
        if ir.rails.contains_key(net) || is_power_net(net) {
            continue;
        }
        // Clique-expand only small nets — a fat bus would couple everything into a blob.
        if pins.len() <= 5 {
            for a in 0..pins.len() {
                for b in (a + 1)..pins.len() {
                    if pins[a].0 != pins[b].0 {
                        edges.push((pins[a].0, pins[b].0));
                    }
                }
            }
        }
    }
    if edges.is_empty() {
        return;
    }
    let sm = cola::StressMajorizer::new(n, &edges, IDEAL);
    let x0: Vec<f64> = items.iter().map(|it| it.at.x).collect();
    let y0: Vec<f64> = items.iter().map(|it| it.at.y).collect();
    let sizes: Vec<(f64, f64)> = items.iter().map(half_extents).collect();
    let (x, y) = sm.run(&x0, &y0, &sizes, &[], &[], 200);
    let snap = |v: f64| (v / 1.27).round() * 1.27;
    for (i, it) in items.iter_mut().enumerate() {
        it.at = Point2::new(snap(x[i]), snap(y[i]));
    }
}

/// Half-extents of a part's body (rotation-aware) plus a one-grid clearance — the VPSC
/// non-overlap keeps this much air between every pair, leaving room for pin/label text.
fn half_extents(it: &Item) -> (f64, f64) {
    let s = it.geom.approx_size();
    let quarter = ((it.angle / 90.0).round() as i64).rem_euclid(2) == 1;
    let (w, h) = if quarter { (s.y, s.x) } else { (s.x, s.y) };
    (w / 2.0 + 1.27, h / 2.0 + 1.27)
}
