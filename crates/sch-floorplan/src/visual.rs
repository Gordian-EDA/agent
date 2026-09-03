//! Deterministic visual facts measured from a live schematic document.

use std::collections::{BTreeMap, BTreeSet};

use geom::{EPS, GRID_50_MIL, Point2, Rect};
use sch_doc::{Item as DocItem, SchDoc, connect, placed_pins};
use sch_model::item::Item;
use sch_model::text::TextKind;
use serde::Serialize;

use crate::floorplan::place::{
    count_body_crossings, count_collinear_body_crossings, count_ic_body_crossings,
    count_parallel_body_crossings,
};
use sch_model::route::DrawnSegment;

/// One wire net that passes through a placed symbol body.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct WireBodyCollision {
    pub net: String,
    #[serde(rename = "ref")]
    pub reference: String,
}

/// One visible symbol field colliding with a wire net or another body.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct TextCollision {
    #[serde(rename = "ref")]
    pub reference: String,
    pub field: String,
    pub with: String,
}

/// Stable visual measurements returned beside a schematic render.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VisualFacts {
    pub sheet_extent: [f64; 4],
    pub body_overlaps: Vec<[String; 2]>,
    pub wires_through_bodies: Vec<WireBodyCollision>,
    pub text_collisions: Vec<TextCollision>,
    pub off_grid_pins: Vec<String>,
    pub dangling_wire_ends: Vec<[f64; 2]>,
}

/// Measure visual facts using the same live scene and crossing primitives as placement.
pub fn measure(doc: &SchDoc) -> VisualFacts {
    let items = crate::live::scene_items(doc);
    let bodies = symbol_bodies(doc);
    let scene = connect::scene(doc);
    let wires: Vec<(DrawnSegment, String)> = scene
        .segments
        .into_iter()
        .map(|(a, b, net)| (DrawnSegment::new(a, b, Some(net.clone())), net))
        .collect();

    VisualFacts {
        sheet_extent: sheet_extent(doc, &bodies),
        body_overlaps: body_overlap_pairs(&bodies),
        wires_through_bodies: wire_body_collisions(doc, &items, &bodies, &wires),
        text_collisions: text_collisions(doc, &bodies, &wires),
        off_grid_pins: off_grid_pins(doc),
        dangling_wire_ends: dangling_wire_ends(doc),
    }
}

fn body_overlap_pairs(bodies: &BTreeMap<(String, u32), Rect>) -> Vec<[String; 2]> {
    let bodies: Vec<_> = bodies.iter().collect();
    let mut pairs = BTreeSet::new();
    for i in 0..bodies.len() {
        for j in (i + 1)..bodies.len() {
            if bodies[i].1.overlaps(bodies[j].1) {
                let a = &bodies[i].0.0;
                let b = &bodies[j].0.0;
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

/// The bodies the DRAWING is made of. Benched symbols are left out: they have no
/// layout to measure, and letting them into the extent would blow up every render
/// bound and every readability count with parts nobody has placed yet.
fn symbol_bodies(doc: &SchDoc) -> BTreeMap<(String, u32), Rect> {
    doc.symbols()
        .filter(|symbol| !crate::bench::is_benched(symbol))
        .filter(|symbol| !symbol.refdes().is_empty() && !symbol.refdes().starts_with('#'))
        .filter_map(|symbol| {
            Some((
                (symbol.refdes().to_string(), symbol.unit),
                sch_doc::body_rect(doc, symbol)?,
            ))
        })
        .collect()
}

fn wire_body_collisions(
    doc: &SchDoc,
    items: &[Item],
    bodies: &BTreeMap<(String, u32), Rect>,
    wires: &[(DrawnSegment, String)],
) -> Vec<WireBodyCollision> {
    let pins = placed_pins(doc);
    let mut found = BTreeSet::new();
    for item in items {
        let unit_pins: Vec<Point2> = pins
            .iter()
            .filter(|pin| pin.refdes == item.refdes && pin.unit == item.unit as u32)
            .map(|pin| pin.at)
            .collect();
        for (wire, net) in wires {
            let slice = std::slice::from_ref(wire);
            let crosses = if unit_pins.len() == 2 {
                let axis = (unit_pins[0].into(), unit_pins[1].into());
                count_body_crossings(&[axis], slice)
                    + count_collinear_body_crossings(&[axis], slice)
                    + count_parallel_body_crossings(&[axis], slice)
                    > 0
            } else {
                bodies
                    .get(&(item.refdes.clone(), item.unit as u32))
                    .is_some_and(|body| count_ic_body_crossings(&[*body], slice) > 0)
            };
            if crosses {
                found.insert(WireBodyCollision {
                    net: net.clone(),
                    reference: item.refdes.clone(),
                });
            }
        }
    }
    found.into_iter().collect()
}

/// Everything the render shows as text touching something it should not.
///
/// The item set is [`sch_doc::drawn_texts`] — every field, label, port, note
/// and pin name/number the sheet draws — boxed by the one as-drawn model, so a
/// hit here is a hit the eye finds in the render. Pairs that are legitimate by
/// construction are exempt: a text against its OWN symbol's body, and two pin
/// texts of the same symbol (the library's intra-symbol layout, not ours).
/// Bodies and wires are checked only against the text that has no business
/// near them — a label rides its own wire by design, and pin text lies inside
/// its own body.
fn text_collisions(
    doc: &SchDoc,
    bodies: &BTreeMap<(String, u32), Rect>,
    wires: &[(DrawnSegment, String)],
) -> Vec<TextCollision> {
    let benched: BTreeSet<String> = doc
        .symbols()
        .filter(|symbol| crate::bench::is_benched(symbol))
        .map(|symbol| symbol.refdes().to_string())
        .collect();
    let texts: Vec<sch_model::text::DrawnText> = sch_doc::drawn_texts(doc)
        .into_iter()
        .filter(|text| !text.owner.as_ref().is_some_and(|r| benched.contains(r)))
        .collect();
    let mut found = BTreeSet::new();
    for (i, a) in texts.iter().enumerate() {
        for b in &texts[i + 1..] {
            if !a.bbox.overlaps(&b.bbox) {
                continue;
            }
            let same_owner = a.owner.is_some() && a.owner == b.owner;
            if same_owner && is_pin_text(a.kind) && is_pin_text(b.kind) {
                continue;
            }
            found.insert(TextCollision {
                reference: a.owner.clone().unwrap_or_default(),
                field: describe(a),
                with: describe(b),
            });
        }
        if is_pin_text(a.kind) {
            continue;
        }
        for ((other, _), body) in bodies {
            if a.owner.as_deref() != Some(other.as_str()) && a.bbox.overlaps(body) {
                found.insert(TextCollision {
                    reference: a.owner.clone().unwrap_or_default(),
                    field: describe(a),
                    with: other.clone(),
                });
            }
        }
        if matches!(a.kind, TextKind::Label | TextKind::PortLabel) {
            continue;
        }
        for (wire, net) in wires {
            if wire.segment.dist_to_rect(&a.bbox) <= EPS {
                found.insert(TextCollision {
                    reference: a.owner.clone().unwrap_or_default(),
                    field: describe(a),
                    with: net.clone(),
                });
            }
        }
    }
    found.into_iter().collect()
}

fn is_pin_text(kind: TextKind) -> bool {
    matches!(kind, TextKind::PinName | TextKind::PinNumber)
}

/// How a drawn text names itself in a warning.
fn describe(text: &sch_model::text::DrawnText) -> String {
    let what = match text.kind {
        TextKind::Field => "field",
        TextKind::Label => "label",
        TextKind::PortLabel => "port",
        TextKind::FreeText => "text",
        TextKind::PinName => "pin name",
        TextKind::PinNumber => "pin",
    };
    format!("{what} \"{}\"", text.text)
}

fn off_grid_pins(doc: &SchDoc) -> Vec<String> {
    let mut pins: Vec<String> = placed_pins(doc)
        .into_iter()
        .filter(|pin| !pin.refdes.is_empty() && !pin.refdes.starts_with('#'))
        .filter(|pin| {
            (GRID_50_MIL.snap(pin.at.x) - pin.at.x).abs() > 1e-6
                || (GRID_50_MIL.snap(pin.at.y) - pin.at.y).abs() > 1e-6
        })
        .map(|pin| format!("{}.{}", pin.refdes, pin.number))
        .collect();
    pins.sort();
    pins.dedup();
    pins
}

fn dangling_wire_ends(doc: &SchDoc) -> Vec<[f64; 2]> {
    let key = |point: Point2| {
        (
            (point.x * 1000.0).round() as i64,
            (point.y * 1000.0).round() as i64,
        )
    };
    let mut endpoint_counts = BTreeMap::new();
    for wire in doc.wires() {
        for point in wire.points.first().into_iter().chain(wire.points.last()) {
            *endpoint_counts.entry(key(*point)).or_insert(0usize) += 1;
        }
    }
    let mut anchored: BTreeSet<(i64, i64)> = placed_pins(doc)
        .into_iter()
        .map(|pin| key(pin.at))
        .collect();
    for item in doc.items() {
        match item {
            DocItem::Junction(junction) => {
                anchored.insert(key(junction.at));
            }
            DocItem::NoConnect(marker) => {
                anchored.insert(key(marker.at));
            }
            DocItem::Label(label) => {
                anchored.insert(key(label.at.point()));
            }
            DocItem::Sheet(sheet) => {
                anchored.extend(sheet.pins.iter().map(|pin| key(pin.at.point())));
            }
            _ => {}
        }
    }

    endpoint_counts
        .into_iter()
        .filter(|(point, count)| *count == 1 && !anchored.contains(point))
        .map(|((x, y), _)| [x as f64 / 1000.0, y as f64 / 1000.0])
        .collect()
}

fn sheet_extent(doc: &SchDoc, bodies: &BTreeMap<(String, u32), Rect>) -> [f64; 4] {
    let mut points = Vec::new();
    for body in bodies.values() {
        extend_rect(&mut points, *body);
    }
    for wire in doc.wires() {
        points.extend(wire.points.iter().copied());
    }
    for pin in placed_pins(doc) {
        points.push(pin.at);
    }
    for symbol in doc.symbols() {
        points.push(symbol.at.point());
    }
    for text in sch_doc::drawn_texts(doc) {
        extend_rect(&mut points, text.bbox);
    }
    for item in doc.items() {
        match item {
            DocItem::Sheet(sheet) => {
                points.push(sheet.at);
                points.push(Point2::new(
                    sheet.at.x + sheet.size.x,
                    sheet.at.y + sheet.size.y,
                ));
            }
            _ => {}
        }
    }
    Rect::bounding(&points).map_or([0.0; 4], |rect| {
        [rect.min_x, rect.min_y, rect.max_x, rect.max_y]
    })
}

fn extend_rect(points: &mut Vec<Point2>, rect: Rect) {
    points.push(Point2::new(rect.min_x, rect.min_y));
    points.push(Point2::new(rect.max_x, rect.max_y));
}
