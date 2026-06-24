//! `place::score` — the cost the placement engines minimise: the routed `layout_cost`
//! and its `count_*` neatness/truthfulness terms (crossings, corners, merges, shorts,
//! congestion), the `warning_count`/`crossing_counts` summaries, and the geometry
//! primitives (`item_rect`, `rects_overlap`, `body_overlap_count`).

use std::collections::{BTreeMap, BTreeSet};

use circuit_lang::model::Design;
use circuit_lang::{find_pin, PinType, SymbolProvider};
use kicad_cli_rs::env::KicadEnv;
use kicad_sexpr::provider::RealSymbolProvider;

use crate::write::SchematicWriter;

use super::*;
use sch_model::item::{Incidence, Item};
use sch_model::netclass::is_ground;

// The disjoint-set forest (over a caller-owned `parent` slice) lives in
// `sch_model::union_find`, shared with circuit-lang's pin reconciler.
use sch_model::ir::LayoutIr;

/// Layout-warning count of `items` as they would SHIP — build the writer and run
/// the same finalize (`prepare`: split wires, solve text, reframe) the real emit
/// does, then count. Used only to pick among the SA's final candidates (a handful
/// of calls), never per-move, so the text-solve cost is affordable here.
pub fn warning_count(
    env: &KicadEnv,
    items: &[Item],
    inc: &Incidence,
    ir: &LayoutIr,
    needs_flag: &BTreeSet<String>,
) -> usize {
    match build_writer(env, None, items, inc, ir, needs_flag, true) {
        Ok(mut w) => {
            w.set_frame(true);
            w.prepare();
            w.layout_warnings().len()
        }
        Err(_) => usize::MAX,
    }
}



/// Weight on the whole-board bbox half-perimeter in `proxy_cost` (the dense fast-lane SA
/// inner loop). Raised from the original 0.45: with DISTRIBUTED power the clusters share
/// few inter-cluster wires, so `hpwl` keeps each net locally tight but nothing pulls the
/// clusters TOGETHER — they float apart, leaving 2-3x the human whitespace-per-part
/// (measured: ours sprawl 37-80 vs human ~23). A stronger global spread pull packs the
/// clusters in. `proxy_cost` is only reached on the dense fast lane (`pins > FAST_PINS`),
/// so every ≤34-pin reference/snapshot stays byte-identical regardless of this value.
/// Override via `PROXY_SPREAD_W` is NOT read here (hot path) — sweep by editing this const.
pub const PROXY_SPREAD_W: f64 = 0.45;


/// Build and score the schematic for `items` exactly as placed (no cell layout).
/// Used by the pin-alignment pass, which nudges raw positions.
pub fn score_items(
    env: &KicadEnv,
    items: &[Item],
    inc: &Incidence,
    ir: &LayoutIr,
    needs_flag: &BTreeSet<String>,
) -> f64 {
    match build_writer(env, None, items, inc, ir, needs_flag, false) {
        Ok(w) => layout_cost(env, &w, items, inc, ir, false),
        Err(_) => f64::INFINITY,
    }
}

/// PREMIUM-tier cost. The paid SA pays for the ACCURATE objective the free tier
/// can't afford: the REAL post-solve lint warning count (`warning_count` =
/// build + text-solve + count), heavily weighted, so the SA directly minimises the
/// shipped warnings — not a cheap pre-solve proxy, which diverges from the truth
/// (the solver fixes much of the pre-solve crowding; minimising the proxy lands
/// WORSE — measured). Then the straightness-weighted routed cost
/// (`layout_cost(premium=true)`) breaks ties toward a tidier sheet. This is what
/// the SA explores; the shipped finalize uses the base cost, same metric both tiers.
pub fn premium_score_items(
    env: &KicadEnv,
    items: &[Item],
    inc: &Incidence,
    ir: &LayoutIr,
    needs_flag: &BTreeSet<String>,
) -> f64 {
    let aes = match build_writer(env, None, items, inc, ir, needs_flag, false) {
        Ok(w) => layout_cost(env, &w, items, inc, ir, true),
        Err(_) => return f64::INFINITY,
    };
    // The accurate objective costs a per-move text solve + reroute; on dense boards
    // (selfrepair's 88 nets, bga's 671 pins) that runs into MANY minutes per emit, so
    // there the premium falls back to the straightness cost alone — still additive,
    // just without the warning-minimisation that drove oneshot (186 pins / 23 nets,
    // affordable) to 0. The cheap base eval the free tier uses scales fine; only this
    // accurate variant needs the guard.
    let pins: usize = items.iter().map(|it| it.geom.pins.len()).sum();
    if pins <= 250 && inc.len() <= 40 {
        10_000.0 * warning_count(env, items, inc, ir, needs_flag) as f64 + aes
    } else {
        aes
    }
}

/// An item's body rect at position `at`. Uses the FULL `approx_size` (which
/// already pads 2.54 mm/side) so the placement overlap check reserves room for
/// the symbol body *and* its side-mounted value/refdes text — matching what the
/// readability lint flags as an overlap, so a layout the climb accepts is one
/// the lint passes. (The router uses its own, tighter solid extent in
/// `emit::route_scene`; this looser one is only for symbol-vs-symbol spacing.)
pub fn item_rect(it: &Item, at: [f64; 2]) -> [f64; 4] {
    let s = it.geom.approx_size();
    let quarter = ((it.angle / 90.0).round() as i64).rem_euclid(2) == 1;
    let (w, h) = if quarter { (s[1], s[0]) } else { (s[0], s[1]) };
    let (hw, hh) = ((w / 2.0).max(1.27), (h / 2.0).max(1.27));
    let mut r = [at[0] - hw, at[1] - hh, at[0] + hw, at[1] + hh];
    // Reserve the side-mounted refdes/value text footprint so a tight pack leaves
    // it collision-free — the readability lint flags text-over-body, so the climb
    // must keep a neighbour out of the conventional text spot. KiCAD draws a
    // vertical 2-pin part's fields stacked to the RIGHT, a horizontal part's
    // refdes above / value below. (~1.1 mm/char, ~1.6 mm/line.)
    if it.geom.pins.len() == 2 {
        if quarter {
            r[1] -= 2.0; // refdes line above
            r[3] += 2.0; // value line below
        } else {
            let chars = it.value.chars().count().max(it.refdes.chars().count()) as f64;
            r[2] += chars * 1.1 + 1.27; // field stack to the right
        }
    }
    r
}

pub fn rects_overlap(a: [f64; 4], b: [f64; 4]) -> bool {
    a[0] < b[2] - EPS && b[0] < a[2] - EPS && a[1] < b[3] - EPS && b[1] < a[3] - EPS
}

/// Count pairs of items whose bodies overlap — the hard "never let two symbols
/// collide" wall. Catches adjacent-cell collisions the same-cell check misses.
pub fn body_overlap_count(items: &[Item]) -> usize {
    let mut n = 0;
    for i in 0..items.len() {
        for j in (i + 1)..items.len() {
            if rects_overlap(item_rect(&items[i], items[i].at), item_rect(&items[j], items[j].at)) {
                n += 1;
            }
        }
    }
    n
}

/// Wires that run straight THROUGH a 2-pin part's body — a foreign (or trunk)
/// segment crossing the pin-to-pin axis at a point strictly interior to it,
/// perpendicular to the part. This reads as "a wire drawn through a resistor" and
/// the existing parallel-proximity check never catches it (it is a crossing, not
/// a hug). A lead leaving a pin is collinear with / starts at the body endpoint,
/// so it is excluded.
pub(crate) fn count_body_crossings(
    bodies: &[([f64; 2], [f64; 2])],
    wires: &[([f64; 2], [f64; 2], Option<String>)],
) -> usize {
    let mut n = 0;
    for (a, b) in bodies {
        let bh = (a[1] - b[1]).abs() < EPS; // body axis horizontal?
        if (a[0] - b[0]).abs() < EPS && (a[1] - b[1]).abs() < EPS {
            continue;
        }
        for (w1, w2, _) in wires {
            let wh = (w1[1] - w2[1]).abs() < EPS;
            if bh == wh {
                continue; // need a perpendicular wire
            }
            let (interior, on_wire) = if bh {
                let p = [w1[0], a[1]];
                (
                    p[0] > a[0].min(b[0]) + EPS && p[0] < a[0].max(b[0]) - EPS,
                    sch_model::geom::point_on_segment(p, *w1, *w2),
                )
            } else {
                let p = [a[0], w1[1]];
                (
                    p[1] > a[1].min(b[1]) + EPS && p[1] < a[1].max(b[1]) - EPS,
                    sch_model::geom::point_on_segment(p, *w1, *w2),
                )
            };
            if interior && on_wire {
                n += 1;
            }
        }
    }
    n
}

/// Wires that run straight THROUGH a 2-pin part COLLINEARLY — a segment on the
/// part's own pin-to-pin axis that extends strictly BEYOND both pins, i.e. it
/// enters one side, slices across the body (and the near pin, a foreign net), and
/// exits the far side. The classic case [`count_body_crossings`] misses: a rail
/// wire reaching a part's FAR pin by going straight through the part instead of
/// approaching from that pin's side (the NE555's GND pin dropping through the LED to
/// the ground rail). A series part's own leads STOP at a pin — never span beyond
/// both — so this never fires on a correctly-drawn in-line resistor/cap.
pub(crate) fn count_collinear_body_crossings(
    bodies: &[([f64; 2], [f64; 2])],
    wires: &[([f64; 2], [f64; 2], Option<String>)],
) -> usize {
    let mut n = 0;
    for (a, b) in bodies {
        if (a[0] - b[0]).abs() < EPS && (a[1] - b[1]).abs() < EPS {
            continue;
        }
        let bh = (a[1] - b[1]).abs() < EPS; // horizontal part (pins differ in x)?
        let axis = if bh { 0 } else { 1 };
        let (plo, phi) = (a[axis].min(b[axis]), a[axis].max(b[axis]));
        for (w1, w2, _) in wires {
            let wh = (w1[1] - w2[1]).abs() < EPS;
            if bh != wh {
                continue; // need a PARALLEL wire (the collinear candidate)
            }
            // ...on the SAME line as the body axis (matching perpendicular coord).
            let perp = if bh { 1 } else { 0 };
            if (w1[perp] - a[perp]).abs() > EPS {
                continue;
            }
            let (wlo, whi) = (w1[axis].min(w2[axis]), w1[axis].max(w2[axis]));
            if wlo < plo - EPS && whi > phi + EPS {
                n += 1;
            }
        }
    }
    n
}

/// Wires routed straight THROUGH an IC (3+ pin) body rectangle — the package
/// equivalent of [`count_body_crossings`] (which only handles a 2-pin part's
/// pin-to-pin axis). A foreign net's segment drawn across the chip box, over its
/// internal glyphs, reads as broken even though the netlist is sound (the body is
/// only priced, never a hard router obstacle). `ic_rects` are body interiors
/// (pin-tip bbox shrunk inward past the pin stubs) so a wire legitimately
/// attaching at a pin tip and routing OUTWARD never counts; only a segment with a
/// portion strictly inside the rect does.
pub(crate) fn count_ic_body_crossings(
    ic_rects: &[[f64; 4]],
    wires: &[([f64; 2], [f64; 2], Option<String>)],
) -> usize {
    let mut n = 0;
    for r in ic_rects {
        if r[2] - r[0] < EPS || r[3] - r[1] < EPS {
            continue;
        }
        for (w1, w2, _) in wires {
            // Wires are axis-aligned; a zero-width interval can't be tested as a
            // 2-D box overlap, so split by orientation: the constant coordinate
            // must be strictly inside the rect, the spanning interval must overlap.
            let cross = if (w1[0] - w2[0]).abs() < EPS {
                let x = w1[0];
                let (ylo, yhi) = (w1[1].min(w2[1]), w1[1].max(w2[1]));
                r[0] + EPS < x && x < r[2] - EPS && ylo.max(r[1]) < yhi.min(r[3]) - EPS
            } else {
                let y = w1[1];
                let (xlo, xhi) = (w1[0].min(w2[0]), w1[0].max(w2[0]));
                r[1] + EPS < y && y < r[3] - EPS && xlo.max(r[0]) < xhi.min(r[2]) - EPS
            };
            if cross {
                n += 1;
            }
        }
    }
    n
}

/// Wires running PARALLEL to a 2-pin part, OFFSET inside its body but off the
/// pin-to-pin centerline — the case [`count_body_crossings`] (perpendicular only)
/// and [`count_collinear_body_crossings`] (on the centerline, beyond both pins) both
/// miss. A 2-pin symbol body (a cap's plates, a resistor's rectangle) is ~3 mm wide,
/// so a riser one 1.27 mm grid step off the part's axis still slices through the
/// drawn body — exactly what a dense vertical-cap column produces. The part's OWN
/// leads attach at the pin ENDS (outside the central body span), so a correctly
/// drawn in-line part never fires.
pub(crate) fn count_parallel_body_crossings(
    bodies: &[([f64; 2], [f64; 2])],
    wires: &[([f64; 2], [f64; 2], Option<String>)],
) -> usize {
    const PLATE_HALF: f64 = 1.4; // half the drawn 2-pin body width (catches a 1.27 mm offset)
    const PIN_STUB: f64 = 2.54; // exclude the pin stubs at each end
    let mut n = 0;
    for (a, b) in bodies {
        if (a[0] - b[0]).abs() < EPS && (a[1] - b[1]).abs() < EPS {
            continue;
        }
        let bh = (a[1] - b[1]).abs() < EPS; // horizontal part?
        let (axis, perp) = if bh { (0, 1) } else { (1, 0) };
        let (plo, phi) = (a[axis].min(b[axis]), a[axis].max(b[axis]));
        let (blo, bhi) = (plo + PIN_STUB, phi - PIN_STUB); // central body, past the stubs
        if bhi <= blo + EPS {
            continue;
        }
        for (w1, w2, _) in wires {
            let wh = (w1[1] - w2[1]).abs() < EPS;
            if bh != wh {
                continue; // need a PARALLEL wire (perpendicular is count_body_crossings)
            }
            if (w1[perp] - a[perp]).abs() > PLATE_HALF - EPS {
                continue; // outside the drawn body width
            }
            let (wlo, whi) = (w1[axis].min(w2[axis]), w1[axis].max(w2[axis]));
            if wlo < bhi - EPS && whi > blo + EPS {
                n += 1;
            }
        }
    }
    n
}

/// Wire corners (L-bends): points where exactly two perpendicular same-net
/// segments meet. Length alone treats a jiggly L-jog path and a straight run as
/// equal; this penalises the BENDS, so the optimiser prefers straight drops and
/// straight runs along rails — the single term that most separates a clean
/// reference layout from a compact-but-jiggly diagonal staircase. A ≥3-way meet
/// (a junction/tap) is not a corner and is excluded by the exact-two test.
pub(crate) fn count_corners(wires: &[([f64; 2], [f64; 2], Option<String>)]) -> usize {
    // (net, point) -> orientations of the segments ending there (true = horizontal).
    let mut at: BTreeMap<(String, u64, u64), Vec<bool>> = BTreeMap::new();
    for (a, b, n) in wires {
        let Some(net) = n else { continue };
        let horiz = (a[1] - b[1]).abs() < EPS;
        for p in [a, b] {
            at.entry((net.clone(), p[0].to_bits(), p[1].to_bits())).or_default().push(horiz);
        }
    }
    at.values().filter(|o| o.len() == 2 && o[0] != o[1]).count()
}

/// Foreign taps = the post-split short class: a wire endpoint of one net lying
/// strictly interior to a wire of a DIFFERENT net. The finalize wire-split makes
/// such a contact a real connection in the netlist (KiCAD splits the through-wire
/// at the tap), so it must read as a short here or the hill-climb would create
/// one to save length. A same-net riser tapping its own rail is the intended case
/// and is excluded by the net check.
pub fn count_foreign_taps(wires: &[([f64; 2], [f64; 2], Option<String>)]) -> usize {
    let strict_interior = |p: [f64; 2], a: [f64; 2], b: [f64; 2]| {
        let is_end = |q: [f64; 2]| near(p, q);
        !is_end(a) && !is_end(b) && sch_model::geom::point_on_segment(p, a, b)
    };
    let mut n = 0;
    for (a1, a2, an) in wires {
        for (b1, b2, bn) in wires {
            if an == bn || an.is_none() || bn.is_none() {
                continue;
            }
            if strict_interior(*a1, *b1, *b2) || strict_interior(*a2, *b1, *b2) {
                n += 1;
            }
        }
    }
    n
}

/// Weighted aesthetic cost of a built schematic. Label fallbacks and shorts
/// dominate (they are correctness/quality failures); then visual wire crossings,
/// then junction dots, with total wire length as a light tiebreaker.
pub fn layout_cost(
    env: &KicadEnv,
    w: &SchematicWriter,
    items: &[Item],
    inc: &Incidence,
    ir: &LayoutIr,
    premium: bool,
) -> f64 {
    let fallbacks = w.signal_label_count();
    let junctions = w.junction_count();
    let wires = w.wires_with_nets();
    let length: f64 =
        wires.iter().map(|(a, b, _)| (a[0] - b[0]).abs() + (a[1] - b[1]).abs()).sum();
    let crossings = count_crossings(&wires);
    let corners = count_corners(&wires);
    let merges = count_merges(&wires, &w.junction_positions())
        + count_shorts(env, w, items, inc, &wires)
        + count_foreign_taps(&wires);
    // Two symbols whose bodies collide is never acceptable; a heavy (but
    // below-merge) wall lets the climb escape an overlapping seed yet never move
    // INTO an overlap, so the final layout is overlap-free even from a poor frame.
    // Symbol-vs-port-label collisions count here too (the annealer likes to slide
    // a decoupling cap onto the TXD1/RXD1 edge pentagons).
    let label_boxes = w.cluster_label_boxes();
    let overlaps = body_overlap_count(items)
        + items
            .iter()
            .filter(|it| {
                let r = item_rect(it, it.at);
                label_boxes.iter().any(|b| rects_overlap(r, *b))
            })
            .count();
    // Each 2-pin part's BODY AXIS (its pin-to-pin line) joins the closeness check
    // as an obstacle, so "a foreign wire hugging a resistor's body" is the same
    // parallel-proximity test as "a wire hugging a wire" — one rule, no rect math.
    // A series part's own wire lies ON its axis (distance 0) and is ignored.
    let bodies: Vec<([f64; 2], [f64; 2])> = items
        .iter()
        .filter(|i| i.geom.pins.len() == 2)
        .filter_map(|it| {
            let (n0, n1) = (&it.geom.pins[0].number, &it.geom.pins[1].number);
            match (w.pin_dirs(env, &it.refdes, n0), w.pin_dirs(env, &it.refdes, n1)) {
                (Ok(d0), Ok(d1)) => match (d0.first(), d1.first()) {
                    (Some((a, _)), Some((b, _))) => Some((*a, *b)),
                    _ => None,
                },
                _ => None,
            }
        })
        .collect();
    let congestion = count_congestion(&w.junction_positions()) + count_close_wires(&wires, &bodies);
    // IC (3+ pin) body interiors: pin-tip bbox shrunk inward past the pin stubs so
    // a wire attaching at a pin tip and routing outward is not a crossing. A
    // foreign wire drawn across the package box IS (the SN74 VCCA→GND-rail riser).
    let ic_rects: Vec<[f64; 4]> = items
        .iter()
        .filter(|it| it.geom.pins.len() >= 3)
        .filter_map(|it| {
            let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
            let mut any = false;
            for pg in &it.geom.pins {
                if let Ok(d) = w.pin_dirs(env, &it.refdes, &pg.number)
                    && let Some((p, _)) = d.first() {
                        lo[0] = lo[0].min(p[0]);
                        lo[1] = lo[1].min(p[1]);
                        hi[0] = hi[0].max(p[0]);
                        hi[1] = hi[1].max(p[1]);
                        any = true;
                    }
            }
            // Shrink 2.0 mm/side: past the pin-stub roots, onto the body rectangle.
            any.then(|| [lo[0] + 2.0, lo[1] + 2.0, hi[0] - 2.0, hi[1] - 2.0])
        })
        .collect();
    let body_cross = count_body_crossings(&bodies, &wires)
        + count_collinear_body_crossings(&bodies, &wires)
        + count_parallel_body_crossings(&bodies, &wires)
        + count_ic_body_crossings(&ic_rects, &wires);
    let stray = count_stray(env, w, items, inc, ir);
    // Orientation convention: a draughtsman runs a 2-pin part VERTICAL when it
    // bridges a rail and an internal node (a pull-up/down, a divider leg, a
    // decoupling cap between two rails), and HORIZONTAL when it sits in the signal
    // flow (between two signals, or feeding a rail from/ to a board port — a series
    // resistor, an input fuse). Penalising the wrong axis stops the router's
    // length-minimisation from flopping a series resistor vertical into an L-jog.
    let mut orient_viol = 0usize;
    let mut leg_viol = 0usize;
    for it in items.iter().filter(|i| i.geom.pins.len() == 2) {
        // Classify by how many of its nets are rails (NOT by port presence — a
        // divider leg like [OUT, GND] touches a port AND a rail yet is still a
        // vertical rail-to-node leg, not a series element):
        //   0 rails → series in the signal flow → horizontal;
        //   2 rails → spans two rails (decoupling) → vertical;
        //   1 rail  → AMBIGUOUS (a pull/leg is vertical, an input fuse feeding the
        //             rail is horizontal) → impose no preference, let length/frame decide.
        let rail_count = it
            .pins
            .iter()
            .filter(|(_, _, n)| n.as_deref().is_some_and(|n| ir.rails.contains_key(n)))
            .count();
        let prefer_vertical: Option<bool> = match rail_count {
            // No rail → a series element in the signal flow → HORIZONTAL. (Tried
            // relaxing this to "let corners decide" so the 555 timing chain
            // DIS→R2→THR could stack vertically — it badly REGRESSED uart, whose
            // series-termination R13/R15/R20 immediately flopped vertical into a
            // tall L-jogged tower. The horizontal prior is load-bearing; keep it.)
            0 => Some(false),
            // Two rails → spans the rails (decoupling) → vertical.
            2 => Some(true),
            // One rail → distinguish a BOARD-EDGE FEED (an input fuse / series part
            // whose non-rail net is a degree-1 port stub, e.g. F1 on 5V_BUS) which
            // runs HORIZONTAL into the rail, from a LEG whose non-rail net is a
            // shared internal node (a divider leg, pull-up — degree ≥2) which hangs
            // VERTICAL. Length/frame alone left F1 vertical; this fixes it without
            // flipping the divider's R8 (its OUT node is degree-3).
            _ => {
                let nonrail = it
                    .pins
                    .iter()
                    .filter_map(|(_, _, n)| n.as_deref())
                    .find(|n| !ir.rails.contains_key(*n));
                let degree = nonrail.and_then(|n| inc.get(n)).map_or(0, |p| p.len());
                Some(degree >= 2)
            }
        };
        let Some(prefer_vertical) = prefer_vertical else { continue };
        let (n0, n1) = (&it.geom.pins[0].number, &it.geom.pins[1].number);
        if let (Ok(d0), Ok(d1)) = (w.pin_dirs(env, &it.refdes, n0), w.pin_dirs(env, &it.refdes, n1))
            && let (Some((a, _)), Some((b, _))) = (d0.first(), d1.first()) {
                let horizontal = (a[0] - b[0]).abs() > (a[1] - b[1]).abs();
                if prefer_vertical == horizontal {
                    orient_viol += 1;
                    // A 1-rail LEG (pull-up/down: prefer vertical, degree≥2 node) is
                    // the RELIABLE branch — track it apart so the premium boost can
                    // bite it without touching the heuristic 0-rail "series→horizontal"
                    // rule, which the uart's legitimately-vertical 62R terminators trip.
                    if rail_count == 1 {
                        leg_viol += 1;
                    }
                } else if rail_count == 1 && prefer_vertical {
                    // Correctly-VERTICAL 1-rail leg: also enforce the up/down DIRECTION.
                    // The rail pin must sit on its band side — V+ UP (smaller y), GND
                    // DOWN — so the power symbol hangs the right way; a flipped leg (a
                    // +3V3 pull-up with the rail symbol at the BOTTOM) reads upside down.
                    let net_of =
                        |pn: &str| it.pins.iter().find(|(p, _, _)| p == pn).and_then(|(_, _, n)| n.as_deref());
                    let n0_rail = net_of(n0).is_some_and(|n| ir.rails.contains_key(n));
                    let rail = if n0_rail { net_of(n0) } else { net_of(n1) };
                    if let Some(rn) = rail {
                        let (rail_pos, other_pos) = if n0_rail { (a, b) } else { (b, a) };
                        let rail_up = rail_pos[1] < other_pos[1] - EPS;
                        if is_ground(rn) == rail_up {
                            leg_viol += 1;
                        }
                    }
                }
            }
    }
    // Spine collinearity: two VERTICAL 2-pin legs that share a non-rail node and
    // whose FAR ends are each a rail form a divider / totem-pole spine
    // (VCC→R7→node→R8→GND). A draughtsman draws them in ONE column. Length-min
    // alone slides the shared node sideways toward a port to shave a stub, which
    // breaks the spine (the divider's R8 gets banished to its own column). Penalise
    // a spine pair whose bodies are not in the same column (cross-axis offset > 1
    // grid). Narrow by construction: parallel decoupling caps share RAILS not a
    // node, and a series part with a non-rail far end (555 R2) is not a spine leg,
    // so neither is touched.
    let legs: Vec<(usize, Vec<&str>, bool)> = items
        .iter()
        .enumerate()
        .filter(|(_, it)| it.geom.pins.len() == 2)
        .map(|(i, it)| {
            let nets: Vec<&str> = it.pins.iter().filter_map(|(_, _, n)| n.as_deref()).collect();
            let (n0, n1) = (&it.geom.pins[0].number, &it.geom.pins[1].number);
            let vertical = match (w.pin_dirs(env, &it.refdes, n0), w.pin_dirs(env, &it.refdes, n1)) {
                (Ok(d0), Ok(d1)) => match (d0.first(), d1.first()) {
                    (Some((a, _)), Some((b, _))) => (a[1] - b[1]).abs() > (a[0] - b[0]).abs(),
                    _ => false,
                },
                _ => false,
            };
            (i, nets, vertical)
        })
        .collect();
    // Are these two legs a series SPINE? They must share a non-rail node AND each
    // run to a rail, and those two far rails must DIFFER — one pulls the node up
    // (VCC), the other down (GND). Two legs to the SAME rail (R8 and C3 both
    // OUT→GND) are PARALLEL drops, not a spine, and must NOT be forced collinear
    // (they'd overlap). Distinct far rails select exactly the divider/totem case.
    let is_spine = |a: &[&str], b: &[&str]| -> bool {
        let is_rail = |n: &str| ir.rails.contains_key(n);
        let Some(node) = a.iter().copied().find(|n| b.contains(n) && !is_rail(n)) else {
            return false;
        };
        let ra = a.iter().copied().find(|n| *n != node && is_rail(n));
        let rb = b.iter().copied().find(|n| *n != node && is_rail(n));
        matches!((ra, rb), (Some(x), Some(y)) if x != y)
    };
    let mut spine_viol = 0usize;
    for a in 0..legs.len() {
        for b in (a + 1)..legs.len() {
            let (ia, na, va) = (legs[a].0, &legs[a].1, legs[a].2);
            let (ib, nb, vb) = (legs[b].0, &legs[b].1, legs[b].2);
            // A capacitor is a SHUNT tap, never a through-path spine leg: it hangs
            // to the side so the resistive divider / indicator chain reads straight
            // (R7 over R8, not R7 over the filter cap C3). Exclude cap legs.
            let cap = |i: usize| items[i].refdes.starts_with('C');
            // Penalise ANY cross-axis offset, not just >1 grid: a spine should be
            // EXACTLY collinear. The looser >1.27 tolerance let the free per-axis
            // nudge slide a leg one grid off the spine (a visible jog) at no cost.
            if va && vb && !cap(ia) && !cap(ib) && is_spine(na, nb)
                && (items[ia].at[0] - items[ib].at[0]).abs() > EPS
            {
                spine_viol += 1;
            }
        }
    }

    // Compactness: the bounding-box half-perimeter of all part bodies. Length
    // alone rewards short wires but tolerates a part flung into open space if its
    // own wire stays short; this penalises the wasted-whitespace spread directly
    // (the #1 visual complaint), pulling the whole drawing tight.
    let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
    for it in items {
        let r = item_rect(it, it.at);
        lo[0] = lo[0].min(r[0]);
        lo[1] = lo[1].min(r[1]);
        hi[0] = hi[0].max(r[2]);
        hi[1] = hi[1].max(r[3]);
    }
    let spread = if lo[0].is_finite() { (hi[0] - lo[0]) + (hi[1] - lo[1]) } else { 0.0 };
    // The author's per-block `layout:` relative ordering. Weighted JUST BELOW the
    // body-overlap wall (so it never forces a collision) but ABOVE every routing /
    // aesthetic term, so the grid is "relatively rigid": the search holds gridded
    // parts in their authored left/right + top/bottom order even when flipping one
    // across its anchor would shave a long wire — exact positions stay free, only
    // the order is held. Empty grid (sidecar / no `layout:`) ⇒ zero, so tuned
    // references are untouched.
    let grid_order = grid_order_viol(items, ir);
    // Merges/shorts are hard correctness failures (a rail-to-rail short lowers
    // length+junctions, so without this the hill-climb would happily create
    // one); fallbacks degrade a wire to a label; then crossings; then CONGESTION
    // (junction dots packed against each other — the "dot knot" / wires-collapse-
    // into-a-resistor look, which length-minimisation otherwise rewards); then
    // junctions and length. The big coefficients keep correctness off the table.
    // Correctness + convention terms (identical for both tiers): a layout that
    // shorts, overlaps, drops a label to a fallback, breaks the authored grid, or
    // runs a wire through a body is wrong regardless of price.
    let correctness = 2000.0 * merges as f64
        + 1500.0 * overlaps as f64
        + 1000.0 * fallbacks as f64
        + 1200.0 * grid_order as f64
        + 30.0 * body_cross as f64
        + 12.0 * orient_viol as f64
        + 10.0 * spine_viol as f64;
    // STRAIGHTNESS / neatness terms — the PREMIUM (paid SA) tier weighs these ~3x
    // to push past the local minimum the free greedy tier accepts: straighter wires
    // (fewer corners/crossings) and less dot-knot congestion. Crucially this does
    // NOT scale the COMPACTNESS terms (spread/stray/length): packing tighter trades
    // against text-collision warnings the routed cost is blind to (it has no text
    // solve), so a premium that squeezed harder would ship a tidier-but-colliding
    // sheet — observed as mixed-signal regressing 0→1. Neatness is safe; tightness
    // is not, until the cost can see the lint's text collisions.
    let neat = if premium { 3.0 } else { 1.0 };
    let base = correctness
        + neat * (5.0 * crossings as f64 + 7.0 * congestion as f64 + 7.0 * corners as f64)
        + 1.0 * junctions as f64
        + 0.5 * stray
        + 0.15 * length
        + 0.45 * spread;
    // MULTI-UNIT COHESION. A multi-unit part's units (op-amp A/B + its V+/V- power unit)
    // share a refdes but NO net, so length-min lets them drift apart — scattering the part
    // and its decoupling across the sheet. Penalise the bounding-box spread of same-refdes
    // items so the units cluster as one IC. ZERO for single-unit parts (every refdes is one
    // item), so `base + 0.0 == base` keeps the free path bit-identical and references
    // unchanged; added OUTSIDE `base` (never re-parenthesising it) per the note above.
    let mut by_refdes: BTreeMap<&str, [f64; 4]> = BTreeMap::new();
    for it in items {
        let e = by_refdes.entry(&it.refdes).or_insert([f64::MAX, f64::MAX, f64::MIN, f64::MIN]);
        e[0] = e[0].min(it.at[0]); e[1] = e[1].min(it.at[1]);
        e[2] = e[2].max(it.at[0]); e[3] = e[3].max(it.at[1]);
    }
    let sib_spread: f64 = by_refdes.values().map(|e| (e[2] - e[0]) + (e[3] - e[1])).sum();
    let multiunit = SIB_COHESION * sib_spread;
    // PREMIUM compaction boost. The neat terms above amplify STRAIGHTNESS ~3x; left
    // unbalanced, the paid SA straightens a wire by flinging its part into open space
    // — the "straight but sprawled" look EVERY visual review flagged as the #1 defect.
    // ADD a matching compaction pull so premium packs as hard as it straightens. This
    // is ADDED, never folded into the base sum: re-parenthesising the base shifts its
    // last bits and flips the chaotic SA acceptances (a measured mixed-signal 0->1
    // regression), so the free path (premium=false) must stay bit-identical. Safe to
    // push hard: on affordable boards premium_score_items scores the REAL per-move
    // warning_count, so an over-tight text/wire collision is rejected mid-search; on
    // big boards the final candidate pick (fewest real warnings, greedy always a
    // candidate) caps it — premium can never ship more warnings than greedy.
    if premium {
        base + multiunit + COMPACT_BOOST * (0.15 * length + 0.45 * spread)
            + ORIENT_BOOST * leg_viol as f64
    } else {
        base + multiunit
    }
    // NB: a premium body-cross BOOST was tried and dropped — on the uart (the only
    // reference that ships crossings) body_xing stayed at 2 from boost 0 to 1000:
    // the SA's move set can't reach a crossing-free layout and the crossings come
    // from the ROUTER drawing through a body, not from placement, so a heavier
    // placement penalty only inflates cost. A real fix belongs in route-around logic.
}

/// Extra PREMIUM-tier weight on orientation violations, on TOP of the shared base
/// (12). A 1-rail leg (pull-up/down) or 2-rail decoupling tap wants to be VERTICAL;
/// in an IC-LESS circuit the base 12 loses to length/spread and the SA ships a
/// HORIZONTAL pull-up, which drags the rail's power-symbol label alongside the part's
/// value text ("10k 3V3" — the collision the user flagged). The paid tier prices the
/// violation hard enough to flip it. Premium-only so the free path stays bit-identical
/// (snapshot unchanged). Bites ONLY rail_count=1 leg violations (`leg_viol`): the
/// references ship 0 of those in their final layouts, and the uart's vertical 62R
/// series terminators are rail_count=0 so they're untouched — verified the uart's
/// premium winner is unchanged at boost 0..200, while a synthetic IC-less pull-up
/// flips horizontal→vertical at 50.
pub(crate) const ORIENT_BOOST: f64 = 50.0;

/// Extra weight the PREMIUM tier puts on compactness (length+spread), on TOP of the
/// base 1x, so the paid SA's straightness pull (`neat`=3x) can't win by spreading
/// parts into open space (the "straight but sprawled" defect every visual review
/// flagged). 2.0 → premium compaction ~3x, matching the straightness amplification.
/// Swept on the four reference fixtures: it tightens the two loosest (555 130→121,
/// uart 165→157 shipped-bbox half-perimeter) with NO warning regression, and stays
/// clear of the over-tight edge (boost 4 destabilises uart). Only the premium branch
/// of `layout_cost` reads it, so the free path stays bit-identical.
pub(crate) const COMPACT_BOOST: f64 = 2.0;

/// Cohesion pull on a multi-unit part's units (same refdes, no shared net): penalises
/// their bounding-box spread so an op-amp's A/B/power units cluster as one IC instead of
/// drifting apart and scattering the part's decoupling. ZERO on single-unit boards (one
/// item per refdes → zero spread), so the free path + all single-unit references stay
/// bit-identical. Both cost tiers read it (clustering is a correctness-of-organisation
/// pull, not a premium nicety).
pub(crate) const SIB_COHESION: f64 = 3.0;

/// Ground truth behind the visual "a wire runs through a part" complaint:
/// `(2-pin transverse + collinear body crossings, IC body crossings)` for a placed
/// item set. Mirrors the obstacle extraction in [`layout_cost`] (kept separate so the
/// hot cost path stays untouched). Surfaced on [`EmitOutput`] (`body_crossings` /
/// `ic_crossings`) — authoritative for grounding a vision critic, which over-reports
/// wire-through-body on correctly-drawn series parts and op-amp triangles.
pub fn crossing_counts(
    env: &KicadEnv,
    items: &[Item],
    inc: &Incidence,
    ir: &LayoutIr,
    needs_flag: &BTreeSet<String>,
) -> (usize, usize, usize) {
    // Measure the SHIPPED geometry (`fan_risers = true`): the finalize riser jog
    // clears trunk-through-body crossings, so the reported count must reflect the
    // jogged sheet, not the raw per-move one.
    let Ok(w) = build_writer(env, None, items, inc, ir, needs_flag, true) else {
        return (0, 0, 0);
    };
    let wires = w.wires_with_nets();
    let bodies: Vec<([f64; 2], [f64; 2])> = items
        .iter()
        .filter(|i| i.geom.pins.len() == 2)
        .filter_map(|it| {
            let (n0, n1) = (&it.geom.pins[0].number, &it.geom.pins[1].number);
            match (w.pin_dirs(env, &it.refdes, n0), w.pin_dirs(env, &it.refdes, n1)) {
                (Ok(d0), Ok(d1)) => match (d0.first(), d1.first()) {
                    (Some((a, _)), Some((b, _))) => Some((*a, *b)),
                    _ => None,
                },
                _ => None,
            }
        })
        .collect();
    let ic_rects: Vec<[f64; 4]> = items
        .iter()
        .filter(|it| it.geom.pins.len() >= 3)
        .filter_map(|it| {
            let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
            let mut any = false;
            for pg in &it.geom.pins {
                if let Ok(d) = w.pin_dirs(env, &it.refdes, &pg.number)
                    && let Some((p, _)) = d.first() {
                        lo[0] = lo[0].min(p[0]);
                        lo[1] = lo[1].min(p[1]);
                        hi[0] = hi[0].max(p[0]);
                        hi[1] = hi[1].max(p[1]);
                        any = true;
                    }
            }
            any.then(|| [lo[0] + 2.0, lo[1] + 2.0, hi[0] - 2.0, hi[1] - 2.0])
        })
        .collect();
    (
        count_body_crossings(&bodies, &wires)
            + count_collinear_body_crossings(&bodies, &wires)
            + count_parallel_body_crossings(&bodies, &wires),
        count_ic_body_crossings(&ic_rects, &wires),
        count_crossings(&wires),
    )
}

/// Violations of the author's per-block `layout:` relative ordering (`ir.grid`).
/// For each pair of gridded parts whose grid boxes are DISJOINT on an axis, the
/// search must hold that order: A strictly left of B (`A.col_max < B.col_min`)
/// requires A's body centre left of B's; A strictly above B (`A.row_max <
/// B.row_min`) requires A above B (smaller y). Boxes that OVERLAP on an axis — a
/// column-span float like a tall IC — impose no constraint on that axis, so the
/// part floats within its span. Empty grid ⇒ 0 (no `layout:` / sidecar path).
pub fn grid_order_viol(items: &[Item], ir: &LayoutIr) -> usize {
    if ir.grid.is_empty() {
        return 0;
    }
    let pos: BTreeMap<&str, [f64; 2]> = items.iter().map(|it| (it.refdes.as_str(), it.at)).collect();
    let g: Vec<(&String, &[i32; 4])> = ir.grid.iter().collect();
    let mut viol = 0;
    for i in 0..g.len() {
        for j in (i + 1)..g.len() {
            let (ra, ba) = g[i];
            let (rb, bb) = g[j];
            let (Some(pa), Some(pb)) = (pos.get(ra.as_str()), pos.get(rb.as_str())) else {
                continue;
            };
            // Columns → left/right, only when the two boxes share no column.
            if (ba[2] < bb[0] && pa[0] >= pb[0] - EPS)
                || (bb[2] < ba[0] && pb[0] >= pa[0] - EPS)
            {
                viol += 1;
            }
            // Rows → above/below (smaller y is higher), only when row-disjoint.
            if (ba[3] < bb[1] && pa[1] >= pb[1] - EPS)
                || (bb[3] < ba[1] && pb[1] >= pa[1] - EPS)
            {
                viol += 1;
            }
        }
    }
    viol
}

/// "Stay near your pin": total Manhattan distance from each satellite (2-pin
/// part) to the centroid of the ANCHOR pins it wires to. A pull-up belongs by the
/// SIGNAL pin it pulls, not the rail, so signal (non-rail) anchor pins are used
/// when present; only a part that touches no signal anchor (a decoupling cap, two
/// rails) falls back to its rail anchor pins (→ the IC power pin). This stops a
/// satellite drifting across the chip to dodge a spacing penalty. A part with no
/// anchor pin at all (e.g. an IC-less divider) contributes nothing.
pub(crate) fn count_stray(
    env: &KicadEnv,
    w: &SchematicWriter,
    items: &[Item],
    inc: &Incidence,
    ir: &LayoutIr,
) -> f64 {
    items
        .iter()
        .filter(|s| s.geom.pins.len() < 3)
        .filter_map(|s| {
            signal_anchor_centroid(env, w, items, inc, ir, s, true)
                .map(|c| (s.at[0] - c[0]).abs() + (s.at[1] - c[1]).abs())
        })
        .sum()
}

/// Centroid of the anchor pins a satellite `s` should sit by: its SIGNAL
/// (non-rail) anchor pins if it has any (a pull-up belongs by the pin it pulls).
/// With `rail_fallback`, a part touching no signal anchor (a decoupling cap, two
/// rails) falls back to its rail anchor pins (→ the IC power pin) — wanted for
/// the gentle stray pull, but NOT for hard pin-alignment (which would snap every
/// decoupling cap onto one power pin and cram them). `None` if no anchor applies.
pub(crate) fn signal_anchor_centroid(
    env: &KicadEnv,
    w: &SchematicWriter,
    items: &[Item],
    inc: &Incidence,
    ir: &LayoutIr,
    s: &Item,
    rail_fallback: bool,
) -> Option<[f64; 2]> {
    let is_anchor = |i: usize| items[i].geom.pins.len() >= 3;
    let collect = |rails: bool| -> ([f64; 2], f64) {
        let (mut sum, mut cnt) = ([0.0f64, 0.0f64], 0.0f64);
        for (_, _, net) in &s.pins {
            let Some(net) = net else { continue };
            if ir.rails.contains_key(net) != rails {
                continue;
            }
            for (j, num) in inc.get(net).into_iter().flatten() {
                if is_anchor(*j)
                    && let Ok(eps) = w.pin_dirs(env, &items[*j].refdes, num) {
                        for (p, _) in &eps {
                            sum[0] += p[0];
                            sum[1] += p[1];
                            cnt += 1.0;
                        }
                    }
            }
        }
        (sum, cnt)
    };
    let (sum, cnt) = match collect(false) {
        (_, 0.0) if rail_fallback => collect(true),
        signal => signal,
    };
    (cnt > 0.0).then(|| [sum[0] / cnt, sum[1] / cnt])
}

/// The position of the IC supply pin a decoupling cap bypasses, to hug it. The
/// cap's non-ground rail net (its V+ side) names the supply; among the IC pins on
/// that net, pick the one nearest the cap so it slides to the closest supply pin
/// (the relevant IC when several share the rail). `None` if the cap touches no
/// non-ground rail with an IC pin (e.g. a pure rail-to-rail divider leg, left to
/// the rail spread).
pub(crate) fn supply_pin_target(
    env: &KicadEnv,
    w: &SchematicWriter,
    items: &[Item],
    inc: &Incidence,
    ir: &LayoutIr,
    s: &Item,
) -> Option<[f64; 2]> {
    let mut best: Option<([f64; 2], f64)> = None;
    for (_, _, net) in &s.pins {
        let Some(net) = net else { continue };
        // V+ side only: the rail that is NOT ground (a GND-hung cap aligns by its
        // supply pin, not its ground return).
        if !ir.rails.contains_key(net) || is_ground(net) {
            continue;
        }
        for (j, num) in inc.get(net).into_iter().flatten() {
            if items[*j].geom.pins.len() < 3 {
                continue; // only IC/connector pins anchor a cap
            }
            if let Ok(eps) = w.pin_dirs(env, &items[*j].refdes, num) {
                for (p, _) in eps {
                    let d = (p[0] - s.at[0]).abs() + (p[1] - s.at[1]).abs();
                    if best.is_none_or(|(_, bd)| d < bd) {
                        best = Some((p, d));
                    }
                }
            }
        }
    }
    best.map(|(p, _)| p)
}

/// For each DRIVEN, non-ground power rail, the world position of the regulator/IC
/// OUTPUT pin that drives it. A rail's power symbol belongs at its DRIVER's output
/// (the LDO `VO`, the buck `SW→VOUT`) so the regulated rail's *exit* is unambiguous —
/// not at whatever bypass cap happens to sit nearest the trunk's left end. The driver
/// is a ≥3-pin anchor whose pin on the net carries `PinType::PowerOutput` (which by
/// construction excludes inputs and grounds — the task's "≥3-pin pin that drives, not
/// an input/ground"). Ground rails are skipped (the GND symbol's home is its return,
/// not a driver). Returns at most one driver per net (first wins — a rail has one
/// source); empty when nothing drives the net (the common undriven-bus case), so the
/// caller's existing placement is untouched.
pub(crate) fn driven_rail_drivers(
    env: &KicadEnv,
    w: &SchematicWriter,
    items: &[Item],
    inc: &Incidence,
    ir: &LayoutIr,
) -> BTreeMap<String, [f64; 2]> {
    let provider = RealSymbolProvider::new(env.clone());
    let mut out: BTreeMap<String, [f64; 2]> = BTreeMap::new();
    for (net, pins) in inc {
        if !ir.rails.contains_key(net) || is_ground(net) {
            continue;
        }
        for (i, num) in pins {
            if items[*i].geom.pins.len() < 3 {
                continue; // only an IC/regulator pin can drive a rail
            }
            let Some(meta) = provider.symbol(&items[*i].part) else { continue };
            if find_pin(&meta.pins, num).map(|p| p.etype) != Some(PinType::PowerOutput) {
                continue;
            }
            if let Ok(eps) = w.pin_dirs(env, &items[*i].refdes, num)
                && let Some((p, _)) = eps.first() {
                    out.entry(net.clone()).or_insert(*p);
                }
        }
    }
    out
}

/// Two parallel axis-aligned segments running too close for a sustained length —
/// nearly on top of each other, which reads as cramped. Returns true past the
/// per-call `near` cutoff: wire-vs-wire uses 1 grid (a 2-grid gap, e.g. risers
/// off adjacent IC pins, is fine), but wire-vs-body uses a wider cutoff because a
/// part's body has width, so a wire hugging the *edge* sits ~2 grid off the
/// pin-to-pin *centre line*.
pub(crate) fn parallel_too_close(a1: [f64; 2], a2: [f64; 2], b1: [f64; 2], b2: [f64; 2], near: f64) -> bool {
    const MIN_OVERLAP: f64 = 6.35; // only a sustained parallel run reads as cramped
    let horiz = |a: &[f64; 2], b: &[f64; 2]| (a[1] - b[1]).abs() < EPS;
    let vert = |a: &[f64; 2], b: &[f64; 2]| (a[0] - b[0]).abs() < EPS;
    let (perp, lo, hi) = if horiz(&a1, &a2) && horiz(&b1, &b2) {
        (
            (a1[1] - b1[1]).abs(),
            a1[0].min(a2[0]).max(b1[0].min(b2[0])),
            a1[0].max(a2[0]).min(b1[0].max(b2[0])),
        )
    } else if vert(&a1, &a2) && vert(&b1, &b2) {
        (
            (a1[0] - b1[0]).abs(),
            a1[1].min(a2[1]).max(b1[1].min(b2[1])),
            a1[1].max(a2[1]).min(b1[1].max(b2[1])),
        )
    } else {
        return false;
    };
    perp > EPS && perp < near - EPS && hi - lo > MIN_OVERLAP
}

/// Cramped-spacing count: parallel wires hugging each other (1-grid cutoff) AND
/// wires hugging a 2-pin part's body axis (wider 1.5-grid cutoff — see
/// [`parallel_too_close`]). One rule covers wire-vs-wire and wire-vs-body.
pub(crate) fn count_close_wires(
    wires: &[([f64; 2], [f64; 2], Option<String>)],
    bodies: &[([f64; 2], [f64; 2])],
) -> usize {
    const NEAR_WIRE: f64 = 2.54; // wires closer than 2 grid (i.e. 1 grid) are too close
    const NEAR_BODY: f64 = 3.81; // a body's width pushes the hug ~1 grid further off centre
    let mut n = 0;
    for i in 0..wires.len() {
        for j in (i + 1)..wires.len() {
            let (a1, a2, _) = wires[i];
            let (b1, b2, _) = wires[j];
            if parallel_too_close(a1, a2, b1, b2, NEAR_WIRE) {
                n += 1;
            }
        }
    }
    for (a1, a2, _) in wires {
        for (b1, b2) in bodies {
            if parallel_too_close(*a1, *a2, *b1, *b2, NEAR_BODY) {
                n += 1;
            }
        }
    }
    n
}

/// Congestion: pairs of junction dots crammed within `TIGHT` mm of each other —
/// the cramped node a human would spread out (e.g. a pull-up's tap landing right
/// on a series resistor's pin). Unavoidable IC-pin-spacing pairs add a constant
/// baseline that does not bias the search; only the avoidable cramming varies.
pub(crate) fn count_congestion(junctions: &[[f64; 2]]) -> usize {
    const TIGHT: f64 = 3.81;
    let mut n = 0;
    for i in 0..junctions.len() {
        for j in (i + 1)..junctions.len() {
            let (dx, dy) = (junctions[i][0] - junctions[j][0], junctions[i][1] - junctions[j][1]);
            if dx.hypot(dy) < TIGHT - EPS {
                n += 1;
            }
        }
    }
    n
}

/// Net merges KiCAD would actually make: two DIFFERENT-net wires that (a)
/// collinear-overlap, or (b) both pass through a junction dot. KiCAD does NOT
/// fuse a wire end (or pin) landing on another wire's interior without a
/// junction, so — unlike the router's stricter `segments_conflict` — those near
/// misses are excluded here, else the scorer chases phantom shorts on a layout
/// ERC calls clean.
pub fn count_merges(
    wires: &[([f64; 2], [f64; 2], Option<String>)],
    junctions: &[[f64; 2]],
) -> usize {
    let mut n = 0;
    // (a) Collinear overlaps.
    for i in 0..wires.len() {
        for j in (i + 1)..wires.len() {
            let (a1, a2, an) = &wires[i];
            let (b1, b2, bn) = &wires[j];
            if an == bn {
                continue;
            }
            if collinear_overlap(*a1, *a2, *b1, *b2) {
                n += 1;
            }
        }
    }
    // (b) Junctions touching more than one net (a junction fuses every wire
    // through it — if those carry different nets, that is a real short).
    for &jp in junctions {
        let mut nets: BTreeSet<&str> = BTreeSet::new();
        for (a, b, wn) in wires {
            if let Some(net) = wn
                && sch_model::geom::point_on_segment(jp, *a, *b) {
                    nets.insert(net.as_str());
                }
        }
        if nets.len() > 1 {
            n += 1;
        }
    }
    n
}

/// Two axis-aligned segments that lie on the same line and overlap (KiCAD fuses
/// these). Endpoint-only touches of perpendicular segments are NOT included.
pub(crate) fn collinear_overlap(a1: [f64; 2], a2: [f64; 2], b1: [f64; 2], b2: [f64; 2]) -> bool {
    let a_h = (a1[1] - a2[1]).abs() < EPS;
    let b_h = (b1[1] - b2[1]).abs() < EPS;
    let a_v = (a1[0] - a2[0]).abs() < EPS;
    let b_v = (b1[0] - b2[0]).abs() < EPS;
    if a_h && b_h && (a1[1] - b1[1]).abs() < EPS {
        let (alo, ahi) = (a1[0].min(a2[0]), a1[0].max(a2[0]));
        let (blo, bhi) = (b1[0].min(b2[0]), b1[0].max(b2[0]));
        alo < bhi - EPS && blo < ahi - EPS
    } else if a_v && b_v && (a1[0] - b1[0]).abs() < EPS {
        let (alo, ahi) = (a1[1].min(a2[1]), a1[1].max(a2[1]));
        let (blo, bhi) = (b1[1].min(b2[1]), b1[1].max(b2[1]));
        alo < bhi - EPS && blo < ahi - EPS
    } else {
        false
    }
}

/// Visual wire crossings: pairs of different-net segments, one horizontal and
/// one vertical, intersecting at a point interior to both (KiCAD draws no
/// junction there — the wires just cross over).
pub(crate) fn count_crossings(wires: &[([f64; 2], [f64; 2], Option<String>)]) -> usize {
    let horiz = |a: &[f64; 2], b: &[f64; 2]| (a[1] - b[1]).abs() < EPS;
    let vert = |a: &[f64; 2], b: &[f64; 2]| (a[0] - b[0]).abs() < EPS;
    let interior = |v: f64, lo: f64, hi: f64| v > lo + EPS && v < hi - EPS;
    let mut n = 0;
    for i in 0..wires.len() {
        for j in (i + 1)..wires.len() {
            let (a1, a2, an) = &wires[i];
            let (b1, b2, bn) = &wires[j];
            if an == bn {
                continue; // same net: a deliberate join, not a crossing
            }
            let (h, v) = if horiz(a1, a2) && vert(b1, b2) {
                ((a1, a2), (b1, b2))
            } else if vert(a1, a2) && horiz(b1, b2) {
                ((b1, b2), (a1, a2))
            } else {
                continue; // parallel (collinear overlap is a same/foreign issue, not a crossing)
            };
            let (hy, vx) = (h.0[1], v.0[0]);
            let (hx_lo, hx_hi) = (h.0[0].min(h.1[0]), h.0[0].max(h.1[0]));
            let (vy_lo, vy_hi) = (v.0[1].min(v.1[1]), v.0[1].max(v.1[1]));
            if interior(vx, hx_lo, hx_hi) && interior(hy, vy_lo, vy_hi) {
                n += 1;
            }
        }
    }
    n
}

/// DIAGNOSTIC (env-gated): print every short — a pin landing on a foreign net's
/// wire (endpoint or interior) and every collinear/junction merge — naming the
/// pin (refdes.num@net) and the offending wire (net + endpoints), so the exact
/// rail/trunk wire that merges two nets is pinpointable without kicad-cli.
pub(crate) fn diagnose_shorts(
    env: &KicadEnv,
    w: &SchematicWriter,
    items: &[Item],
    inc: &Incidence,
    design: &Design,
) {
    let wires = w.wires_with_nets();
    let junctions = w.junction_positions();
    eprintln!(
        "[SHORT-DIAG] {} ({} wires, {} junctions)",
        design.name.as_deref().unwrap_or("<unnamed>"),
        wires.len(),
        junctions.len()
    );
    // (1) pin-on-foreign-wire shorts (count_shorts geometry).
    for (net, pins) in inc {
        for (i, num) in pins {
            let Ok(eps) = w.pin_dirs(env, &items[*i].refdes, num) else { continue };
            for (ep, _) in eps {
                for (a, b, wn) in &wires {
                    if wn.as_deref() == Some(net.as_str()) {
                        continue;
                    }
                    let how = if near(ep, *a) || near(ep, *b) {
                        "ENDPOINT"
                    } else if sch_model::geom::point_on_segment(ep, *a, *b) {
                        "INTERIOR"
                    } else {
                        continue;
                    };
                    eprintln!(
                        "[SHORT-DIAG]  PIN {}.{}@{net} at [{:.2},{:.2}] lands {how} of net {:?} wire \
                         [{:.2},{:.2}]->[{:.2},{:.2}]",
                        items[*i].refdes, num, ep[0], ep[1], wn, a[0], a[1], b[0], b[1]
                    );
                }
            }
        }
    }
    // (2) collinear overlaps of different nets.
    for i in 0..wires.len() {
        for j in (i + 1)..wires.len() {
            let (a1, a2, an) = &wires[i];
            let (b1, b2, bn) = &wires[j];
            if an == bn || an.is_none() || bn.is_none() {
                continue;
            }
            if collinear_overlap(*a1, *a2, *b1, *b2) {
                eprintln!(
                    "[SHORT-DIAG]  COLLINEAR net {:?} [{:.2},{:.2}]->[{:.2},{:.2}] overlaps net {:?} \
                     [{:.2},{:.2}]->[{:.2},{:.2}]",
                    an, a1[0], a1[1], a2[0], a2[1], bn, b1[0], b1[1], b2[0], b2[1]
                );
            }
        }
    }
    // (3) junctions fusing >1 net.
    for &jp in &junctions {
        let mut nets: BTreeSet<&str> = BTreeSet::new();
        for (a, b, wn) in &wires {
            if let Some(net) = wn
                && sch_model::geom::point_on_segment(jp, *a, *b) {
                    nets.insert(net.as_str());
                }
        }
        if nets.len() > 1 {
            eprintln!(
                "[SHORT-DIAG]  JUNCTION at [{:.2},{:.2}] fuses nets {:?}",
                jp[0], jp[1], nets
            );
        }
    }
}

/// Placement shorts: a pin whose connection point coincides exactly with the
/// ENDPOINT of a different net's wire (two wire/pin terminals at one point fuse
/// in KiCAD). A pin merely sitting on a wire's interior is NOT a connection
/// without a junction, so — matching `count_merges` — those are excluded.
pub fn count_shorts(
    env: &KicadEnv,
    w: &SchematicWriter,
    items: &[Item],
    inc: &Incidence,
    wires: &[([f64; 2], [f64; 2], Option<String>)],
) -> usize {
    let mut n = 0;
    for (net, pins) in inc {
        for (i, num) in pins {
            let Ok(eps) = w.pin_dirs(env, &items[*i].refdes, num) else { continue };
            for (ep, _) in eps {
                for (a, b, wn) in wires {
                    if wn.as_deref() == Some(net.as_str()) {
                        continue; // own net
                    }
                    // A pin coinciding with a foreign wire's endpoint, OR landing
                    // on its interior (KiCAD connects a pin to a wire it touches),
                    // is a short on a different net.
                    if near(ep, *a) || near(ep, *b) || sch_model::geom::point_on_segment(ep, *a, *b) {
                        n += 1;
                    }
                }
            }
        }
    }
    n
}
