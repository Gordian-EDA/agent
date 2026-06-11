//! Text placement solver: choose collision-free positions for movable text.
//!
//! Pure geometry — no I/O, no KiCAD environment — so the core is unit-testable
//! and deterministic. `emit.rs` builds [`Obstacle`]s and [`Movable`]s from
//! writer state, calls [`choose`], and applies the returned candidate indices.
//!
//! The solver is greedy: movables are processed in the order given (callers
//! pass a deterministic order — most-constrained first), each takes its first
//! candidate that collides with nothing, and the chosen box becomes an
//! obstacle for everything after it. Greedy is enough here because candidate
//! lists are short and ordered by convention (the first candidate is the
//! KiCAD-conventional spot); a global optimizer would buy little and cost
//! determinism scrutiny.

/// An axis-aligned bbox: `[min_x, min_y, max_x, max_y]` (sheet mm, y down).
pub(crate) type BBox = [f64; 4];

/// Whether two boxes overlap (open intervals: edge-touching is NOT overlap,
/// matching the lint's `boxes_overlap` so solver and oracle agree).
pub(crate) fn boxes_overlap(a: &BBox, b: &BBox) -> bool {
    a[0] < b[2] && b[0] < a[2] && a[1] < b[3] && b[1] < a[3]
}

/// Fixed geometry a movable must not collide with.
pub(crate) enum ObKind {
    /// A symbol body, exempted for text OWNED by that refdes (a label on its
    /// own pin endpoint legitimately sits inside its symbol's generous bbox).
    OwnExempt(String),
    /// Never exempted: pin text, wires, fixed labels, no-connects.
    Hard,
}

pub(crate) struct Obstacle {
    pub bbox: BBox,
    pub kind: ObKind,
}

/// One piece of movable text with its candidate boxes in preference order.
pub(crate) struct Movable {
    /// Owning refdes, matched against [`ObKind::OwnExempt`].
    pub owner: Option<String>,
    /// Candidate bboxes, best-first. Never empty.
    pub candidates: Vec<BBox>,
}

/// For each movable (in order), the index of the first candidate that collides
/// with no obstacle (minus own-body exemptions) and no previously chosen box.
/// Falls back to candidate 0 when none is free (caller's lint then flags it —
/// visible degradation, per spec).
pub(crate) fn choose(obstacles: &[Obstacle], movables: &[Movable]) -> Vec<usize> {
    let mut placed: Vec<BBox> = Vec::new();
    let mut out = Vec::with_capacity(movables.len());
    for m in movables {
        let free = |b: &BBox| {
            obstacles.iter().all(|o| match &o.kind {
                ObKind::OwnExempt(r) if Some(r) == m.owner.as_ref() => true,
                _ => !boxes_overlap(b, &o.bbox),
            }) && placed.iter().all(|p| !boxes_overlap(b, p))
        };
        let idx = m.candidates.iter().position(|c| free(c)).unwrap_or(0);
        placed.push(m.candidates[idx]);
        out.push(idx);
    }
    out
}

use kicad_bridge::geometry::PinGeom;

use crate::emit::{transform_offset, Dir};

/// Estimated width of rendered text (mm): 1.1 mm/char at the 1.27 font.
pub(crate) fn text_width(s: &str) -> f64 {
    s.chars().count() as f64 * 1.1
}

/// Bbox of a net label anchored at `at` reading along `dir`.
///
/// KiCAD renders label text floating 0.4 mm off the anchor on the side away
/// from the wire, so the box is offset by 0.4 mm perpendicular to the reading
/// direction — a label sitting ON its own wire does not collide with it.
pub(crate) fn label_box(at: [f64; 2], dir: Dir, width: f64) -> BBox {
    const H: f64 = 1.6; // text height
    const OFF: f64 = 0.4; // standoff from the anchor/wire
    let [x, y] = at;
    match dir {
        Dir::East => [x, y - OFF - H, x + width, y - OFF],
        Dir::West => [x - width, y - OFF - H, x, y - OFF],
        Dir::North => [x - OFF - H, y - width, x - OFF, y],
        Dir::South => [x + OFF, y, x + OFF + H, y + width],
    }
}

/// Bboxes (sheet space) of a pin's rendered NAME and NUMBER text for a placed
/// instance. The name is omitted for unnamed pins (`"~"`) — only the number
/// box is returned then. The name starts just past the pin's body end and
/// extends INTO the body along the pin direction; the number straddles the
/// pin line midpoint.
pub(crate) fn pin_text_boxes(
    pin: &PinGeom,
    inst_at: [f64; 2],
    inst_angle: f64,
    inst_mirror: bool,
) -> Vec<BBox> {
    const NAME_OFFSET: f64 = 0.508; // KiCAD default pin-name offset
    let theta = pin.angle.to_radians();
    let u = [theta.cos(), theta.sin()]; // local: from tip INTO the body
    let p = [-u[1], u[0]]; // perpendicular
    let to_sheet = |local: [f64; 2]| {
        let off = transform_offset(local, inst_angle, inst_mirror);
        [inst_at[0] + off[0], inst_at[1] + off[1]]
    };
    let mut boxes = Vec::new();
    if pin.name != "~" {
        let w = text_width(&pin.name);
        let start = [
            pin.at[0] + (pin.length + NAME_OFFSET) * u[0],
            pin.at[1] + (pin.length + NAME_OFFSET) * u[1],
        ];
        let end = [start[0] + w * u[0], start[1] + w * u[1]];
        let c1 = [start[0] - 0.8 * p[0], start[1] - 0.8 * p[1]];
        let c2 = [end[0] + 0.8 * p[0], end[1] + 0.8 * p[1]];
        boxes.push(rect_from_corners(to_sheet(c1), to_sheet(c2)));
    }
    // Number text straddles the pin line midpoint.
    let mid = [
        pin.at[0] + 0.5 * pin.length * u[0],
        pin.at[1] + 0.5 * pin.length * u[1],
    ];
    let c1 = [mid[0] - 1.1 * u[0] - 0.8 * p[0], mid[1] - 1.1 * u[1] - 0.8 * p[1]];
    let c2 = [mid[0] + 1.1 * u[0] + 0.8 * p[0], mid[1] + 1.1 * u[1] + 0.8 * p[1]];
    boxes.push(rect_from_corners(to_sheet(c1), to_sheet(c2)));
    boxes
}

/// Sheet bbox from two transformed corner points (normalizes min/max).
pub(crate) fn rect_from_corners(a: [f64; 2], b: [f64; 2]) -> BBox {
    [a[0].min(b[0]), a[1].min(b[1]), a[0].max(b[0]), a[1].max(b[1])]
}

/// Instance half-extents with the body rotation applied: 90/270 swaps w/h.
pub(crate) fn rotated_half_extents(h: [f64; 2], angle: f64) -> [f64; 2] {
    let a = angle.rem_euclid(360.0);
    if (a - 90.0).abs() < 1e-9 || (a - 270.0).abs() < 1e-9 {
        [h[1], h[0]]
    } else {
        h
    }
}

/// Thin obstacle box around a wire segment (inflated 0.13 mm).
pub(crate) fn wire_box(a: [f64; 2], b: [f64; 2]) -> BBox {
    [
        a[0].min(b[0]) - 0.13,
        a[1].min(b[1]) - 0.13,
        a[0].max(b[0]) + 0.13,
        a[1].max(b[1]) + 0.13,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hard(b: BBox) -> Obstacle {
        Obstacle { bbox: b, kind: ObKind::Hard }
    }

    #[test]
    fn picks_first_free_candidate() {
        let obstacles = vec![hard([0.0, 0.0, 10.0, 10.0])];
        let m = Movable {
            owner: None,
            candidates: vec![[5.0, 5.0, 8.0, 8.0], [12.0, 0.0, 15.0, 3.0]],
        };
        assert_eq!(choose(&obstacles, &[m]), vec![1]);
    }

    #[test]
    fn falls_back_to_candidate_zero_when_all_collide() {
        let obstacles = vec![hard([0.0, 0.0, 20.0, 20.0])];
        let m = Movable {
            owner: None,
            candidates: vec![[1.0, 1.0, 2.0, 2.0], [3.0, 3.0, 4.0, 4.0]],
        };
        assert_eq!(choose(&obstacles, &[m]), vec![0]);
    }

    #[test]
    fn own_body_is_exempt_but_hard_is_not() {
        let obstacles = vec![
            Obstacle { bbox: [0.0, 0.0, 10.0, 10.0], kind: ObKind::OwnExempt("R1".into()) },
            hard([0.0, 0.0, 4.0, 4.0]),
        ];
        // Candidate 0 overlaps both; only the body is exempt for R1, so the
        // hard obstacle still rejects it. Candidate 1 overlaps the body only
        // -> exempt -> chosen.
        let m = Movable {
            owner: Some("R1".into()),
            candidates: vec![[1.0, 1.0, 3.0, 3.0], [5.0, 5.0, 9.0, 9.0]],
        };
        assert_eq!(choose(&obstacles, &[m]), vec![1]);
        // A different owner gets no exemption anywhere -> all collide -> 0.
        let m2 = Movable {
            owner: Some("R2".into()),
            candidates: vec![[1.0, 1.0, 3.0, 3.0], [5.0, 5.0, 9.0, 9.0]],
        };
        assert_eq!(choose(&obstacles, &[m2]), vec![0]);
    }

    #[test]
    fn chosen_boxes_block_later_movables() {
        let a = Movable { owner: None, candidates: vec![[0.0, 0.0, 5.0, 5.0]] };
        let b = Movable {
            owner: None,
            candidates: vec![[1.0, 1.0, 4.0, 4.0], [10.0, 10.0, 12.0, 12.0]],
        };
        assert_eq!(choose(&[], &[a, b]), vec![0, 1]);
    }

    #[test]
    fn edge_touching_is_not_collision() {
        let obstacles = vec![hard([0.0, 0.0, 10.0, 10.0])];
        let m = Movable { owner: None, candidates: vec![[10.0, 0.0, 14.0, 4.0]] };
        assert_eq!(choose(&obstacles, &[m]), vec![0]);
    }

    fn bbox_close(got: BBox, want: BBox) {
        for i in 0..4 {
            assert!((got[i] - want[i]).abs() < 1e-6, "got {got:?}, want {want:?}");
        }
    }

    #[test]
    fn label_box_per_direction() {
        // East: text extends +x from the anchor, floats 0.4 above (-y):
        // y range [20 - 0.4 - 1.6, 20 - 0.4] = [18.0, 19.6].
        bbox_close(label_box([10.0, 20.0], Dir::East, 5.5), [10.0, 18.0, 15.5, 19.6]);
        // West: extends -x.
        bbox_close(label_box([10.0, 20.0], Dir::West, 5.5), [4.5, 18.0, 10.0, 19.6]);
        // North: vertical text extending -y, floating 0.4 to the -x side.
        bbox_close(label_box([10.0, 20.0], Dir::North, 5.5), [8.0, 14.5, 9.6, 20.0]);
        // South: extending +y, floating to the +x side.
        bbox_close(label_box([10.0, 20.0], Dir::South, 5.5), [10.4, 20.0, 12.0, 25.5]);
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
            at: [-10.16, 2.54],
            angle: 0.0,
            length: 2.54,
            unit: 1,
        };
        let boxes = pin_text_boxes(&pin, [100.0, 100.0], 0.0, false);
        assert_eq!(boxes.len(), 2, "named pin -> name box + number box");
        bbox_close(boxes[0], [92.888, 96.66, 96.188, 98.26]);
        // Number box straddles the pin line midpoint, local (-8.89, 2.54):
        // sheet (91.11, 97.46) ± (1.1, 0.8).
        bbox_close(boxes[1], [90.01, 96.66, 92.21, 98.26]);
    }

    #[test]
    fn unnamed_pin_has_only_number_box() {
        let pin = PinGeom {
            number: "1".into(),
            name: "~".into(),
            at: [0.0, 3.81],
            angle: 270.0,
            length: 1.27,
            unit: 1,
        };
        let boxes = pin_text_boxes(&pin, [50.0, 50.0], 0.0, false);
        assert_eq!(boxes.len(), 1);
    }

    #[test]
    fn rotated_half_extents_swaps_at_90() {
        assert_eq!(rotated_half_extents([3.0, 1.0], 0.0), [3.0, 1.0]);
        assert_eq!(rotated_half_extents([3.0, 1.0], 90.0), [1.0, 3.0]);
        assert_eq!(rotated_half_extents([3.0, 1.0], 180.0), [3.0, 1.0]);
        assert_eq!(rotated_half_extents([3.0, 1.0], 270.0), [1.0, 3.0]);
    }
}
