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

/// Room a net label's text takes beyond its stub.
fn label_room(net: &str) -> f64 {
    sch_model::text::text_width(net)
}

/// One leaf of the tree, resolved against the item it draws.
pub struct Part<'a> {
    /// Index into the placement problem's items.
    pub index: usize,
    pub item: &'a Item,
    /// This unit's pins, in symbol order.
    pub pins: Vec<&'a PinGeom>,
    /// Room (mm) each pin needs beyond its tip for the power symbol or net label that will
    /// hang there, in `pins` order.
    attach: Vec<f64>,
}

impl<'a> Part<'a> {
    /// `labelled` names the pins that will carry a net label — one per net leaving the
    /// block — so the measure keeps room for exactly the labels that get drawn.
    pub fn new(index: usize, item: &'a Item, labelled: &BTreeSet<(usize, String)>) -> Self {
        // The unit's pins are the ones the LOWERING resolved onto this item, matched by
        // number: `PinGeom::unit` folds a symbol's common pins onto unit 1, so filtering
        // the geometry by unit drops an op-amp's shared supply pins.
        let pins = item
            .pins
            .iter()
            .filter_map(|(number, ..)| item.geom.pins.iter().find(|p| p.number == *number))
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
                Some(net) if is_power_net(net) => POWER_STUB,
                Some(net) if labelled.contains(&(index, pin.number.clone())) => {
                    LABEL_STUB + label_room(net)
                }
                _ => 0.0,
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

    /// The drawing's claim on the sheet under `pose`, relative to the instance origin:
    /// the body the pins bound, plus the band the writer seats the reference/value pair in.
    pub fn extent(&self, pose: Pose) -> Rect {
        let mut points: Vec<Point2> = self
            .pins
            .iter()
            .map(|p| self.pin_offset(p, pose))
            .collect();
        points.push(Point2::new(0.0, 0.0));
        let attachments: Vec<Point2> = self
            .pins
            .iter()
            .zip(&self.attach)
            .filter(|(_, reach)| **reach > 0.0)
            .map(|(pin, reach)| {
                let tip = self.pin_offset(pin, pose);
                let d = self.pin_dir(pin, pose).vec();
                Point2::new(tip.x + d.x * reach, tip.y + d.y * reach)
            })
            .collect();
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
        if self.is_connector() {
            // A connector's fields go beside it, on whichever side its pins leave free —
            // and a jack's value ("OUTPUT 3.5mm") is far wider than the symbol. Half of it
            // (what `field_pad` reserves) is not the room the solver takes.
            let text = sch_model::text::text_width(&self.item.value)
                .max(sch_model::text::text_width(&self.item.refdes));
            r.min_x -= text;
            r.max_x += text;
        }
        for a in attachments {
            r.min_x = r.min_x.min(a.x);
            r.max_x = r.max_x.max(a.x);
            r.min_y = r.min_y.min(a.y);
            r.max_y = r.max_y.max(a.y);
        }
        r
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
