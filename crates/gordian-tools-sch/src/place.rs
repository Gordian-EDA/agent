//! Where a new or moved part goes.
//!
//! The model says *left of U1* or *somewhere with room for 20×15*; the answer
//! is always a coordinate this module derives from what the sheet already
//! holds. Everything is snapped to KiCAD's 1.27 mm schematic grid, because a
//! pin off the grid is a pin no wire can reach.

use geom::{Point2, Rect};
use sch_doc::{Pose, SchDoc, SymbolInst, body_rect, placed_pins};

/// The schematic grid: 50 mil.
const GRID: f64 = 1.27;

/// Air kept between a placed body and its neighbours.
pub(crate) const CLEARANCE: f64 = 2.54;

pub(crate) fn snap(value: f64) -> f64 {
    (value / GRID).round() * GRID
}

pub(crate) fn snap_point(p: Point2) -> Point2 {
    Point2::new(snap(p.x), snap(p.y))
}

/// Which way a part sits from its anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Side {
    Left,
    Right,
    Above,
    Below,
}

impl Side {
    pub fn parse(token: &str) -> Option<Side> {
        match token {
            "left" => Some(Side::Left),
            "right" => Some(Side::Right),
            "above" => Some(Side::Above),
            "below" => Some(Side::Below),
            _ => None,
        }
    }

    fn step(self) -> Point2 {
        match self {
            Side::Left => Point2::new(-GRID, 0.0),
            Side::Right => Point2::new(GRID, 0.0),
            Side::Above => Point2::new(0.0, -GRID),
            Side::Below => Point2::new(0.0, GRID),
        }
    }

    /// The unit vector pointing from a part on this side back at its anchor.
    pub fn toward_anchor(self) -> Point2 {
        match self {
            Side::Left => Point2::new(1.0, 0.0),
            Side::Right => Point2::new(-1.0, 0.0),
            Side::Above => Point2::new(0.0, 1.0),
            Side::Below => Point2::new(0.0, -1.0),
        }
    }
}

/// KiCAD's default field text: 1.27 mm tall, roughly as wide per character.
///
/// A visible footprint field is a 60-character string 60 mm long. Reserving
/// all of it would push every new part a whole sheet away from its anchor, so
/// only the first inch of a run of text counts as territory; a neighbour
/// overlapping the tail of one is a far smaller sin than the detour.
fn text_rect(text: &str, at: Pose) -> Option<Rect> {
    let len = text.chars().count();
    if len == 0 {
        return None;
    }
    let half = Point2::new((0.55 * len as f64).min(6.35), 0.8).rotated_half_extents(at.rot);
    Some(Rect::from_center_half(at.point(), (half.x, half.y)))
}

/// The boxes a symbol's *visible* properties print into.
///
/// Reference and value sit right against the body, and a sheet that shows its
/// footprint fields prints a 60-character string down the side of every part.
/// A placement that ignores them reads as a collision even when no two bodies
/// touch.
fn field_rects(inst: &SymbolInst) -> Vec<Rect> {
    inst.fields
        .values()
        .filter(|f| !f.hidden)
        .filter_map(|f| text_rect(&f.value, f.at?))
        .collect()
}

/// The space a placed symbol really claims: its drawn body, its pin tips, and
/// the text it prints.
///
/// A part's pins reach well past its outline — an LED's do by 3.8 mm — and two
/// parts spaced only by their bodies end up with pins in each other's laps,
/// where no wire can be routed between them.
pub(crate) fn extent(doc: &SchDoc, inst: &SymbolInst) -> Option<Rect> {
    let mut corners: Vec<Point2> = placed_pins(doc)
        .iter()
        .filter(|p| p.owner == inst.uuid)
        .map(|p| p.at)
        .collect();
    let boxes = body_rect(doc, inst).into_iter().chain(field_rects(inst));
    for r in boxes {
        corners.push(Point2::new(r.min_x, r.min_y));
        corners.push(Point2::new(r.max_x, r.max_y));
    }
    Rect::bounding(&corners)
}

/// The rotation that points a two-pin part's entry pin back at its anchor.
///
/// A part dropped beside something should lie along the line to it and be
/// entered from that side — a series resistor left of a connector is
/// horizontal with pin 1 facing the connector, not vertical beside it, and a
/// diode below its supply points its anode up so current runs on through it.
/// Parts with more pins have no such axis, so they keep the library's drawing.
pub(crate) fn facing_rotation(doc: &mut SchDoc, refdes: &str, side: Side) -> f64 {
    // Anode first for a polarised part, pin 1 otherwise: current enters there.
    let entry = |pin: &sch_doc::PlacedPin| matches!(pin.name.as_str(), "A" | "+");
    let pin_span = |doc: &SchDoc| {
        let mut pins: Vec<sch_doc::PlacedPin> = placed_pins(doc)
            .into_iter()
            .filter(|p| p.refdes == refdes)
            .collect();
        if pins.iter().any(entry) {
            pins.sort_by_key(|p| !entry(p));
        }
        match pins.as_slice() {
            [a, b] => Some((a.at, b.at)),
            _ => None,
        }
    };
    if pin_span(doc).is_none() {
        return doc.symbol_by_ref(refdes).map_or(0.0, |s| s.at.rot);
    }
    let want = side.toward_anchor();
    let mut best = (f64::MIN, 0.0);
    for rot in [0.0, 90.0, 180.0, 270.0] {
        if doc
            .set_symbol_orientation(refdes, rot, sch_doc::Mirror::None)
            .is_err()
        {
            continue;
        }
        let Some((first, second)) = pin_span(doc) else {
            continue;
        };
        let score = (first.x - second.x) * want.x + (first.y - second.y) * want.y;
        if score > best.0 {
            best = (score, rot);
        }
    }
    let _ = doc.set_symbol_orientation(refdes, best.1, sch_doc::Mirror::None);
    best.1
}

fn extents(doc: &SchDoc) -> Vec<(String, Rect)> {
    doc.symbols()
        .filter_map(|s| Some((s.uuid.clone(), extent(doc, s)?)))
        .collect()
}

/// Everything a new body must stay clear of: the drawn symbols and the wires.
pub(crate) struct Occupancy {
    blocks: Vec<Rect>,
    content: Rect,
}

impl Occupancy {
    /// Everything drawn except `skip`'s bodies *and the wires attached to
    /// them* — what a move or a placement of those parts sees. A part's own
    /// copper follows it, so treating it as an obstacle would forbid every
    /// nudge.
    pub fn skipping(doc: &SchDoc, skip: &[String]) -> Occupancy {
        let mut blocks: Vec<Rect> = extents(doc)
            .into_iter()
            .filter(|(uuid, _)| !skip.contains(uuid))
            .map(|(_, r)| r)
            .collect();
        let own: Vec<Point2> = placed_pins(doc)
            .into_iter()
            .filter(|p| skip.contains(&p.owner))
            .map(|p| p.at)
            .collect();
        let attached = |a: Point2, b: Point2| {
            own.iter()
                .any(|p| p.near_eq(a, geom::EPS) || p.near_eq(b, geom::EPS))
        };
        for wire in doc.wires() {
            for pair in wire.points.windows(2) {
                if !attached(pair[0], pair[1]) {
                    blocks.push(Rect::from_points(pair[0], pair[1]).inflate(0.2));
                }
            }
        }
        for label in doc.labels() {
            blocks.push(Rect::from_center_half(label.at.point(), (1.0, 1.0)));
        }
        let content = Rect::bounding(
            &blocks
                .iter()
                .flat_map(|r| [Point2::new(r.min_x, r.min_y), Point2::new(r.max_x, r.max_y)])
                .collect::<Vec<_>>(),
        )
        .unwrap_or_else(|| Rect::new(25.4, 25.4, 254.0, 177.8));
        Occupancy { blocks, content }
    }

    pub fn content(&self) -> Rect {
        self.content
    }

    /// Whether a `w`×`h` body centred at `at` clears everything already drawn.
    pub fn free(&self, at: Point2, w: f64, h: f64) -> bool {
        let want = Rect::from_center_half(at, (w / 2.0 + CLEARANCE, h / 2.0 + CLEARANCE));
        !self.blocks.iter().any(|b| b.overlaps(&want))
    }

    /// The nearest free centre for a `w`×`h` body, searching outward from
    /// `from` in a grid spiral. `None` when the sheet is that full.
    pub fn nearest_free(&self, from: Point2, w: f64, h: f64) -> Option<Point2> {
        self.nearest_free_within(from, w, h, 160)
    }

    /// The same, giving up after `rings` grid steps — how far a *nudge* is
    /// still the move that was asked for rather than a different one.
    pub fn nearest_free_within(&self, from: Point2, w: f64, h: f64, rings: i32) -> Option<Point2> {
        let start = snap_point(from);
        if self.free(start, w, h) {
            return Some(start);
        }
        for ring in 1..rings {
            let span = ring as f64 * GRID;
            let mut best: Option<(f64, Point2)> = None;
            for step in -ring..=ring {
                let offset = step as f64 * GRID;
                for candidate in [
                    Point2::new(start.x + offset, start.y - span),
                    Point2::new(start.x + offset, start.y + span),
                    Point2::new(start.x - span, start.y + offset),
                    Point2::new(start.x + span, start.y + offset),
                ] {
                    if candidate.x < GRID || candidate.y < GRID || !self.free(candidate, w, h) {
                        continue;
                    }
                    let d = candidate.manhattan(from);
                    if best.is_none_or(|(held, _)| d < held) {
                        best = Some((d, candidate));
                    }
                }
            }
            if let Some((_, at)) = best {
                return Some(at);
            }
        }
        None
    }

    /// The first free centre for a `w`×`h` body on one side of `anchor`, and
    /// whether it really landed on that side.
    pub fn beside(&self, anchor: Rect, side: Side, w: f64, h: f64) -> Option<(Point2, bool)> {
        let centre = anchor.center();
        let (half_w, half_h) = (w / 2.0, h / 2.0);
        let start = match side {
            Side::Left => Point2::new(anchor.min_x - CLEARANCE - half_w, centre.y),
            Side::Right => Point2::new(anchor.max_x + CLEARANCE + half_w, centre.y),
            Side::Above => Point2::new(centre.x, anchor.min_y - CLEARANCE - half_h),
            Side::Below => Point2::new(centre.x, anchor.max_y + CLEARANCE + half_h),
        };
        // Slide along the side only as far as still reads as "beside": past
        // that, a spot in some other direction but *close* is the better
        // answer than one on the right side of the sheet and 60 mm away.
        let step = side.step();
        let mut at = snap_point(start);
        for _ in 0..12 {
            if at.x >= GRID && at.y >= GRID && self.free(at, w, h) {
                return Some((at, true));
            }
            at = Point2::new(at.x + step.x, at.y + step.y);
        }
        self.nearest_free(snap_point(start), w, h)
            .map(|at| (at, false))
    }
}
