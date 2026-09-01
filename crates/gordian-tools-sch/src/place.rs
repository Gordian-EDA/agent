//! Where a new or moved part goes.
//!
//! The model says *left of U1* or *somewhere with room for 20×15*; the answer
//! is always a coordinate this module derives from what the sheet already
//! holds. Everything is snapped to KiCAD's 1.27 mm schematic grid, because a
//! pin off the grid is a pin no wire can reach.

use geom::{Point2, Rect};
use sch_doc::{SchDoc, SymbolInst, body_rect, placed_pins};

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
}

/// The space a placed symbol really claims: its drawn body *plus* its pin tips.
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
    if let Some(body) = body_rect(doc, inst) {
        corners.push(Point2::new(body.min_x, body.min_y));
        corners.push(Point2::new(body.max_x, body.max_y));
    }
    Rect::bounding(&corners)
}

fn extents(doc: &SchDoc) -> Vec<(String, Rect)> {
    doc.symbols()
        .filter_map(|s| Some((s.refdes().to_string(), extent(doc, s)?)))
        .collect()
}

/// Everything a new body must stay clear of: the drawn symbols and the wires.
pub(crate) struct Occupancy {
    blocks: Vec<Rect>,
    content: Rect,
}

impl Occupancy {
    pub fn of(doc: &SchDoc) -> Occupancy {
        Occupancy::skipping(doc, &[])
    }

    /// The same, minus the bodies of `skip` — what a move of those parts sees.
    pub fn skipping(doc: &SchDoc, skip: &[String]) -> Occupancy {
        let mut blocks: Vec<Rect> = extents(doc)
            .into_iter()
            .filter(|(refdes, _)| !skip.contains(refdes))
            .map(|(_, r)| r)
            .collect();
        for wire in doc.wires() {
            for pair in wire.points.windows(2) {
                blocks.push(Rect::from_points(pair[0], pair[1]).inflate(0.2));
            }
        }
        for label in doc.labels() {
            blocks.push(Rect::from_center_half(label.at.point(), (1.0, 1.0)));
        }
        let content = Rect::bounding(
            &blocks
                .iter()
                .flat_map(|r| {
                    [
                        Point2::new(r.min_x, r.min_y),
                        Point2::new(r.max_x, r.max_y),
                    ]
                })
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
        let start = snap_point(from);
        if self.free(start, w, h) {
            return Some(start);
        }
        for ring in 1..160 {
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

    /// The first free centre for a `w`×`h` body on one side of `anchor`.
    pub fn beside(&self, anchor: Rect, side: Side, w: f64, h: f64) -> Option<Point2> {
        let centre = anchor.center();
        let (half_w, half_h) = (w / 2.0, h / 2.0);
        let start = match side {
            Side::Left => Point2::new(anchor.min_x - CLEARANCE - half_w, centre.y),
            Side::Right => Point2::new(anchor.max_x + CLEARANCE + half_w, centre.y),
            Side::Above => Point2::new(centre.x, anchor.min_y - CLEARANCE - half_h),
            Side::Below => Point2::new(centre.x, anchor.max_y + CLEARANCE + half_h),
        };
        let step = side.step();
        let mut at = snap_point(start);
        for _ in 0..120 {
            if at.x >= GRID && at.y >= GRID && self.free(at, w, h) {
                return Some(at);
            }
            at = Point2::new(at.x + step.x, at.y + step.y);
        }
        self.nearest_free(snap_point(start), w, h)
    }
}
