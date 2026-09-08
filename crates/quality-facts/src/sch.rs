//! The parts of a `.kicad_sch` the quality harness measures.
//!
//! Everything is read from the file itself, embedded `lib_symbols` included, so
//! a sheet is measurable without a symbol library on disk.

use std::collections::BTreeMap;

use crate::geom::{Mirror, Point, Rect};
use crate::sexp::{Sexp, parse};

#[derive(Debug, Clone)]
pub struct LibPin {
    pub at: Point,
    pub rot: f64,
    pub length: f64,
    pub name: String,
    pub number: String,
    pub etype: String,
    pub hidden: bool,
    pub unit: u32,
    pub style: u32,
}

#[derive(Debug, Clone, Default)]
pub struct LibSymbol {
    pub extends: Option<String>,
    pub is_power: bool,
    pub unit_count: u32,
    pub pins: Vec<LibPin>,
    /// Graphic corner points per `(unit, style)`; unit or style `0` is shared.
    pub graphics: Vec<(u32, u32, Vec<Point>)>,
}

#[derive(Debug, Clone)]
pub struct Field {
    pub name: String,
    pub value: String,
    pub at: Point,
    pub rot: f64,
    pub hidden: bool,
    pub size: f64,
    pub justify: Justify,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Justify {
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone)]
pub struct SymbolInst {
    pub lib_id: String,
    /// The `lib_symbols` key: `lib_name` when the sheet carries its own copy.
    pub lib_key: String,
    pub uuid: String,
    pub at: Point,
    pub rot: f64,
    pub mirror: Mirror,
    pub unit: u32,
    pub style: u32,
    pub dnp: bool,
    pub fields: Vec<Field>,
}

impl SymbolInst {
    pub fn refdes(&self) -> &str {
        self.field("Reference")
    }

    pub fn value(&self) -> &str {
        self.field("Value")
    }

    pub fn field(&self, name: &str) -> &str {
        self.fields
            .iter()
            .find(|field| field.name == name)
            .map_or("", |field| field.value.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelKind {
    Local,
    Global,
    Hier,
}

#[derive(Debug, Clone)]
pub struct Label {
    pub at: Point,
    pub rot: f64,
    pub text: String,
    pub kind: LabelKind,
    pub size: f64,
}

#[derive(Debug, Clone)]
pub struct FreeText {
    pub at: Point,
    pub rot: f64,
    pub text: String,
    pub size: f64,
}

#[derive(Debug, Clone)]
pub struct SheetPin {
    pub at: Point,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct SubSheet {
    pub at: Point,
    pub size: Point,
    pub pins: Vec<SheetPin>,
}

#[derive(Debug, Clone, Default)]
pub struct Schematic {
    pub lib_symbols: BTreeMap<String, LibSymbol>,
    pub symbols: Vec<SymbolInst>,
    pub wires: Vec<[Point; 2]>,
    pub junctions: Vec<Point>,
    pub no_connects: Vec<Point>,
    pub labels: Vec<Label>,
    pub texts: Vec<FreeText>,
    pub sheets: Vec<SubSheet>,
    pub has_bus: bool,
}

/// One placed pin, in sheet coordinates.
#[derive(Debug, Clone)]
pub struct PlacedPin {
    pub owner: String,
    pub refdes: String,
    pub unit: u32,
    pub multi_unit: bool,
    pub number: String,
    pub name: String,
    pub etype: String,
    pub hidden: bool,
    pub dnp: bool,
    pub power_symbol: bool,
    pub at: Point,
}

impl Schematic {
    pub fn read(path: &std::path::Path) -> Result<Schematic, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{e}"))?;
        Schematic::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Schematic, String> {
        let root = parse(text)?;
        if root.head() != Some("kicad_sch") {
            return Err("not a kicad_sch document".into());
        }
        let mut doc = Schematic::default();
        if let Some(libs) = root.child("lib_symbols") {
            for def in libs.children("symbol") {
                let Some(name) = def.items().get(1).and_then(Sexp::as_atom) else {
                    continue;
                };
                doc.lib_symbols
                    .insert(name.to_string(), read_definition(def));
            }
        }
        for item in root.items() {
            match item.head() {
                Some("symbol") => doc.symbols.push(read_instance(item)),
                Some("wire") | Some("bus") => {
                    if item.head() == Some("bus") {
                        doc.has_bus = true;
                    }
                    let points = read_points(item);
                    for pair in points.windows(2) {
                        doc.wires.push([pair[0], pair[1]]);
                    }
                }
                Some("bus_entry") | Some("bus_alias") => doc.has_bus = true,
                Some("junction") => doc.junctions.push(read_at(item).0),
                Some("no_connect") => doc.no_connects.push(read_at(item).0),
                Some("label") => doc.labels.push(read_label(item, LabelKind::Local)),
                Some("global_label") => doc.labels.push(read_label(item, LabelKind::Global)),
                Some("hierarchical_label") => doc.labels.push(read_label(item, LabelKind::Hier)),
                Some("text") => {
                    let (at, rot) = read_at(item);
                    doc.texts.push(FreeText {
                        at,
                        rot,
                        text: item
                            .items()
                            .get(1)
                            .and_then(Sexp::as_atom)
                            .unwrap_or("")
                            .into(),
                        size: font_size(item),
                    });
                }
                Some("sheet") => doc.sheets.push(read_sheet(item)),
                _ => {}
            }
        }
        Ok(doc)
    }

    /// Follow `(extends …)` to the definition that carries the geometry.
    pub fn definition(&self, key: &str) -> Option<&LibSymbol> {
        let library = key.split_once(':').map(|(lib, _)| lib);
        let mut def = self.lib_symbols.get(key)?;
        for _ in 0..8 {
            let Some(parent) = def.extends.as_deref() else {
                return Some(def);
            };
            let full = match library {
                Some(lib) => format!("{lib}:{parent}"),
                None => parent.to_string(),
            };
            def = self.lib_symbols.get(&full)?;
        }
        None
    }

    /// Every pin of one placed symbol, in sheet coordinates.
    pub fn pins_of(&self, inst: &SymbolInst) -> Vec<PlacedPin> {
        let Some(def) = self.definition(&inst.lib_key) else {
            return Vec::new();
        };
        let units = def.unit_count.max(1);
        let unit = inst.unit.clamp(1, units);
        def.pins
            .iter()
            .filter(|pin| belongs(pin, unit, inst.style))
            .map(|pin| PlacedPin {
                owner: inst.uuid.clone(),
                refdes: inst.refdes().to_string(),
                unit,
                multi_unit: units > 1,
                number: pin.number.clone(),
                name: pin.name.clone(),
                etype: pin.etype.clone(),
                hidden: pin.hidden,
                dnp: inst.dnp,
                power_symbol: def.is_power,
                at: pin.at.to_sheet(inst.at, inst.rot, inst.mirror),
            })
            .collect()
    }

    pub fn placed_pins(&self) -> Vec<PlacedPin> {
        self.symbols.iter().flat_map(|s| self.pins_of(s)).collect()
    }

    /// The box one placed symbol draws, falling back to the box its pins span.
    pub fn body_rect(&self, inst: &SymbolInst) -> Option<Rect> {
        let def = self.definition(&inst.lib_key)?;
        let unit = inst.unit.clamp(1, def.unit_count.max(1));
        let mut local: Vec<Point> = def
            .graphics
            .iter()
            .filter(|(u, s, _)| (*u == 0 || *u == unit) && (*s == 0 || *s == inst.style))
            .flat_map(|(_, _, points)| points.iter().copied())
            .collect();
        if local.is_empty() {
            local.extend(
                def.pins
                    .iter()
                    .filter(|pin| belongs(pin, unit, inst.style))
                    .map(|pin| pin.at),
            );
            local.push(Point::new(0.0, 0.0));
        }
        let sheet: Vec<Point> = local
            .into_iter()
            .map(|p| p.to_sheet(inst.at, inst.rot, inst.mirror))
            .collect();
        Rect::bounding(&sheet)
    }
}

/// Whether a definition pin is drawn for this unit and body style; `0` is shared.
pub fn belongs(pin: &LibPin, unit: u32, style: u32) -> bool {
    (pin.unit == 0 || pin.unit == unit) && (pin.style == 0 || pin.style == style)
}

fn read_definition(def: &Sexp) -> LibSymbol {
    let mut symbol = LibSymbol {
        extends: def.text("extends").map(str::to_string),
        is_power: def.child("power").is_some(),
        ..LibSymbol::default()
    };
    graphic_points(def, 0, 0, &mut symbol.graphics);
    for sub in def.children("symbol") {
        let (unit, style) = sub
            .items()
            .get(1)
            .and_then(Sexp::as_atom)
            .map_or((1, 1), unit_and_style);
        symbol.unit_count = symbol.unit_count.max(unit);
        graphic_points(sub, unit, style, &mut symbol.graphics);
        for pin in sub.children("pin") {
            symbol.pins.push(read_lib_pin(pin, unit, style));
        }
    }
    for pin in def.children("pin") {
        symbol.pins.push(read_lib_pin(pin, 0, 0));
    }
    symbol.unit_count = symbol.unit_count.max(1);
    symbol
}

fn read_lib_pin(pin: &Sexp, unit: u32, style: u32) -> LibPin {
    let (at, rot) = read_at(pin);
    LibPin {
        at,
        rot,
        length: pin.child("length").and_then(|n| n.number(1)).unwrap_or(0.0),
        name: pin.text("name").unwrap_or("").to_string(),
        number: pin.text("number").unwrap_or("").to_string(),
        etype: pin
            .items()
            .get(1)
            .and_then(Sexp::as_atom)
            .unwrap_or("")
            .into(),
        hidden: is_hidden(pin),
        unit,
        style,
    }
}

/// `R_0_1` -> unit 0, style 1; a name without the suffix is unit 1, style 1.
pub fn unit_and_style(name: &str) -> (u32, u32) {
    let mut parts = name.rsplitn(3, '_');
    let style = parts.next().and_then(|p| p.parse().ok());
    let unit = parts.next().and_then(|p| p.parse().ok());
    match (unit, style) {
        (Some(unit), Some(style)) => (unit, style),
        _ => (1, 1),
    }
}

fn graphic_points(node: &Sexp, unit: u32, style: u32, out: &mut Vec<(u32, u32, Vec<Point>)>) {
    let mut points = Vec::new();
    for child in node.items() {
        match child.head() {
            Some("rectangle" | "arc" | "bezier" | "polyline") => {
                for tag in ["start", "mid", "end"] {
                    if let Some(node) = child.child(tag) {
                        points.push(Point::new(
                            node.number(1).unwrap_or(0.0),
                            node.number(2).unwrap_or(0.0),
                        ));
                    }
                }
                if let Some(pts) = child.child("pts") {
                    points.extend(pts.children("xy").map(|xy| {
                        Point::new(xy.number(1).unwrap_or(0.0), xy.number(2).unwrap_or(0.0))
                    }));
                }
            }
            Some("circle") => {
                let centre = child.child("center");
                let radius = child.child("radius").and_then(|n| n.number(1));
                if let (Some(centre), Some(radius)) = (centre, radius) {
                    let (x, y) = (
                        centre.number(1).unwrap_or(0.0),
                        centre.number(2).unwrap_or(0.0),
                    );
                    points.push(Point::new(x - radius, y - radius));
                    points.push(Point::new(x + radius, y + radius));
                }
            }
            _ => {}
        }
    }
    if !points.is_empty() {
        out.push((unit, style, points));
    }
}

fn read_instance(node: &Sexp) -> SymbolInst {
    let (at, rot) = read_at(node);
    let lib_id = node.text("lib_id").unwrap_or("").to_string();
    let lib_key = node.text("lib_name").unwrap_or(&lib_id).to_string();
    SymbolInst {
        lib_id,
        lib_key,
        uuid: node.text("uuid").unwrap_or("").to_string(),
        at,
        rot,
        mirror: match node.text("mirror") {
            Some("x") => Mirror::X,
            Some("y") => Mirror::Y,
            _ => Mirror::None,
        },
        unit: node.child("unit").and_then(|n| n.number(1)).unwrap_or(1.0) as u32,
        style: node
            .child("body_style")
            .or_else(|| node.child("convert"))
            .and_then(|n| n.number(1))
            .unwrap_or(1.0) as u32,
        dnp: node.flag("dnp").unwrap_or(false),
        fields: node.children("property").map(read_field).collect(),
    }
}

fn read_field(node: &Sexp) -> Field {
    let (at, rot) = read_at(node);
    Field {
        name: node
            .items()
            .get(1)
            .and_then(Sexp::as_atom)
            .unwrap_or("")
            .into(),
        value: node
            .items()
            .get(2)
            .and_then(Sexp::as_atom)
            .unwrap_or("")
            .into(),
        at,
        rot,
        hidden: is_hidden(node),
        size: font_size(node),
        justify: justify(node),
    }
}

fn read_label(node: &Sexp, kind: LabelKind) -> Label {
    let (at, rot) = read_at(node);
    Label {
        at,
        rot,
        text: node
            .items()
            .get(1)
            .and_then(Sexp::as_atom)
            .unwrap_or("")
            .into(),
        kind,
        size: font_size(node),
    }
}

fn read_sheet(node: &Sexp) -> SubSheet {
    let (at, _) = read_at(node);
    let size = node
        .child("size")
        .map(|n| Point::new(n.number(1).unwrap_or(0.0), n.number(2).unwrap_or(0.0)))
        .unwrap_or(Point::new(0.0, 0.0));
    SubSheet {
        at,
        size,
        pins: node
            .children("pin")
            .map(|pin| SheetPin {
                at: read_at(pin).0,
                name: pin
                    .items()
                    .get(1)
                    .and_then(Sexp::as_atom)
                    .unwrap_or("")
                    .into(),
            })
            .collect(),
    }
}

fn read_points(node: &Sexp) -> Vec<Point> {
    node.child("pts")
        .map(|pts| {
            pts.children("xy")
                .map(|xy| Point::new(xy.number(1).unwrap_or(0.0), xy.number(2).unwrap_or(0.0)))
                .collect()
        })
        .unwrap_or_default()
}

fn read_at(node: &Sexp) -> (Point, f64) {
    match node.child("at") {
        Some(at) => (
            Point::new(at.number(1).unwrap_or(0.0), at.number(2).unwrap_or(0.0)),
            at.number(3).unwrap_or(0.0),
        ),
        None => (Point::new(0.0, 0.0), 0.0),
    }
}

/// KiCad writes `(hide yes)` today and a bare `hide` in older files, either on
/// the node itself or inside its `effects`.
fn is_hidden(node: &Sexp) -> bool {
    let here = |n: &Sexp| {
        n.flag("hide").unwrap_or(false) || n.items().iter().any(|i| i.as_atom() == Some("hide"))
    };
    here(node) || node.child("effects").is_some_and(here)
}

fn font_size(node: &Sexp) -> f64 {
    node.child("effects")
        .and_then(|e| e.child("font"))
        .and_then(|f| f.child("size"))
        .and_then(|s| s.number(2))
        .unwrap_or(1.27)
}

fn justify(node: &Sexp) -> Justify {
    let words = node
        .child("effects")
        .and_then(|e| e.child("justify"))
        .map(|j| {
            j.items()
                .iter()
                .filter_map(Sexp::as_atom)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if words.contains(&"left") {
        Justify::Left
    } else if words.contains(&"right") {
        Justify::Right
    } else {
        Justify::Center
    }
}

/// KiCad escapes `{`, `}` and `~` in label and pin text; the netlist carries the
/// plain form.
pub fn unescape(text: &str) -> String {
    text.replace("{slash}", "/")
        .replace("{tab}", "\t")
        .replace("{space}", " ")
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
}
