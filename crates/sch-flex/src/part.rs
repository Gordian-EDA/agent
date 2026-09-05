//! What the typesetter needs to know about one part: the pins of the unit it is drawing,
//! where each lands under a pose, which way each points, and how much sheet the drawing
//! plus its text claims.

use std::collections::BTreeSet;

use circuit_graph::netclass::{is_connector_like, is_ground, is_power_net};
use geom::{Dir, Point2, Rect};
use kicad_symbol::geometry::PinGeom;
use sch_model::geometry::{field_pad, quantize_dir};
use sch_model::item::Item;

/// Body padding per side (mm), and the floor a bare passive's body gets — the same
/// convention as [`kicad_symbol::geometry::SymbolGeometry::approx_size`], but measured
/// over ONE unit's pins so a multi-unit symbol's power unit is not sized like the whole
/// package.
const BODY_PAD: f64 = 2.54;
const BODY_MIN: f64 = 5.08;
/// Reach of a power symbol on a pin stub, and of a net label's stub before its text.
const POWER_STUB: f64 = 7.62;
const LABEL_STUB: f64 = 3.81;

/// What the writer will hang off a pin beyond its tip.
///
/// Room for it is kept clear of whatever is drawn nearby, but it is NOT part of the box
/// the typesetter packs and aligns on: a label reaches into whatever gap its neighbour
/// leaves, the way a person writes one into the white space beside a part.
struct Attach {
    net: String,
    /// A power symbol rather than a net label — a glyph on the stub, not a line of text.
    power: bool,
    /// How far past the tip the stub runs before the glyph or the text starts.
    stub: f64,
}

/// One leaf of the tree, resolved against the item it draws.
pub struct Part<'a> {
    /// Index into the placement problem's items.
    pub index: usize,
    pub item: &'a Item,
    /// This unit's pins, in symbol order.
    pub pins: Vec<&'a PinGeom>,
    /// The power symbol or net label that will hang off each pin, in `pins` order.
    attach: Vec<Option<Attach>>,
}

impl<'a> Part<'a> {
    /// `labelled` names the pins that will carry a net label — one per net leaving the
    /// block — so the measure keeps room for exactly the labels that get drawn.
    pub fn new(index: usize, item: &'a Item, labelled: &BTreeSet<(usize, String)>) -> Self {
        // This unit's pins: the geometry's own, plus any the caller's net map names.
        // Neither alone is complete — `PinGeom::unit` folds a symbol's common pins onto
        // unit 1 (dropping an op-amp's shared supplies), and a net map lifted off a live
        // sheet carries only the pins that are connected.
        let pins: Vec<&PinGeom> = item
            .geom
            .pins
            .iter()
            .filter(|p| {
                p.unit == item.unit
                    || item.pins.iter().any(|(number, ..)| *number == p.number)
            })
            .collect();
        let mut part = Self {
            index,
            item,
            pins,
            attach: Vec::new(),
        };
        part.attach = part
            .pins
            .iter()
            .map(|pin| match part.net(pin) {
                Some(net) if is_power_net(net) => Some(Attach {
                    net: net.to_owned(),
                    power: true,
                    stub: POWER_STUB,
                }),
                Some(net) if labelled.contains(&(index, pin.number.clone())) => Some(Attach {
                    net: net.to_owned(),
                    power: false,
                    stub: LABEL_STUB,
                }),
                _ => None,
            })
            .collect();
        part
    }

    /// The net on `pin`, if it carries one.
    pub fn net(&self, pin: &PinGeom) -> Option<&str> {
        self.item
            .pins
            .iter()
            .find(|(number, ..)| *number == pin.number)
            .and_then(|(_, _, net)| net.as_deref())
    }

    /// Every net this part's unit connects to.
    pub fn nets(&self) -> Vec<&str> {
        self.pins.iter().filter_map(|p| self.net(p)).collect()
    }

    /// The power/ground nets among them — the rails that decide which way the part stands.
    pub fn rails(&self) -> Vec<&str> {
        self.nets()
            .into_iter()
            .filter(|net| is_power_net(net))
            .collect()
    }

    pub fn two_pin(&self) -> bool {
        self.pins.len() == 2
    }

    /// Whether `pin`'s net ENDS at this pin on the drawing — a label, a rail symbol or
    /// a ground stub — rather than continuing sideways to another part. It is the
    /// difference between a leg off a rail (stands) and a series element that happens to
    /// touch one (lies along its chain); standing the second forces a U-turn around its
    /// own body.
    pub fn terminates(&self, pin: &PinGeom) -> bool {
        self.pins
            .iter()
            .position(|p| std::ptr::eq(*p, pin))
            .is_some_and(|i| self.attach.get(i).is_some_and(|room| *room > 0.0))
    }

    /// Connectors and headers: a human mirrors the symbol to face the circuit rather than
    /// drawing wires around it, and never rotates it.
    pub fn is_connector(&self) -> bool {
        is_connector_like(&self.item.part) || self.item.refdes.starts_with('J')
    }

    /// Sheet-space offset of `pin` from the instance origin, under `pose`.
    pub fn pin_offset(&self, pin: &PinGeom, pose: Pose) -> Point2 {
        pin.at.transform_offset(pose.angle, pose.mirror)
    }

    /// Which way `pin` points away from the body, under `pose`.
    pub fn pin_dir(&self, pin: &PinGeom, pose: Pose) -> Dir {
        quantize_dir(pin.angle, pose.angle, pose.mirror)
    }

    /// The box the typesetter PACKS: the body its pins bound, plus the band the writer
    /// seats the reference/value pair in. Text that hangs off a pin is [`Part::overhang`]
    /// — measured, kept clear, never summed into a row's width.
    pub fn body(&self, pose: Pose) -> Rect {
        let mut points: Vec<Point2> = self
            .pins
            .iter()
            .map(|p| self.pin_offset(p, pose))
            .collect();
        points.push(Point2::new(0.0, 0.0));
        let bounds = Rect::bounding(&points).unwrap_or_else(|| Rect::new(0.0, 0.0, 0.0, 0.0));
        let (w, h) = (
            bounds.width().max(BODY_MIN) + 2.0 * BODY_PAD,
            bounds.height().max(BODY_MIN) + 2.0 * BODY_PAD,
        );
        let mid = Point2::new(
            (bounds.min_x + bounds.max_x) / 2.0,
            (bounds.min_y + bounds.max_y) / 2.0,
        );
        let mut r = Rect::new(
            mid.x - w / 2.0,
            mid.y - h / 2.0,
            mid.x + w / 2.0,
            mid.y + h / 2.0,
        );
        let [left, right, top, bottom] = field_pad(self.item, w, h);
        r.min_x -= left;
        r.max_x += right;
        r.min_y -= top;
        r.max_y += bottom;
        r
    }

    /// The text and glyphs this part draws OUTSIDE its body: one box per net label or
    /// power symbol on a pin, plus a connector's fields, which go beside the symbol
    /// rather than under it. Each is checked for collision where it lands; none of them
    /// widens the part.
    pub fn overhang(&self, pose: Pose) -> Vec<Rect> {
        let mut out: Vec<Rect> = Vec::new();
        for (pin, attach) in self.pins.iter().zip(&self.attach) {
            let Some(attach) = attach else { continue };
            let tip = self.pin_offset(pin, pose);
            let dir = self.pin_dir(pin, pose);
            let d = dir.vec();
            let at = Point2::new(tip.x + d.x * attach.stub, tip.y + d.y * attach.stub);
            let glyph = if attach.power {
                // A rail glyph says its own name, centred on the stub it stands on.
                let half = (sch_model::text::text_width(&attach.net) / 2.0).max(BODY_PAD);
                Rect::from_center_half(at, (half, half))
            } else {
                sch_model::text::label_box(at, dir, &attach.net)
            };
            out.push(
                Rect::bounding(&[
                    tip,
                    Point2::new(glyph.min_x, glyph.min_y),
                    Point2::new(glyph.max_x, glyph.max_y),
                ])
                .expect("three points bound a box"),
            );
        }
        if self.is_connector() {
            // A connector's fields go beside it, on the side its pins leave free — a
            // jack's value ("OUTPUT 3.5mm") is far wider than the symbol.
            let text = sch_model::text::text_width(&self.item.value)
                .max(sch_model::text::text_width(&self.item.refdes));
            let r = self.body(pose);
            let beside = |min_x: f64, max_x: f64| Rect::new(min_x, r.min_y, max_x, r.max_y);
            match self.pins_face_west(pose) {
                Some(true) => out.push(beside(r.max_x, r.max_x + text)),
                Some(false) => out.push(beside(r.min_x - text, r.min_x)),
                None => {
                    out.push(beside(r.min_x - text, r.min_x));
                    out.push(beside(r.max_x, r.max_x + text));
                }
            }
        }
        out
    }

    /// Everything this part draws: its body and every overhang. What a block's frame has
    /// to enclose — not what its neighbours have to make room for.
    pub fn extent(&self, pose: Pose) -> Rect {
        let mut r = self.body(pose);
        for o in self.overhang(pose) {
            r.min_x = r.min_x.min(o.min_x);
            r.max_x = r.max_x.max(o.max_x);
            r.min_y = r.min_y.min(o.min_y);
            r.max_y = r.max_y.max(o.max_y);
        }
        r
    }

    /// Whether this part's pins all leave to the WEST under `pose` (so its fields belong
    /// to the east), all to the EAST, or neither — a connector whose pins face both ways
    /// leaves no side free and keeps room on both.
    fn pins_face_west(&self, pose: Pose) -> Option<bool> {
        let mut west = false;
        let mut east = false;
        for pin in &self.pins {
            match self.pin_dir(pin, pose) {
                Dir::West => west = true,
                Dir::East => east = true,
                _ => return None,
            }
        }
        match (west, east) {
            (true, false) => Some(true),
            (false, true) => Some(false),
            _ => None,
        }
    }

    /// Whether the two pins of a 2-pin part run left↔right under `pose`.
    pub fn lies_flat(&self, pose: Pose) -> bool {
        let (a, b) = (
            self.pin_offset(self.pins[0], pose),
            self.pin_offset(self.pins[1], pose),
        );
        (a.y - b.y).abs() < geom::EPS
    }

    /// The rotations that put a 2-pin part's pins on the wanted axis, 0° and 90° first.
    pub fn rotations_along(&self, horizontal: bool) -> Vec<f64> {
        [0.0, 90.0, 180.0, 270.0]
            .into_iter()
            .filter(|angle| {
                self.lies_flat(Pose {
                    angle: *angle,
                    mirror: false,
                }) == horizontal
            })
            .collect()
    }
}

/// A trial orientation: what the typesetter is deciding for a leaf.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Pose {
    pub angle: f64,
    pub mirror: bool,
}

/// Ground pins belong at the bottom of a drawing and supplies at the top; this is the
/// penalty for a pose that says otherwise.
pub fn rail_penalty(net: &str, dir: Dir) -> f64 {
    let want = if is_ground(net) { Dir::South } else { Dir::North };
    if dir == want {
        0.0
    } else if dir == Dir::East || dir == Dir::West {
        6.0
    } else {
        20.0
    }
}
