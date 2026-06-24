//! `place::score` — the routed-sheet COUNT/geometry primitives an engine measures
//! against: the `count_*` neatness/truthfulness terms (crossings, corners, merges,
//! shorts, congestion, body-crossings), the orientation/spine/stray/grid-order
//! classifiers, and the geometry primitives (`item_rect`, `rects_overlap`,
//! `body_overlap_count`). The MEASUREMENT library [`super::measure`] assembles these
//! into the raw 16 terms; each ENGINE then weights them into its own objective. This
//! module bakes in NO weights and NO `premium` policy — those are engine-owned.

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
pub fn count_body_crossings(
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
pub fn count_collinear_body_crossings(
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
pub fn count_ic_body_crossings(
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
pub fn count_parallel_body_crossings(
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
pub fn count_corners(wires: &[([f64; 2], [f64; 2], Option<String>)]) -> usize {
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
pub fn count_stray(
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
pub fn signal_anchor_centroid(
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
pub fn supply_pin_target(
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
pub fn count_close_wires(
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
pub fn count_congestion(junctions: &[[f64; 2]]) -> usize {
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
pub fn count_crossings(wires: &[([f64; 2], [f64; 2], Option<String>)]) -> usize {
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
