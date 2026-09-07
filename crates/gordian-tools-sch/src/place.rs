//! Where a new or moved part goes.
//!
//! The model says *left of U1* or *somewhere with room for 20×15*; the answer
//! is always a coordinate this module derives from what the sheet already
//! holds. Everything is snapped to KiCAD's 1.27 mm schematic grid, because a
//! pin off the grid is a pin no wire can reach.

use geom::{Point2, Rect};
use sch_doc::{Pose, SchDoc, SymbolInst, body_rect, placed_pins};

/// Straight lead the router reserves before the first bend at a pin.
const WIRE_STUB: f64 = 2.54;

/// The TERRITORY a field claims for placement, which is not the box it draws
/// into: a visible footprint field is a 60-character string 60 mm long, and
/// reserving all of it would push every new part a whole sheet away from its
/// anchor. Only the first inch of a run of text counts; a neighbour
/// overlapping the tail of one is a far smaller sin than the detour.
fn text_rect(text: &str, at: Pose) -> Option<Rect> {
    if text.is_empty() {
        return None;
    }
    let drawn = sch_model::text::drawn_box(
        text,
        sch_model::text::FONT_SIZE,
        sch_model::text::HJust::Center,
        sch_model::text::VJust::Center,
        at.rot,
        at.point(),
    );
    let half = Point2::new(
        (drawn.width() / 2.0).min(6.35),
        (drawn.height() / 2.0).min(6.35),
    );
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

/// The space a placed symbol really claims: its drawn body, printed text, and
/// the straight wire lead leaving each pin.
///
/// A part's pins reach well past its outline — an LED's do by 3.8 mm — and two
/// parts spaced only by their bodies end up with pins in each other's laps,
/// where no wire can be routed between them.
pub(crate) fn extent(doc: &SchDoc, inst: &SymbolInst) -> Option<Rect> {
    let pins: Vec<sch_doc::PlacedPin> = placed_pins(doc)
        .iter()
        .filter(|p| p.owner == inst.uuid)
        .cloned()
        .collect();
    let mut corners: Vec<Point2> = pins
        .iter()
        .flat_map(|pin| {
            [
                pin.at,
                Point2::new(
                    pin.at.x + pin.out.x * WIRE_STUB,
                    pin.at.y + pin.out.y * WIRE_STUB,
                ),
            ]
        })
        .collect();
    let boxes = body_rect(doc, inst).into_iter().chain(field_rects(inst));
    for r in boxes {
        corners.push(Point2::new(r.min_x, r.min_y));
        corners.push(Point2::new(r.max_x, r.max_y));
    }
    Rect::bounding(&corners)
}
