//! Text placement: what solved text must avoid, what may move, and the
//! [`TextSolver`] contract a layout engine plugs its own solver into.

use geom::{Dir, Point2, Rect};
use kicad_symbol::geometry::PinGeom;

/// Fixed geometry a movable must not collide with.
pub enum ObKind {
    /// A symbol body, exempted for text OWNED by that refdes (a label on its
    /// own pin endpoint legitimately sits inside its symbol's generous bbox).
    OwnExempt(String),
    /// Never exempted: pin text, wires, fixed labels, no-connects.
    Hard,
}

pub struct Obstacle {
    pub bbox: Rect,
    pub kind: ObKind,
}

/// One piece of movable text with its candidate boxes in preference order.
pub struct Movable {
    /// Owning refdes, matched against [`ObKind::OwnExempt`].
    pub owner: Option<String>,
    /// Candidate bboxes, best-first. Never empty.
    pub candidates: Vec<Rect>,
}

/// Estimated width of rendered text (mm): 1.1 mm/char at the 1.27 font.
pub fn text_width(s: &str) -> f64 {
    s.chars().count() as f64 * 1.1
}

/// Bbox of a net label anchored at `at` reading along `dir`.
///
/// KiCAD renders label text floating 0.4 mm off the anchor on the side away
/// from the wire, so the box is offset by 0.4 mm perpendicular to the reading
/// direction — a label sitting ON its own wire does not collide with it.
pub fn label_box(at: impl Into<Point2>, dir: Dir, width: f64) -> Rect {
    let at = at.into();
    const H: f64 = 1.6; // text height
    const OFF: f64 = 0.4; // standoff from the anchor/wire
    let (x, y) = (at.x, at.y);
    match dir {
        Dir::East => Rect::new(x, y - OFF - H, x + width, y - OFF),
        Dir::West => Rect::new(x - width, y - OFF - H, x, y - OFF),
        Dir::North => Rect::new(x - OFF - H, y - width, x - OFF, y),
        Dir::South => Rect::new(x + OFF, y, x + OFF + H, y + width),
    }
}

/// Bboxes (sheet space) of a pin's rendered NAME and NUMBER text for a placed
/// instance. The name is omitted for unnamed pins (`"~"`) — only the number
/// box is returned then. The name starts just past the pin's body end and
/// extends INTO the body along the pin direction; the number straddles the
/// pin line midpoint.
pub fn pin_text_boxes(
    pin: &PinGeom,
    inst_at: Point2,
    inst_angle: f64,
    inst_mirror: bool,
) -> Vec<Rect> {
    const NAME_OFFSET: f64 = 0.508; // KiCAD default pin-name offset
    let theta = pin.angle.to_radians();
    let u = Point2::new(theta.cos(), theta.sin()); // local: from tip INTO the body
    let p = Point2::new(-u.y, u.x); // perpendicular
    let to_sheet = |local: Point2| {
        let off = local.transform_offset(inst_angle, inst_mirror);
        Point2::new(inst_at.x + off.x, inst_at.y + off.y)
    };
    let mut boxes = Vec::new();
    if pin.name != "~" {
        let w = text_width(&pin.name);
        let start = Point2::new(
            pin.at.x + (pin.length + NAME_OFFSET) * u.x,
            pin.at.y + (pin.length + NAME_OFFSET) * u.y,
        );
        let end = Point2::new(start.x + w * u.x, start.y + w * u.y);
        let c1 = Point2::new(start.x - 0.8 * p.x, start.y - 0.8 * p.y);
        let c2 = Point2::new(end.x + 0.8 * p.x, end.y + 0.8 * p.y);
        boxes.push(Rect::from_points(to_sheet(c1), to_sheet(c2)));
    }
    // Number text straddles the pin line midpoint.
    let mid = Point2::new(
        pin.at.x + 0.5 * pin.length * u.x,
        pin.at.y + 0.5 * pin.length * u.y,
    );
    let c1 = Point2::new(mid.x - 1.1 * u.x - 0.8 * p.x, mid.y - 1.1 * u.y - 0.8 * p.y);
    let c2 = Point2::new(mid.x + 1.1 * u.x + 0.8 * p.x, mid.y + 1.1 * u.y + 0.8 * p.y);
    boxes.push(Rect::from_points(to_sheet(c1), to_sheet(c2)));
    boxes
}

/// Thin obstacle box around a wire segment (inflated 0.13 mm).
pub fn wire_box(a: Point2, b: Point2) -> Rect {
    Rect::from_points(a, b).inflate(0.13)
}


/// One movable's outcome: which candidate box it took, and whether that box was actually
/// free (a `false` fit means every candidate collided and the caller must degrade the
/// text — lint it, or hide it when it is optional).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pick {
    pub candidate: usize,
    pub fits: bool,
}

/// A schematic TEXT solver: the leaf that seats refdes/value fields and net labels.
///
/// ## Contract
/// - **Deterministic and order-respecting.** Movables are solved in the order given
///   (callers pass most-constrained first); the same input always yields the same picks.
/// - **Total.** Exactly one [`Pick`] per movable, always with a valid candidate index —
///   a movable with no free spot falls back to candidate 0 with `fits: false`.
/// - **Pure.** No I/O, no KiCAD environment.
pub trait TextSolver {
    /// Open provenance: the solver's stable name (e.g. `"greedy"`).
    fn name(&self) -> &'static str;

    /// Seat every movable clear of the obstacles and of each other.
    fn solve(&self, obstacles: &[Obstacle], movables: &[Movable]) -> Vec<Pick>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use geom::Dir;

    fn bbox_close(got: Rect, want: Rect) {
        for (g, w) in [
            (got.min_x, want.min_x),
            (got.min_y, want.min_y),
            (got.max_x, want.max_x),
            (got.max_y, want.max_y),
        ] {
            assert!((g - w).abs() < 1e-6, "got {got:?}, want {want:?}");
        }
    }

    #[test]
    fn label_box_per_direction() {
        // East: text extends +x from the anchor, floats 0.4 above (-y):
        // y range [20 - 0.4 - 1.6, 20 - 0.4] = [18.0, 19.6].
        bbox_close(
            label_box(Point2::new(10.0, 20.0), Dir::East, 5.5),
            Rect::new(10.0, 18.0, 15.5, 19.6),
        );
        // West: extends -x.
        bbox_close(
            label_box(Point2::new(10.0, 20.0), Dir::West, 5.5),
            Rect::new(4.5, 18.0, 10.0, 19.6),
        );
        // North: vertical text extending -y, floating 0.4 to the -x side.
        bbox_close(
            label_box(Point2::new(10.0, 20.0), Dir::North, 5.5),
            Rect::new(8.0, 14.5, 9.6, 20.0),
        );
        // South: extending +y, floating to the +x side.
        bbox_close(
            label_box(Point2::new(10.0, 20.0), Dir::South, 5.5),
            Rect::new(10.4, 20.0, 12.0, 25.5),
        );
    }

    #[test]
    fn pin_name_box_extends_into_body() {
        // A west-side IC pin: connection at local (-10.16, 2.54), angle 0
        // (pointing east INTO the body), length 2.54, name "RST" (3 chars).
        // Body end (local) = (-10.16 + 2.54, 2.54) = (-7.62, 2.54).
        // Name spans local x in [-7.112, -3.812] (0.508 offset + 3.3 width),
        // local y in [1.74, 3.34] (±0.8). Sheet (inst at (100,100), angle 0,
        // y-flip): x in [92.888, 96.188], y in [96.66, 98.26].
        let pin = PinGeom {
            number: "4".into(),
            name: "RST".into(),
            at: Point2::new(-10.16, 2.54),
            angle: 0.0,
            length: 2.54,
            unit: 1,
        };
        let boxes = pin_text_boxes(&pin, Point2::new(100.0, 100.0), 0.0, false);
        assert_eq!(boxes.len(), 2, "named pin -> name box + number box");
        bbox_close(boxes[0], Rect::new(92.888, 96.66, 96.188, 98.26));
        // Number box straddles the pin line midpoint, local (-8.89, 2.54):
        // sheet (91.11, 97.46) ± (1.1, 0.8).
        bbox_close(boxes[1], Rect::new(90.01, 96.66, 92.21, 98.26));
    }

    #[test]
    fn unnamed_pin_has_only_number_box() {
        let pin = PinGeom {
            number: "1".into(),
            name: "~".into(),
            at: Point2::new(0.0, 3.81),
            angle: 270.0,
            length: 1.27,
            unit: 1,
        };
        let boxes = pin_text_boxes(&pin, Point2::new(50.0, 50.0), 0.0, false);
        assert_eq!(boxes.len(), 1);
    }
}
