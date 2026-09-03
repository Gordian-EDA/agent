//! Every text the sheet DRAWS, boxed by the one as-drawn model.
//!
//! [`drawn_texts`] is the item set: visible symbol fields (including a power
//! symbol's `#`-prefixed Reference and its rail-name Value), local labels,
//! global and hierarchical labels, free notes, and the name/number KiCAD
//! prints beside every pin. Boxing them all through
//! [`sch_model::text`] is what lets a readability lint report exactly what a
//! render shows.

use kiutils_sexpr::Node;
use sch_model::text::{DrawnText, FONT_SIZE, HJust, TextKind, VJust};

use crate::doc::SchDoc;
use crate::model::{Item, LabelKind, SymbolInst};
use crate::pins;
use crate::sexpr::{self, child, items};

/// Font size and justification from a node's `(effects …)`, with KiCAD's
/// defaults: 1.27 mm, and centred both ways when a token is absent.
fn effects(node: &Node) -> (f64, HJust, VJust, bool) {
    let Some(effects) = child(node, "effects") else {
        return (FONT_SIZE, HJust::Center, VJust::Center, false);
    };
    let size = child(effects, "font")
        .and_then(|font| child(font, "size"))
        .and_then(|size| items(size).get(1).and_then(sexpr::number))
        .unwrap_or(FONT_SIZE);
    let (mut hjust, mut vjust) = (HJust::Center, VJust::Center);
    if let Some(justify) = child(effects, "justify") {
        for token in items(justify).iter().skip(1).filter_map(sexpr::text) {
            match token {
                "left" => hjust = HJust::Left,
                "right" => hjust = HJust::Right,
                "top" => vjust = VJust::Top,
                "bottom" => vjust = VJust::Bottom,
                _ => {}
            }
        }
    }
    (size, hjust, vjust, sexpr::flag_present(effects, "hide"))
}

/// A symbol property's DRAWN angle. KiCAD composes a property's angle with its
/// symbol's and then folds it into `[0, 180)`, auto-flipping past 180° so the
/// text stays readable.
fn field_angle(symbol_rot: f64, field_rot: f64) -> f64 {
    (symbol_rot + field_rot).rem_euclid(180.0)
}

/// Text drawn by one placed symbol: its visible fields and its pin text.
fn symbol_texts(doc: &SchDoc, symbol: &SymbolInst, out: &mut Vec<DrawnText>) {
    let owner = symbol.refdes().to_string();
    for field in symbol.fields.values() {
        let Some(at) = field.at.filter(|_| !field.hidden && !field.value.is_empty()) else {
            continue;
        };
        let (size, hjust, vjust, _) = effects(field.node());
        out.push(DrawnText {
            owner: Some(owner.clone()),
            kind: TextKind::Field,
            text: field.value.clone(),
            bbox: sch_model::text::drawn_box(
                &field.value,
                size,
                hjust,
                vjust,
                field_angle(symbol.at.rot, at.rot),
                at.point(),
            ),
        });
    }
    for pin in pins::pins_of(doc, symbol) {
        for (kind, bbox) in sch_model::text::placed_pin_texts(&pin.as_drawn()) {
            out.push(DrawnText {
                owner: Some(owner.clone()),
                kind,
                text: match kind {
                    TextKind::PinName => pin.name.clone(),
                    _ => pin.number.clone(),
                },
                bbox,
            });
        }
    }
}

/// Every text the sheet draws, in document order.
pub fn drawn_texts(doc: &SchDoc) -> Vec<DrawnText> {
    let mut out = Vec::new();
    for symbol in doc.symbols() {
        symbol_texts(doc, symbol, &mut out);
    }
    for item in doc.items() {
        match item {
            Item::Label(label) => {
                let node = label.raw.node();
                let (size, hjust, vjust, hidden) = effects(node);
                if hidden || label.text.is_empty() {
                    continue;
                }
                let angle = label.at.rot.rem_euclid(180.0);
                // KiCAD renders the DECODED string: `A{slash}B` draws three
                // glyphs, not nine.
                let drawn = crate::text::unescape(&label.text);
                let bbox = match label.kind {
                    LabelKind::Local => sch_model::text::local_label_box(
                        &drawn,
                        size,
                        hjust,
                        vjust,
                        angle,
                        label.at.point(),
                    ),
                    _ => sch_model::text::port_label_text_box(
                        &drawn,
                        size,
                        hjust,
                        angle,
                        label.at.point(),
                    ),
                };
                out.push(DrawnText {
                    owner: None,
                    kind: match label.kind {
                        LabelKind::Local => TextKind::Label,
                        _ => TextKind::PortLabel,
                    },
                    text: drawn,
                    bbox,
                });
            }
            Item::Text(text) => {
                let (size, hjust, vjust, hidden) = effects(text.raw.node());
                if hidden || text.text.is_empty() {
                    continue;
                }
                let drawn = crate::text::unescape(&text.text);
                out.push(DrawnText {
                    owner: None,
                    kind: TextKind::FreeText,
                    bbox: sch_model::text::note_box(
                        &drawn,
                        size,
                        hjust,
                        vjust,
                        text.at.rot.rem_euclid(180.0),
                        text.at.point(),
                    ),
                    text: drawn,
                });
            }
            Item::Sheet(sheet) => sheet_texts(sheet, &mut out),
            _ => {}
        }
    }
    // One glyph on the page counts once. A generator that stacks two identical
    // power symbols on the same point draws its rail name twice at the same
    // place; boxing both would report the sheet as colliding with itself.
    let mut seen = std::collections::BTreeSet::new();
    out.retain(|text| {
        let micron = |v: f64| (v * 1000.0).round() as i64;
        seen.insert((
            text.text.clone(),
            micron(text.bbox.min_x),
            micron(text.bbox.min_y),
            micron(text.bbox.max_x),
            micron(text.bbox.max_y),
        ))
    });
    out
}

/// Text drawn by a hierarchical sheet: its Sheetname/Sheetfile properties and
/// the port label on every sheet pin.
fn sheet_texts(sheet: &crate::model::Sheet, out: &mut Vec<DrawnText>) {
    for property in items(sheet.raw.node())
        .iter()
        .filter(|node| sexpr::head(node) == Some("property"))
    {
        let Some(value) = items(property).get(2).and_then(sexpr::text) else {
            continue;
        };
        let (size, hjust, vjust, hidden) = effects(property);
        if hidden || value.is_empty() {
            continue;
        }
        let Some(at) = child(property, "at") else {
            continue;
        };
        let coord = |i: usize| items(at).get(i).and_then(sexpr::number).unwrap_or_default();
        out.push(DrawnText {
            owner: None,
            kind: TextKind::Field,
            text: value.to_string(),
            bbox: sch_model::text::drawn_box(
                value,
                size,
                hjust,
                vjust,
                coord(3).rem_euclid(180.0),
                geom::Point2::new(coord(1), coord(2)),
            ),
        });
    }
    for pin in &sheet.pins {
        let drawn = crate::text::unescape(&pin.name);
        // A sheet pin reads INTO the sheet: angle 0 sits on the left border
        // with its text running right, 180 on the right border running left.
        let hjust = if (90.0..270.0).contains(&pin.at.rot.rem_euclid(360.0)) {
            HJust::Right
        } else {
            HJust::Left
        };
        out.push(DrawnText {
            owner: None,
            kind: TextKind::PortLabel,
            bbox: sch_model::text::port_label_text_box(
                &drawn,
                FONT_SIZE,
                hjust,
                pin.at.rot.rem_euclid(180.0),
                pin.at.point(),
            ),
            text: drawn,
        });
    }
}
