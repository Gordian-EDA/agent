//! Pin geometry: from the *embedded* `lib_symbols` definition — never the
//! installed library — through the instance transform into sheet coordinates.

use geom::Point2;
use kiutils_sexpr::Node;

use crate::doc::SchDoc;
use crate::model::{Mirror, Pose, SymbolInst};
use crate::sexpr::{self, child, child_text, items};

/// A pin as drawn inside a `lib_symbols` definition, in symbol coordinates
/// (y grows upward).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LibPin {
    pub number: String,
    pub name: String,
    /// Connection point — the tip a wire attaches to. The pin line runs from
    /// here *into* the body, so no length projection is applied.
    pub at: Pose,
    pub etype: String,
    pub hidden: bool,
    /// Unit this pin belongs to; `0` means common to every unit.
    pub unit: u32,
    /// Body style this pin belongs to; `0` means common to every style.
    pub style: u32,
}

/// A pin of a placed symbol, resolved into sheet coordinates.
#[derive(Debug, Clone, PartialEq)]
pub struct PlacedPin {
    pub refdes: String,
    pub unit: u32,
    pub number: String,
    pub name: String,
    pub etype: String,
    pub hidden: bool,
    /// Whether the owning symbol is marked do-not-populate. DNP symbols still
    /// connect; consumers decide what that means for them.
    pub dnp: bool,
    /// Whether the owning symbol's definition carries the `(power)` marker.
    pub power_symbol: bool,
    pub at: Point2,
}

/// Sub-symbol names inside a definition end in `_<unit>_<style>`.
fn unit_and_style(name: &str) -> (u32, u32) {
    let mut parts = name.rsplitn(3, '_');
    let style = parts.next().and_then(|s| s.parse().ok());
    let unit = parts.next().and_then(|s| s.parse().ok());
    match (unit, style) {
        (Some(u), Some(s)) => (u, s),
        _ => (1, 1),
    }
}

fn decode_pin(node: &Node, unit: u32, style: u32) -> Option<LibPin> {
    let at = child(node, "at")?;
    let number = child_text(node, "number")?.to_string();
    Some(LibPin {
        number,
        name: child_text(node, "name").unwrap_or("~").to_string(),
        at: Pose::new(
            items(at).get(1).and_then(sexpr::number).unwrap_or_default(),
            items(at).get(2).and_then(sexpr::number).unwrap_or_default(),
            items(at).get(3).and_then(sexpr::number).unwrap_or_default(),
        ),
        etype: items(node)
            .get(1)
            .and_then(sexpr::text)
            .unwrap_or("passive")
            .to_string(),
        hidden: sexpr::flag_present(node, "hide"),
        unit,
        style,
    })
}

/// Every pin declared by a `lib_symbols` definition, across all units.
pub(crate) fn lib_pins(def: &Node) -> Vec<LibPin> {
    let mut pins = Vec::new();
    for child in items(def) {
        match sexpr::head(child) {
            Some("pin") => pins.extend(decode_pin(child, 1, 1)),
            Some("symbol") => {
                let (unit, style) = items(child)
                    .get(1)
                    .and_then(sexpr::text)
                    .map(unit_and_style)
                    .unwrap_or((1, 1));
                for grandchild in items(child) {
                    if sexpr::head(grandchild) == Some("pin") {
                        pins.extend(decode_pin(grandchild, unit, style));
                    }
                }
            }
            _ => {}
        }
    }
    pins
}

/// Whether a `lib_symbols` definition is a power symbol.
pub(crate) fn is_power_definition(def: &Node) -> bool {
    items(def).iter().any(|c| sexpr::head(c) == Some("power"))
}

/// Map a symbol-space point onto the sheet through an instance's pose.
///
/// KiCAD applies the mirror first and the rotation second (the parser sets the
/// orientation from `(at … angle)` and then right-multiplies the mirror), then
/// flips y because symbol space grows upward and the sheet grows downward.
pub(crate) fn to_sheet(local: Point2, at: Pose, mirror: Mirror) -> Point2 {
    let local = match mirror {
        Mirror::X => Point2::new(local.x, -local.y),
        Mirror::None | Mirror::Y => local,
    };
    let offset = local.transform_offset(at.rot, mirror == Mirror::Y);
    Point2::new(at.x + offset.x, at.y + offset.y)
}

/// The instance's body style; KiCAD defaults to the first.
fn body_style(inst: &SymbolInst) -> u32 {
    sexpr::child_text(&inst.raw.node, "body_style")
        .or_else(|| sexpr::child_text(&inst.raw.node, "convert"))
        .and_then(|s| s.parse().ok())
        .unwrap_or(1)
}

/// Resolve one instance's pins into sheet coordinates.
///
/// Only pins of the instance's own unit and body style are placed, plus the
/// unit-0 / style-0 pins every unit shares.
pub(crate) fn pins_of(doc: &SchDoc, inst: &SymbolInst) -> Vec<PlacedPin> {
    let Some(def) = doc.lib_symbols().and_then(|libs| resolve(libs, &inst.lib_id)) else {
        return Vec::new();
    };
    let power_symbol = is_power_definition(def);
    let style = body_style(inst);
    lib_pins(def)
        .into_iter()
        .filter(|p| (p.unit == 0 || p.unit == inst.unit) && (p.style == 0 || p.style == style))
        .map(|p| PlacedPin {
            refdes: inst.refdes().to_string(),
            unit: inst.unit,
            number: p.number,
            name: p.name,
            etype: p.etype,
            hidden: p.hidden,
            dnp: inst.dnp,
            power_symbol,
            at: to_sheet(p.at.point(), inst.at, inst.mirror),
        })
        .collect()
}

/// Follow `(extends …)` to the definition that actually carries the geometry.
pub(crate) fn resolve<'a>(libs: &'a crate::model::LibSymbols, lib_id: &str) -> Option<&'a Node> {
    let mut def = libs.get(lib_id)?;
    for _ in 0..8 {
        let Some(parent) = child_text(def, "extends") else {
            return Some(def);
        };
        let lib = lib_id.split_once(':').map(|(l, _)| l).unwrap_or_default();
        def = libs.get(&format!("{lib}:{parent}"))?;
    }
    Some(def)
}

/// Every pin of every placed symbol, in sheet coordinates.
pub fn placed_pins(doc: &SchDoc) -> Vec<PlacedPin> {
    doc.symbols().flat_map(|s| pins_of(doc, s)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sub_symbol_names_carry_unit_and_style() {
        assert_eq!(unit_and_style("R_0_1"), (0, 1));
        assert_eq!(unit_and_style("74LS00_3_2"), (3, 2));
        assert_eq!(unit_and_style("Odd_Name"), (1, 1));
    }

    #[test]
    fn transform_matches_kicad_orientations() {
        let at = Pose::new(100.0, 50.0, 0.0);
        let p = Point2::new(0.0, 3.81);
        assert_eq!(to_sheet(p, at, Mirror::None), Point2::new(100.0, 46.19));
        // Mirror y flips left-to-right: local x negates, y is untouched.
        assert_eq!(
            to_sheet(Point2::new(2.54, 0.0), at, Mirror::Y),
            Point2::new(97.46, 50.0)
        );
        // Mirror x flips top-to-bottom: local y negates before the sheet flip.
        assert_eq!(to_sheet(p, at, Mirror::X), Point2::new(100.0, 53.81));
    }

    #[test]
    fn rotation_is_counter_clockwise_in_symbol_space() {
        let at = Pose::new(0.0, 0.0, 90.0);
        let got = to_sheet(Point2::new(0.0, 3.81), at, Mirror::None);
        assert!((got.x + 3.81).abs() < 1e-9 && got.y.abs() < 1e-9, "{got:?}");
    }
}
