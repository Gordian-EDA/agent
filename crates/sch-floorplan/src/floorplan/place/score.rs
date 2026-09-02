//! `place::score` — the routed-sheet COUNT/geometry primitives an engine measures
//! against: the `count_*` neatness/truthfulness terms (crossings, corners, merges,
//! shorts, congestion, body-crossings), the orientation/spine/stray/grid-order
//! classifiers, and the geometry primitives (`item_rect`, `body_overlap_count`).
//! The MEASUREMENT library [`super::measure`] assembles these
//! into the raw 18 terms; each ENGINE then weights them into its own objective. This
//! module bakes in NO weights and NO `premium` policy — those are engine-owned.

use std::collections::{BTreeMap, BTreeSet};

use geom::{EPS, Point2, Segment};
use kicad::KicadInstallation;
use sch_check::model::Design;

use crate::write::SchematicWriter;
use sch_model::route::DrawnSegment;

use circuit_graph::netclass::is_ground;
use sch_model::item::{Incidence, Item};

// The disjoint-set forest (over a caller-owned `parent` slice) lives in
// `geom::union_find`, shared with the desugar pin reconciler.
use sch_model::ir::LayoutIr;

/// Wires that run straight THROUGH a 2-pin part's body — a foreign (or trunk)
/// segment crossing the pin-to-pin axis at a point strictly interior to it,
/// perpendicular to the part. This reads as "a wire drawn through a resistor" and
/// the existing parallel-proximity check never catches it (it is a crossing, not
/// a hug). A lead leaving a pin is collinear with / starts at the body endpoint,
/// so it is excluded.
pub fn count_body_crossings(bodies: &[([f64; 2], [f64; 2])], wires: &[DrawnSegment]) -> usize {
    let mut n = 0;
    for (a, b) in bodies {
        let bh = (a[1] - b[1]).abs() < EPS; // body axis horizontal?
        if (a[0] - b[0]).abs() < EPS && (a[1] - b[1]).abs() < EPS {
            continue;
        }
        for wire in wires {
            let seg = wire.segment;
            let wh = (seg.a.y - seg.b.y).abs() < EPS;
            if bh == wh {
                continue; // need a perpendicular wire
            }
            let (interior, on_wire) = if bh {
                let p = Point2::new(seg.a.x, a[1]);
                (
                    p.x > a[0].min(b[0]) + EPS && p.x < a[0].max(b[0]) - EPS,
                    seg.contains_point(p),
                )
            } else {
                let p = Point2::new(a[0], seg.a.y);
                (
                    p.y > a[1].min(b[1]) + EPS && p.y < a[1].max(b[1]) - EPS,
                    seg.contains_point(p),
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
    wires: &[DrawnSegment],
) -> usize {
    let mut n = 0;
    for (a, b) in bodies {
        if (a[0] - b[0]).abs() < EPS && (a[1] - b[1]).abs() < EPS {
            continue;
        }
        let bh = (a[1] - b[1]).abs() < EPS; // horizontal part (pins differ in x)?
        let axis = if bh { 0 } else { 1 };
        let (plo, phi) = (a[axis].min(b[axis]), a[axis].max(b[axis]));
        for wire in wires {
            let seg = wire.segment;
            let wh = (seg.a.y - seg.b.y).abs() < EPS;
            if bh != wh {
                continue; // need a PARALLEL wire (the collinear candidate)
            }
            // ...on the SAME line as the body axis (matching perpendicular coord).
            let perp = if bh { 1 } else { 0 };
            if (seg.a[perp] - a[perp]).abs() > EPS {
                continue;
            }
            let (wlo, whi) = (seg.a[axis].min(seg.b[axis]), seg.a[axis].max(seg.b[axis]));
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
pub fn count_ic_body_crossings(ic_rects: &[::geom::Rect], wires: &[DrawnSegment]) -> usize {
    let mut n = 0;
    for r in ic_rects {
        if r.width() < EPS || r.height() < EPS {
            continue;
        }
        for wire in wires {
            if wire.segment.axis_aligned_hits_rect_interior(r) {
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
    wires: &[DrawnSegment],
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
        for wire in wires {
            let seg = wire.segment;
            let wh = (seg.a.y - seg.b.y).abs() < EPS;
            if bh != wh {
                continue; // need a PARALLEL wire (perpendicular is count_body_crossings)
            }
            if (seg.a[perp] - a[perp]).abs() > PLATE_HALF - EPS {
                continue; // outside the drawn body width
            }
            let (wlo, whi) = (seg.a[axis].min(seg.b[axis]), seg.a[axis].max(seg.b[axis]));
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
pub fn count_corners(wires: &[DrawnSegment]) -> usize {
    // (net, point) -> orientations of the segments ending there (true = horizontal).
    let mut at: BTreeMap<(String, u64, u64), Vec<bool>> = BTreeMap::new();
    for wire in wires {
        let Some(net) = &wire.net else { continue };
        let seg = wire.segment;
        let horiz = (seg.a.y - seg.b.y).abs() < EPS;
        for p in [seg.a, seg.b] {
            at.entry((net.clone(), p.x.to_bits(), p.y.to_bits()))
                .or_default()
                .push(horiz);
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
pub fn count_foreign_taps(wires: &[DrawnSegment]) -> usize {
    let strict_interior = |p: Point2, seg: Segment| {
        !p.near_eq(seg.a, EPS) && !p.near_eq(seg.b, EPS) && seg.contains_point(p)
    };
    let mut n = 0;
    for endpoint_wire in wires {
        for through_wire in wires {
            if endpoint_wire.net.as_deref() == through_wire.net.as_deref()
                || endpoint_wire.net.is_none()
                || through_wire.net.is_none()
            {
                continue;
            }
            if strict_interior(endpoint_wire.segment.a, through_wire.segment)
                || strict_interior(endpoint_wire.segment.b, through_wire.segment)
            {
                n += 1;
            }
        }
    }
    n
}

/// "Stay near your pin": total Manhattan distance from each satellite (2-pin
/// part) to the centroid of the ANCHOR pins it wires to. A pull-up belongs by the
/// SIGNAL pin it pulls, not the rail, so signal (non-rail) anchor pins are used
/// when present; only a part that touches no signal anchor (a decoupling cap, two
/// rails) falls back to its rail anchor pins (→ the IC power pin). This stops a
/// satellite drifting across the chip to dodge a spacing penalty. A part with no
/// anchor pin at all (e.g. an IC-less divider) contributes nothing.
pub fn count_stray(
    env: &KicadInstallation,
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
    env: &KicadInstallation,
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
                    && let Ok(eps) = w.pin_dirs(env, &items[*j].refdes, num)
                {
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
    env: &KicadInstallation,
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

/// Two parallel axis-aligned segments running too close for a sustained length —
/// nearly on top of each other, which reads as cramped. Returns true past the
/// per-call `near` cutoff: wire-vs-wire uses 1 grid (a 2-grid gap, e.g. risers
/// off adjacent IC pins, is fine), but wire-vs-body uses a wider cutoff because a
/// part's body has width, so a wire hugging the *edge* sits ~2 grid off the
/// pin-to-pin *centre line*.
pub(crate) fn parallel_too_close(a: Segment, b: Segment, near: f64) -> bool {
    const MIN_OVERLAP: f64 = 6.35; // only a sustained parallel run reads as cramped
    let (a1, a2, b1, b2) = (a.a, a.b, b.a, b.b);
    let horiz = |segment: Segment| (segment.a.y - segment.b.y).abs() < EPS;
    let vert = |segment: Segment| (segment.a.x - segment.b.x).abs() < EPS;
    let (perp, lo, hi) = if horiz(a) && horiz(b) {
        (
            (a1.y - b1.y).abs(),
            a1.x.min(a2.x).max(b1.x.min(b2.x)),
            a1.x.max(a2.x).min(b1.x.max(b2.x)),
        )
    } else if vert(a) && vert(b) {
        (
            (a1.x - b1.x).abs(),
            a1.y.min(a2.y).max(b1.y.min(b2.y)),
            a1.y.max(a2.y).min(b1.y.max(b2.y)),
        )
    } else {
        return false;
    };
    perp > EPS && perp < near - EPS && hi - lo > MIN_OVERLAP
}

/// Cramped-spacing count: parallel wires hugging each other (1-grid cutoff) AND
/// wires hugging a 2-pin part's body axis (wider 1.5-grid cutoff — see
/// [`parallel_too_close`]). One rule covers wire-vs-wire and wire-vs-body.
pub fn count_close_wires(wires: &[DrawnSegment], bodies: &[([f64; 2], [f64; 2])]) -> usize {
    const NEAR_WIRE: f64 = 2.54; // wires closer than 2 grid (i.e. 1 grid) are too close
    const NEAR_BODY: f64 = 3.81; // a body's width pushes the hug ~1 grid further off centre
    let mut n = 0;
    for i in 0..wires.len() {
        for j in (i + 1)..wires.len() {
            if parallel_too_close(wires[i].segment, wires[j].segment, NEAR_WIRE) {
                n += 1;
            }
        }
    }
    for wire in wires {
        for (b1, b2) in bodies {
            if parallel_too_close(
                wire.segment,
                Segment::new((*b1).into(), (*b2).into()),
                NEAR_BODY,
            ) {
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
            if ::geom::Point2::from(junctions[i]).dist(junctions[j].into()) < TIGHT - EPS {
                n += 1;
            }
        }
    }
    n
}

/// Net merges KiCAD would actually make: two DIFFERENT-net wires that (a)
/// collinear-overlap, or (b) both pass through a junction dot. KiCAD does NOT
/// fuse a wire end (or pin) landing on another wire's interior without a
/// junction, so — unlike the router's stricter connecting-touch test — those near
/// misses are excluded here, else the scorer chases phantom shorts on a layout
/// ERC calls clean.
pub fn count_merges(wires: &[DrawnSegment], junctions: &[[f64; 2]]) -> usize {
    let mut n = 0;
    // (a) Collinear overlaps.
    for i in 0..wires.len() {
        for j in (i + 1)..wires.len() {
            let a = &wires[i];
            let b = &wires[j];
            if a.net.as_deref() == b.net.as_deref() {
                continue;
            }
            if a.segment.axis_aligned_collinear_overlap(b.segment) {
                n += 1;
            }
        }
    }
    // (b) Junctions touching more than one net (a junction fuses every wire
    // through it — if those carry different nets, that is a real short).
    for &jp in junctions {
        let mut nets: BTreeSet<&str> = BTreeSet::new();
        for wire in wires {
            if let Some(net) = wire.net.as_deref()
                && wire.segment.contains_point(jp.into())
            {
                nets.insert(net);
            }
        }
        if nets.len() > 1 {
            n += 1;
        }
    }
    n
}

/// Visual wire crossings: pairs of different-net segments, one horizontal and
/// one vertical, intersecting at a point interior to both (KiCAD draws no
/// junction there — the wires just cross over).
pub fn count_crossings(wires: &[DrawnSegment]) -> usize {
    let mut n = 0;
    for i in 0..wires.len() {
        for j in (i + 1)..wires.len() {
            let a = &wires[i];
            let b = &wires[j];
            if a.net.as_deref() == b.net.as_deref() {
                continue; // same net: a deliberate join, not a crossing
            }
            if a.segment.axis_aligned_crosses_interior(b.segment) {
                n += 1;
            }
        }
    }
    n
}

/// DIAGNOSTIC (env-gated): log every short — a pin landing on a foreign net's
/// wire (endpoint or interior) and every collinear/junction merge — naming the
/// pin (refdes.num@net) and the offending wire (net + endpoints), so the exact
/// rail/trunk wire that merges two nets is pinpointable without kicad.
pub(crate) fn diagnose_shorts(
    env: &KicadInstallation,
    w: &SchematicWriter,
    items: &[Item],
    inc: &Incidence,
    design: &Design,
) {
    let wires = w.wires_with_nets();
    let junctions = w.junction_positions();
    tracing::debug!(
        "[SHORT-DIAG] {} ({} wires, {} junctions)",
        design.name.as_deref().unwrap_or("<unnamed>"),
        wires.len(),
        junctions.len()
    );
    // (1) pin-on-foreign-wire shorts (count_shorts geometry).
    for (net, pins) in inc {
        for (i, num) in pins {
            let Ok(eps) = w.pin_dirs(env, &items[*i].refdes, num) else {
                continue;
            };
            for (ep, _) in eps {
                for wire in &wires {
                    if wire.net.as_deref() == Some(net.as_str()) {
                        continue;
                    }
                    let seg = wire.segment;
                    let ep_point = ::geom::Point2::from(ep);
                    let how = if ep_point.near_eq(seg.a, EPS) || ep_point.near_eq(seg.b, EPS) {
                        "ENDPOINT"
                    } else if seg.contains_point(ep.into()) {
                        "INTERIOR"
                    } else {
                        continue;
                    };
                    tracing::debug!(
                        "[SHORT-DIAG]  PIN {}.{}@{net} at [{:.2},{:.2}] lands {how} of net {:?} wire \
                         [{:.2},{:.2}]->[{:.2},{:.2}]",
                        items[*i].refdes,
                        num,
                        ep[0],
                        ep[1],
                        &wire.net,
                        seg.a.x,
                        seg.a.y,
                        seg.b.x,
                        seg.b.y
                    );
                }
            }
        }
    }
    // (2) collinear overlaps of different nets.
    for i in 0..wires.len() {
        for j in (i + 1)..wires.len() {
            let a = &wires[i];
            let b = &wires[j];
            if a.net.as_deref() == b.net.as_deref() || a.net.is_none() || b.net.is_none() {
                continue;
            }
            if a.segment.axis_aligned_collinear_overlap(b.segment) {
                tracing::debug!(
                    "[SHORT-DIAG]  COLLINEAR net {:?} [{:.2},{:.2}]->[{:.2},{:.2}] overlaps net {:?} \
                     [{:.2},{:.2}]->[{:.2},{:.2}]",
                    &a.net,
                    a.segment.a.x,
                    a.segment.a.y,
                    a.segment.b.x,
                    a.segment.b.y,
                    &b.net,
                    b.segment.a.x,
                    b.segment.a.y,
                    b.segment.b.x,
                    b.segment.b.y
                );
            }
        }
    }
    // (3) junctions fusing >1 net.
    for &jp in &junctions {
        let mut nets: BTreeSet<&str> = BTreeSet::new();
        for wire in &wires {
            if let Some(net) = wire.net.as_deref()
                && wire.segment.contains_point(jp.into())
            {
                nets.insert(net);
            }
        }
        if nets.len() > 1 {
            tracing::debug!(
                "[SHORT-DIAG]  JUNCTION at [{:.2},{:.2}] fuses nets {:?}",
                jp[0],
                jp[1],
                nets
            );
        }
    }
}

/// Placement shorts: a pin whose connection point coincides exactly with the
/// ENDPOINT of a different net's wire (two wire/pin terminals at one point fuse
/// in KiCAD). A pin merely sitting on a wire's interior is NOT a connection
/// without a junction, so — matching `count_merges` — those are excluded.
pub fn count_shorts(
    env: &KicadInstallation,
    w: &SchematicWriter,
    items: &[Item],
    inc: &Incidence,
    wires: &[DrawnSegment],
) -> usize {
    let mut n = 0;
    for (net, pins) in inc {
        for (i, num) in pins {
            let Ok(eps) = w.pin_dirs(env, &items[*i].refdes, num) else {
                continue;
            };
            for (ep, _) in eps {
                for wire in wires {
                    if wire.net.as_deref() == Some(net.as_str()) {
                        continue; // own net
                    }
                    // A pin coinciding with a foreign wire's endpoint, OR landing
                    // on its interior (KiCAD connects a pin to a wire it touches),
                    // is a short on a different net.
                    let ep_point = ::geom::Point2::from(ep);
                    if ep_point.near_eq(wire.segment.a, EPS)
                        || ep_point.near_eq(wire.segment.b, EPS)
                        || wire.segment.contains_point(ep.into())
                    {
                        n += 1;
                    }
                }
            }
        }
    }
    n
}
