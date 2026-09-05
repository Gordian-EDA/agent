//! Symbol body extents: the drawn outline of a placed symbol, in sheet mm.
//!
//! Read from the *embedded* definition's graphics, so it is the shape KiCAD
//! actually renders. A definition that draws nothing — a power flag, a net tie
//! — falls back to the box its pins span, which is the only extent it has.

use geom::{Point2, Rect};
use kiutils_sexpr::Node;

use crate::doc::SchDoc;
use crate::model::SymbolInst;
use crate::pins::{belongs, body_style, lib_key, lib_pins, to_sheet, unit_and_style, unit_count};
use crate::sexpr::{self, items};

/// Read an `(x y)` pair starting at `at` inside a node's item list.
fn xy(node: &Node, at: usize) -> Option<Point2> {
    let v = items(node);
    Some(Point2::new(
        sexpr::number(v.get(at)?)?,
        sexpr::number(v.get(at + 1)?)?,
    ))
}

/// The corners of every graphic primitive in a definition block, in symbol
/// coordinates. Curves contribute their control points, which bound the curves
/// KiCAD symbols draw.
fn graphic_points(node: &Node, out: &mut Vec<Point2>) {
    for child in items(node) {
        match sexpr::head(child) {
            Some("rectangle" | "arc" | "bezier" | "polyline") => {
                for tag in ["start", "mid", "end"] {
                    if let Some(p) = sexpr::child(child, tag).and_then(|n| xy(n, 1)) {
                        out.push(p);
                    }
                }
                if let Some(pts) = sexpr::child(child, "pts") {
                    out.extend(items(pts).iter().filter_map(|p| xy(p, 1)));
                }
            }
            Some("circle") => {
                let centre = sexpr::child(child, "center").and_then(|n| xy(n, 1));
                let radius = sexpr::child_text(child, "radius").and_then(|r| r.parse::<f64>().ok());
                if let (Some(c), Some(r)) = (centre, radius) {
                    out.push(Point2::new(c.x - r, c.y - r));
                    out.push(Point2::new(c.x + r, c.y + r));
                }
            }
            _ => {}
        }
    }
}

/// The body a placed symbol draws, in sheet coordinates.
///
/// Only the graphics of the instance's own unit and body style count: a
/// multi-unit part draws one unit per placement, and unioning them all would
/// hand back a box the size of the sheet.
///
/// `None` when the definition is not embedded — pin geometry is unknown there
/// too, so there is nothing to bound.
pub fn body_rect(doc: &SchDoc, inst: &SymbolInst) -> Option<Rect> {
    let def = doc
        .lib_symbols()
        .and_then(|libs| crate::pins::resolve(libs, lib_key(inst)))?;
    let style = body_style(inst);
    let unit = inst.unit.clamp(1, unit_count(def));
    let mut local = Vec::new();
    graphic_points(def, &mut local);
    for sub in items(def) {
        if sexpr::head(sub) != Some("symbol") {
            continue;
        }
        let (u, s) = items(sub)
            .get(1)
            .and_then(sexpr::text)
            .map_or((1, 1), unit_and_style);
        if (u == 0 || u == unit) && (s == 0 || s == style) {
            graphic_points(sub, &mut local);
        }
    }
    if local.is_empty() {
        local.extend(
            lib_pins(def)
                .iter()
                .filter(|p| belongs(p, unit, style))
                .map(|p| p.at.point()),
        );
        local.push(Point2::new(0.0, 0.0));
    }
    let sheet: Vec<Point2> = local
        .into_iter()
        .map(|p| to_sheet(p, inst.at, inst.mirror))
        .collect();
    Rect::bounding(&sheet)
}

/// The box one UNIT of a `(symbol …)` DEFINITION draws, in symbol coordinates.
///
/// [`body_rect`] split at the point where the shape stops depending on the
/// placement: a caller holding a library definition but no document — the
/// schematic writer, seating text against what it is about to draw — parses it
/// here and poses the result itself. Falling back to the unit's pin points
/// matches `body_rect`, so the two agree symbol for symbol.
///
/// `None` when the text does not parse as one `(symbol …)` block.
pub fn definition_unit_box(definition: &str, unit: u8) -> Option<Rect> {
    let cst = kiutils_sexpr::parse_one(definition).ok()?;
    let def = cst.nodes.first()?;
    let unit = u32::from(unit.max(1)).min(unit_count(def));
    let mut local = Vec::new();
    graphic_points(def, &mut local);
    for sub in items(def) {
        if sexpr::head(sub) != Some("symbol") {
            continue;
        }
        let (u, s) = items(sub)
            .get(1)
            .and_then(sexpr::text)
            .map_or((1, 1), unit_and_style);
        if (u == 0 || u == unit) && s <= 1 {
            graphic_points(sub, &mut local);
        }
    }
    if local.is_empty() {
        local.extend(
            lib_pins(def)
                .iter()
                .filter(|p| belongs(p, unit, 1))
                .map(|p| p.at.point()),
        );
        local.push(Point2::new(0.0, 0.0));
    }
    Rect::bounding(&local)
}

/// Every placed symbol's body, paired with its reference designator.
pub fn body_rects(doc: &SchDoc) -> Vec<(String, Rect)> {
    doc.symbols()
        .filter_map(|s| Some((s.refdes().to_string(), body_rect(doc, s)?)))
        .collect()
}
