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
    // GRAVITY: a virtual centroid node (index `n`) linked to every part. The stress skips
    // disconnected pairs (weight 1/d²), so an edge-less INTERFACE part — a resistor with one pin
    // on a GPIO label and the other a no-connect — has no spring and would strand wherever the SA
    // left it. The centroid edge gives every part a gentle pull into the layout, so the human's
    // "pack the interface parts tight too" falls out. Its final position is discarded.
    let center = n;
    for i in 0..n {
        edges.push((i, center));
    }
    let sm = cola::StressMajorizer::new(n + 1, &edges, IDEAL);
    let mut x0: Vec<f64> = items.iter().map(|it| it.at.x).collect();
    let mut y0: Vec<f64> = items.iter().map(|it| it.at.y).collect();
    let cx = x0.iter().sum::<f64>() / n as f64;
    let cy = y0.iter().sum::<f64>() / n as f64;
    x0.push(cx);
    y0.push(cy);
    let mut sizes: Vec<(f64, f64)> = items.iter().map(half_extents).collect();
    sizes.push((0.0, 0.0)); // the centroid is a point, no body
    let (x, y) = sm.run(&x0, &y0, &sizes, &[], &[], 200);
    let snap = |v: f64| (v / 1.27).round() * 1.27;
    for (i, it) in items.iter_mut().enumerate() {
        it.at = Point2::new(snap(x[i]), snap(y[i]));
    }
}

/// Label-aware half-extents: the part's body (rotation-aware) plus room for its refdes/value
/// text and a net-label gap. The VPSC non-overlap keeps this much air between every pair, so the
/// tight stress pack still leaves the text solver room to place labels without overlap (the cola
/// crate's historical blocker was packing into label space → readability warnings).
fn half_extents(it: &Item) -> (f64, f64) {
    let s = it.geom.approx_size();
    let quarter = ((it.angle / 90.0).round() as i64).rem_euclid(2) == 1;
    let (w, h) = if quarter { (s.y, s.x) } else { (s.x, s.y) };
    let text = it.value.chars().count().max(2) as f64 * 1.1;
    (w / 2.0 + text * 0.5 + 1.27, h / 2.0 + 3.0)
}
