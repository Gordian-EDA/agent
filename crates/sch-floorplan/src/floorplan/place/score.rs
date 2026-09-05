//! `place::score` — the `count_*` truthfulness terms [`super::measure`] reads off a
//! routed sheet: wire-through-body crossings, foreign taps, net merges, visual wire
//! crossings, and placement shorts.

use std::collections::BTreeSet;

use geom::{EPS, Point2, Segment};
use kicad::KicadInstallation;

use crate::write::SchematicWriter;
use sch_model::route::DrawnSegment;

use sch_model::item::{Incidence, Item};

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
    let mut n = 0;
    for (a, b) in bodies {
        let Some(plate) = two_pin_plate(*a, *b) else {
            continue;
        };
        for wire in wires {
            if plate.contains_axis_run(wire.segment) {
                n += 1;
            }
        }
    }
    n
}

/// Half the drawn width of a 2-pin symbol body — wide enough that a wire one 1.27 mm
/// grid step off the part's axis still counts as slicing the drawn plate.
const PLATE_HALF: f64 = 1.4;
/// The pin stub at each end of a 2-pin part, excluded from its drawn plate: the part's
/// own leads legitimately attach there.
const PIN_STUB: f64 = 2.54;

/// The drawn PLATE of a 2-pin part whose pins sit at `a` and `b`: the central span
/// between the pins, past both stubs, [`PLATE_HALF`] wide about the pin axis.
///
/// This is the shape a wire drawn *along* a series part slices through, which neither
/// the symbol's own graphics box nor the pin-tip bbox states: a switch's or a crystal's
/// graphics sit off the pin axis, so a wire running down the axis clears every box the
/// library draws and still reads as a wire through the part. It is stated once here and
/// used twice — to COUNT the defect, and (via [`sch_model::route::SymbolInk`]) to stop
/// the router drawing it.
pub fn two_pin_plate(a: [f64; 2], b: [f64; 2]) -> Option<Plate> {
    let horizontal = (a[1] - b[1]).abs() < EPS;
    if (a[0] - b[0]).abs() >= EPS && !horizontal {
        return None; // a diagonal pair is no axis to run along
    }
    let (axis, perp) = if horizontal { (0, 1) } else { (1, 0) };
    let (plo, phi) = (a[axis].min(b[axis]), a[axis].max(b[axis]));
    let (lo, hi) = (plo + PIN_STUB, phi - PIN_STUB);
    (hi > lo + EPS).then_some(Plate {
        horizontal,
        axis_at: a[perp],
        lo,
        hi,
    })
}

/// The central plate of a 2-pin part — see [`two_pin_plate`].
#[derive(Debug, Clone, Copy)]
pub struct Plate {
    /// Whether the part's pin axis runs in x.
    pub horizontal: bool,
    /// The plate's coordinate on the axis PERPENDICULAR to the pin axis.
    pub axis_at: f64,
    /// The plate's span along the pin axis, past both pin stubs.
    pub lo: f64,
    pub hi: f64,
}

impl Plate {
    /// Whether `seg` runs along this plate — parallel to the pin axis, within the drawn
    /// plate's width, overlapping its span.
    pub fn contains_axis_run(&self, seg: ::geom::Segment) -> bool {
        let (axis, perp) = if self.horizontal { (0, 1) } else { (1, 0) };
        if ((seg.a.y - seg.b.y).abs() < EPS) != self.horizontal {
            return false;
        }
        if (seg.a[perp] - self.axis_at).abs() > PLATE_HALF - EPS {
            return false;
        }
        let (wlo, whi) = (seg.a[axis].min(seg.b[axis]), seg.a[axis].max(seg.b[axis]));
        wlo < self.hi - EPS && whi > self.lo + EPS
    }

    /// The plate as a rectangle, for an obstacle model that speaks in boxes.
    pub fn rect(&self) -> ::geom::Rect {
        match self.horizontal {
            true => ::geom::Rect::new(
                self.lo,
                self.axis_at - PLATE_HALF,
                self.hi,
                self.axis_at + PLATE_HALF,
            ),
            false => ::geom::Rect::new(
                self.axis_at - PLATE_HALF,
                self.lo,
                self.axis_at + PLATE_HALF,
                self.hi,
            ),
        }
    }
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
