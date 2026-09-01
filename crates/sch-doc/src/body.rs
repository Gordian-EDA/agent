//! Symbol body extents: the drawn outline of a placed symbol, in sheet mm.
//!
//! Read from the *embedded* definition's graphics, so it is the shape KiCAD
//! actually renders. A definition that draws nothing — a power flag, a net tie
//! — falls back to the box its pins span, which is the only extent it has.

use geom::{Point2, Rect};
use kiutils_sexpr::Node;

use crate::doc::SchDoc;
use crate::model::SymbolInst;
use crate::pins::{lib_pins, to_sheet};
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
/// `None` when the definition is not embedded — pin geometry is unknown there
/// too, so there is nothing to bound.
pub fn body_rect(doc: &SchDoc, inst: &SymbolInst) -> Option<Rect> {
    let def = doc
        .lib_symbols()
        .and_then(|libs| crate::pins::resolve(libs, &inst.lib_id))?;
    let mut local = Vec::new();
    graphic_points(def, &mut local);
    for sub in items(def) {
        if sexpr::head(sub) == Some("symbol") {
            graphic_points(sub, &mut local);
        }
    }
    if local.is_empty() {
        local.extend(lib_pins(def).iter().map(|p| p.at.point()));
        local.push(Point2::new(0.0, 0.0));
    }
    let sheet: Vec<Point2> = local
        .into_iter()
        .map(|p| to_sheet(p, inst.at, inst.mirror))
        .collect();
    Rect::bounding(&sheet)
}

/// Every placed symbol's body, paired with its reference designator.
pub fn body_rects(doc: &SchDoc) -> Vec<(String, Rect)> {
    doc.symbols()
        .filter_map(|s| Some((s.refdes().to_string(), body_rect(doc, s)?)))
        .collect()
}
