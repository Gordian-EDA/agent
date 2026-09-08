//! Visual defects measured from a sheet's geometry: what the render shows as
//! symbols on top of each other, wires through bodies, and text on text.

use std::collections::{BTreeMap, BTreeSet};

use crate::geom::{Point, Rect};
use crate::net;
use crate::sch::{Field, Justify, Schematic};

#[derive(Debug, Clone)]
pub struct VisualFacts {
    pub sheet_extent: [f64; 4],
    pub body_overlaps: Vec<[String; 2]>,
    pub wires_through_bodies: Vec<(String, String)>,
    pub text_collisions: Vec<(String, String, String)>,
}

/// One drawn piece of text and the box it occupies.
struct Text {
    owner: Option<String>,
    what: String,
    text: String,
    bbox: Rect,
    /// A label rides its own wire by design and is not checked against wires.
    is_label: bool,
}

impl Text {
    fn describe(&self) -> String {
        format!("{} \"{}\"", self.what, self.text)
    }
}

pub fn measure(doc: &Schematic) -> VisualFacts {
    let bodies = symbol_bodies(doc);
    let wires = net::scene(doc).segments;
    let texts = drawn_texts(doc);
    VisualFacts {
        sheet_extent: sheet_extent(doc, &bodies, &texts),
        body_overlaps: body_overlaps(&bodies),
        wires_through_bodies: wires_through_bodies(doc, &bodies, &wires),
        text_collisions: text_collisions(&texts, &bodies, &wires),
    }
}

/// The bodies the drawing is made of, keyed by reference and unit. Power
/// symbols and unnamed parts are left out: they carry no layout to judge.
fn symbol_bodies(doc: &Schematic) -> BTreeMap<(String, u32), Rect> {
    doc.symbols
        .iter()
        .filter(|s| !s.refdes().is_empty() && !s.refdes().starts_with('#'))
        .filter_map(|s| Some(((s.refdes().to_string(), s.unit), doc.body_rect(s)?)))
        .collect()
}

fn body_overlaps(bodies: &BTreeMap<(String, u32), Rect>) -> Vec<[String; 2]> {
    let bodies: Vec<_> = bodies.iter().collect();
    let mut pairs = BTreeSet::new();
    for i in 0..bodies.len() {
        for j in (i + 1)..bodies.len() {
            if bodies[i].1.overlaps(bodies[j].1) {
                let (a, b) = (&bodies[i].0.0, &bodies[j].0.0);
                pairs.insert(if a <= b {
                    [a.clone(), b.clone()]
                } else {
                    [b.clone(), a.clone()]
                });
            }
        }
    }
    pairs.into_iter().collect()
}

/// Wires that enter a symbol body's interior instead of stopping at its pins.
fn wires_through_bodies(
    doc: &Schematic,
    bodies: &BTreeMap<(String, u32), Rect>,
    wires: &[(Point, Point, String)],
) -> Vec<(String, String)> {
    let mut pins_of: BTreeMap<(String, u32), Vec<Point>> = BTreeMap::new();
    for pin in doc.placed_pins() {
        pins_of
            .entry((pin.refdes.clone(), pin.unit))
            .or_default()
            .push(pin.at);
    }
    let mut found = BTreeSet::new();
    for (key, body) in bodies {
        let pins = pins_of.get(key).map(Vec::as_slice).unwrap_or(&[]);
        for (from, to, net) in wires {
            // A wire may run right up to a pin; only entering the body counts,
            // and a two-pin part's own axis is the wire the library draws over.
            if !body.crossed_by(*from, *to) {
                continue;
            }
            let along_axis = pins.len() == 2
                && crate::geom::on_segment(pins[0], *from, *to)
                && crate::geom::on_segment(pins[1], *from, *to);
            if along_axis {
                continue;
            }
            found.insert((net.clone(), key.0.clone()));
        }
    }
    found.into_iter().collect()
}

/// Text against text, text against a body that is not its own, and text on a
/// wire. Reported as `(reference, what, with)`.
fn text_collisions(
    texts: &[Text],
    bodies: &BTreeMap<(String, u32), Rect>,
    wires: &[(Point, Point, String)],
) -> Vec<(String, String, String)> {
    let mut found = BTreeSet::new();
    for (i, a) in texts.iter().enumerate() {
        for b in &texts[i + 1..] {
            if a.bbox.overlaps(&b.bbox) {
                found.insert((
                    a.owner.clone().unwrap_or_default(),
                    a.describe(),
                    b.describe(),
                ));
            }
        }
        for ((other, _), body) in bodies {
            if a.owner.as_deref() != Some(other.as_str()) && a.bbox.overlaps(body) {
                found.insert((
                    a.owner.clone().unwrap_or_default(),
                    a.describe(),
                    other.clone(),
                ));
            }
        }
        if a.is_label {
            continue;
        }
        for (from, to, net) in wires {
            if a.bbox.crossed_by(*from, *to) {
                found.insert((
                    a.owner.clone().unwrap_or_default(),
                    a.describe(),
                    net.clone(),
                ));
            }
        }
    }
    found.into_iter().collect()
}

/// Every visible field, label and note the sheet draws.
fn drawn_texts(doc: &Schematic) -> Vec<Text> {
    let mut out = Vec::new();
    for symbol in &doc.symbols {
        for field in &symbol.fields {
            if field.hidden || field.value.is_empty() || !shown(field) {
                continue;
            }
            out.push(Text {
                owner: Some(symbol.refdes().to_string()),
                what: "field".into(),
                text: field.value.clone(),
                bbox: text_box(field.at, field.rot, field.size, &field.value, field.justify),
                is_label: false,
            });
        }
    }
    for label in &doc.labels {
        out.push(Text {
            owner: None,
            what: "label".into(),
            text: label.text.clone(),
            // A label's anchor is its attachment point; KiCad draws the text
            // away from it along the label's own rotation.
            bbox: text_box(label.at, label.rot, label.size, &label.text, Justify::Left),
            is_label: true,
        });
    }
    for text in &doc.texts {
        out.push(Text {
            owner: None,
            what: "text".into(),
            text: text.text.clone(),
            bbox: text_box(text.at, text.rot, text.size, &text.text, text.justify),
            is_label: false,
        });
    }
    out
}

/// Fields KiCad keeps out of the drawing whatever their value.
fn shown(field: &Field) -> bool {
    !matches!(
        field.name.as_str(),
        "Footprint" | "Datasheet" | "Description" | "ki_keywords" | "ki_fp_filters"
    ) && !field.name.starts_with("ki_")
}

/// The box a run of text occupies. KiCad's stroke font is about 0.6 em wide per
/// glyph with a 0.2 em gap, and a line box is about 1.2 em tall. The anchor sits
/// at the justified edge and the text runs along its own rotation: 0 to the
/// right, 90 upwards, 180 to the left, 270 downwards.
fn text_box(at: Point, rot: f64, size: f64, text: &str, justify: Justify) -> Rect {
    let lines: Vec<&str> = text.split('\n').collect();
    let longest = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    let width = longest as f64 * size * 0.8;
    let height = lines.len() as f64 * size * 1.2;
    let (sin, cos) = rot.rem_euclid(360.0).to_radians().sin_cos();
    let along = Point::new(cos, -sin);
    let across = Point::new(-sin, -cos);
    let (from, to) = match justify {
        Justify::Left => (0.0, width),
        Justify::Right => (-width, 0.0),
        Justify::Center => (-width / 2.0, width / 2.0),
    };
    let corners: Vec<Point> = [(from, -height / 2.0), (to, height / 2.0)]
        .iter()
        .map(|(u, v)| {
            Point::new(
                at.x + along.x * u + across.x * v,
                at.y + along.y * u + across.y * v,
            )
        })
        .collect();
    Rect::bounding(&corners).expect("two corners bound a box")
}

fn sheet_extent(
    doc: &Schematic,
    bodies: &BTreeMap<(String, u32), Rect>,
    texts: &[Text],
) -> [f64; 4] {
    let mut points = Vec::new();
    let mut corners = |rect: &Rect| {
        points.push(Point::new(rect.min_x, rect.min_y));
        points.push(Point::new(rect.max_x, rect.max_y));
    };
    for body in bodies.values() {
        corners(body);
    }
    for text in texts {
        corners(&text.bbox);
    }
    for wire in &doc.wires {
        points.extend(wire.iter().copied());
    }
    for pin in doc.placed_pins() {
        points.push(pin.at);
    }
    for symbol in &doc.symbols {
        points.push(symbol.at);
    }
    Rect::bounding(&points).map_or([0.0; 4], |r| [r.min_x, r.min_y, r.max_x, r.max_y])
}
