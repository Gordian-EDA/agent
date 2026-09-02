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
    /// UUID of the symbol this pin belongs to. Unlike a reference designator,
    /// this is unique even before the schematic is annotated.
    pub owner: String,
    pub refdes: String,
    pub unit: u32,
    /// Whether the definition splits into several units, which is what makes
    /// KiCAD qualify the reference with a unit letter (`U1A`).
    pub multi_unit: bool,
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
    /// Unit vector along which a wire leaves this pin, in sheet coordinates
    /// (y grows downward) — the direction pointing away from the symbol body.
    pub out: Point2,
}

/// Sub-symbol names inside a definition end in `_<unit>_<style>`.
pub(crate) fn unit_and_style(name: &str) -> (u32, u32) {
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
            // A pin declared straight on the definition belongs to no
            // particular unit or body style, so it belongs to every one.
            Some("pin") => pins.extend(decode_pin(child, 0, 0)),
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

/// How many units a definition draws.
///
/// Read from the sub-symbol names rather than from the pins: a unit that
/// carries only graphics still counts, and a part with two units is written
/// `U1A`/`U1B` whether or not both have pins.
pub(crate) fn unit_count(def: &Node) -> u32 {
    items(def)
        .iter()
        .filter(|child| sexpr::head(child) == Some("symbol"))
        .filter_map(|child| items(child).get(1).and_then(sexpr::text))
        .map(|name| unit_and_style(name).0)
        .max()
        .map_or(1, |units| units.max(1))
}

/// Whether a `lib_symbols` definition is a power symbol.
pub(crate) fn is_power_definition(def: &Node) -> bool {
    items(def).iter().any(|c| sexpr::head(c) == Some("power"))
}

/// Map a symbol-space point onto the sheet through an instance's pose.
///
/// Rotation happens in symbol space (with the y flip into sheet space); the
/// mirror is then a reflection of the *sheet* offset, which is why `(mirror x)`
/// on a rotated symbol is not the same as negating a local coordinate. `x`
/// reflects across the sheet's x axis, `y` across its y axis.
pub(crate) fn to_sheet(local: Point2, at: Pose, mirror: Mirror) -> Point2 {
    let d = to_sheet_dir(local, at, mirror);
    Point2::new(at.x + d.x, at.y + d.y)
}

/// The sheet-space image of a symbol-space *offset* — [`to_sheet`] without the
/// instance's translation, which is what a direction needs.
pub(crate) fn to_sheet_dir(local: Point2, at: Pose, mirror: Mirror) -> Point2 {
    let offset = local.transform_offset(at.rot, false);
    let (dx, dy) = match mirror {
        Mirror::None => (offset.x, offset.y),
        Mirror::X => (offset.x, -offset.y),
        Mirror::Y => (-offset.x, offset.y),
    };
    Point2::new(dx, dy)
}

/// A pin's outward unit vector in sheet space. A library pin's angle points
/// *into* the body, so a wire leaves along its opposite.
fn out_dir(pin: &LibPin, at: Pose, mirror: Mirror) -> Point2 {
    let (s, c) = (pin.at.rot + 180.0).to_radians().sin_cos();
    to_sheet_dir(Point2::new(c, s), at, mirror)
}

/// The instance's body style; KiCAD defaults to the first. KiCAD 7 and earlier
/// spelled the field `convert`, and the corpus still holds pre-8 files.
pub(crate) fn body_style(inst: &SymbolInst) -> u32 {
    let node = inst.retained().node();
    sexpr::child_text(node, "body_style")
        .or_else(|| sexpr::child_text(node, "convert"))
        .and_then(|s| s.parse().ok())
        .unwrap_or(1)
}

/// Whether a definition pin is drawn for this unit and body style. Zero means
/// "shared by all", which is how KiCAD marks a multi-unit part's common pins.
pub(crate) fn belongs(pin: &LibPin, unit: u32, style: u32) -> bool {
    (pin.unit == 0 || pin.unit == unit) && (pin.style == 0 || pin.style == style)
}

/// The pin numbers a placed unit draws, in definition order and deduplicated.
pub(crate) fn pin_numbers(def: &Node, unit: u32, style: u32) -> Vec<String> {
    let mut seen = Vec::new();
    for pin in lib_pins(def) {
        if belongs(&pin, unit, style) && !seen.contains(&pin.number) {
            seen.push(pin.number);
        }
    }
    seen
}

/// Resolve one instance's pins into sheet coordinates.
///
/// Only pins of the instance's own unit and body style are placed, plus the
/// unit-0 / style-0 pins every unit shares.
pub(crate) fn pins_of(doc: &SchDoc, inst: &SymbolInst) -> Vec<PlacedPin> {
    let Some(def) = doc
        .lib_symbols()
        .and_then(|libs| resolve(libs, lib_key(inst)))
    else {
        return Vec::new();
    };
    let power_symbol = is_power_definition(def);
    let style = body_style(inst);
    let units = unit_count(def);
    // KiCAD clamps an instance whose unit the definition does not have; taking
    // it at its word would leave the symbol with no pins and say nothing.
    let unit = inst.unit.clamp(1, units);
    lib_pins(def)
        .into_iter()
        .filter(|p| belongs(p, unit, style))
        .map(|p| {
            let out = out_dir(&p, inst.at, inst.mirror);
            PlacedPin {
                owner: inst.uuid.clone(),
                refdes: inst.refdes().to_string(),
                unit,
                multi_unit: units > 1,
                number: p.number,
                name: p.name,
                etype: p.etype,
                hidden: p.hidden,
                dnp: inst.dnp,
                power_symbol,
                at: to_sheet(p.at.point(), inst.at, inst.mirror),
                out,
            }
        })
        .collect()
}

/// The `lib_symbols` entry an instance draws from.
///
/// KiCAD writes `(lib_name …)` when the sheet carries its own edited copy of a
/// symbol: the definition is filed under that name, and the `lib_id` only says
/// where it came from. Reading the `lib_id` there finds nothing, and a symbol
/// with no definition has no pins and no body.
pub(crate) fn lib_key(inst: &SymbolInst) -> &str {
    sexpr::child_text(inst.retained().node(), "lib_name").unwrap_or(&inst.lib_id)
}

/// Follow `(extends …)` to the definition that actually carries the geometry.
///
/// A derived symbol has no body of its own, so an unresolved chain is a miss,
/// not a definition — returning the `extends` node would hand back a symbol
/// with no pins as if it were the real one.
pub(crate) fn resolve<'a>(libs: &'a crate::model::LibSymbols, key: &str) -> Option<&'a Node> {
    let lib = key.split_once(':').map(|(lib, _)| lib);
    let mut def = libs.get(key)?;
    for _ in 0..8 {
        let Some(parent) = child_text(def, "extends") else {
            return Some(def);
        };
        def = match lib {
            Some(lib) => libs.get(&format!("{lib}:{parent}"))?,
            None => libs.get(parent)?,
        };
    }
    None
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
        assert_eq!(
            to_sheet(Point2::new(2.54, 0.0), at, Mirror::Y),
            Point2::new(97.46, 50.0)
        );
        assert_eq!(to_sheet(p, at, Mirror::X), Point2::new(100.0, 53.81));
    }

    /// The mirror reflects the sheet offset, so on a symbol rotated 90 degrees
    /// `(mirror x)` moves a pin the opposite way from mirroring its local
    /// coordinate would.
    #[test]
    fn mirror_applies_after_rotation() {
        let at = Pose::new(0.0, 0.0, 90.0);
        let p = Point2::new(0.0, 3.81);
        let close = |got: Point2, want: Point2| {
            assert!(got.near_eq(want, 1e-9), "got {got:?}, want {want:?}");
        };
        close(to_sheet(p, at, Mirror::None), Point2::new(-3.81, 0.0));
        close(to_sheet(p, at, Mirror::X), Point2::new(-3.81, 0.0));
        close(to_sheet(p, at, Mirror::Y), Point2::new(3.81, 0.0));
    }

    #[test]
    fn rotation_is_counter_clockwise_in_symbol_space() {
        let at = Pose::new(0.0, 0.0, 90.0);
        let got = to_sheet(Point2::new(0.0, 3.81), at, Mirror::None);
        assert!(got.near_eq(Point2::new(-3.81, 0.0), 1e-9), "{got:?}");
    }
}
