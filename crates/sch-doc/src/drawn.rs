//! Every text the sheet DRAWS, boxed by the one as-drawn model.
//!
//! [`drawn_texts`] is the item set: visible symbol fields (including a power
//! symbol's `#`-prefixed Reference and its rail-name Value), local labels,
//! global and hierarchical labels, free notes, and the name/number KiCAD
//! prints beside every pin. Boxing them all through
//! [`sch_model::text`] is what lets a readability lint report exactly what a
//! render shows.

use kiutils_sexpr::Node;
use sch_model::text::{DrawnText, HJust, TextKind, VJust};

use crate::doc::SchDoc;
use crate::model::{Item, LabelKind, SymbolInst};
use crate::pins;
use crate::sexpr::{self, child, items};

/// Font size and justification from a node's `(effects …)`, with KiCAD's
/// defaults: 1.27 mm, and centred both ways when a token is absent.
fn effects(node: &Node) -> (f64, HJust, VJust, bool) {
    let Some(effects) = child(node, "effects") else {
        return (1.27, HJust::Center, VJust::Center, false);
    };
    let size = child(effects, "font")
        .and_then(|font| child(font, "size"))
        .and_then(|size| items(size).get(1).and_then(sexpr::number))
        .unwrap_or(1.27);
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
        let (size, hjust, vjust, hidden) = effects(field.node());
        if hidden {
            continue;
        }
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
    for pin in pins::drawn_pins(doc, symbol) {
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
                let bbox = match label.kind {
                    LabelKind::Local => sch_model::text::local_label_box(
                        &label.text,
                        size,
                        hjust,
                        vjust,
                        angle,
                        label.at.point(),
                    ),
                    _ => sch_model::text::port_label_text_box(
                        &label.text,
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
                    text: label.text.clone(),
                    bbox,
                });
            }
            Item::Text(text) => {
                let (size, hjust, vjust, hidden) = effects(text.raw.node());
                if hidden || text.text.is_empty() {
                    continue;
                }
                out.push(DrawnText {
                    owner: None,
                    kind: TextKind::FreeText,
                    text: text.text.clone(),
                    bbox: sch_model::text::drawn_box(
                        &text.text,
                        size,
                        hjust,
                        vjust,
                        text.at.rot.rem_euclid(180.0),
                        text.at.point(),
                    ),
                });
            }
            _ => {}
        }
    }
    out
}
