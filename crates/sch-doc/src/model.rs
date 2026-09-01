//! The typed skeleton: exactly the schematic items tools touch, each still
//! carrying the node it was decoded from so that everything else survives.

use geom::Point2;
use indexmap::IndexMap;
use kiutils_sexpr::{Node, Span};

use crate::sexpr::{self, child, child_flag, child_text, items, list, num, quoted, sym, tagged};

/// A CST node held exactly as parsed. `span` points at the bytes it came from
/// and is cleared the moment an edit makes those bytes stale, so "print
/// verbatim" and "re-render from the typed fields" are the same decision.
#[derive(Debug, Clone, PartialEq)]
pub struct Retained {
    pub(crate) node: Node,
    pub(crate) span: Option<Span>,
}

impl Retained {
    /// The node as it stands, whether parsed or synthesized.
    pub fn node(&self) -> &Node {
        &self.node
    }

    pub(crate) fn parsed(node: Node) -> Self {
        let span = match &node {
            Node::List { span, .. } | Node::Atom { span, .. } => Some(*span),
        };
        Self { node, span }
    }

    pub(crate) fn owned(node: Node) -> Self {
        Self { node, span: None }
    }

    pub(crate) fn touch(&mut self) {
        self.span = None;
    }
}

/// Position and rotation of a placed item, in millimetres and degrees.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Pose {
    pub x: f64,
    pub y: f64,
    pub rot: f64,
}

impl Pose {
    pub fn new(x: f64, y: f64, rot: f64) -> Self {
        Self { x, y, rot }
    }

    pub fn point(&self) -> Point2 {
        Point2::new(self.x, self.y)
    }
}

/// A placed symbol's mirroring. `Y` flips left-to-right (negating local x),
/// `X` flips top-to-bottom (negating local y) — KiCAD names each mirror after
/// the axis it reflects across.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mirror {
    #[default]
    None,
    X,
    Y,
}

impl Mirror {
    fn parse(token: &str) -> Self {
        match token {
            "x" => Mirror::X,
            "y" => Mirror::Y,
            _ => Mirror::None,
        }
    }

    fn token(self) -> Option<&'static str> {
        match self {
            Mirror::None => None,
            Mirror::X => Some("x"),
            Mirror::Y => Some("y"),
        }
    }
}

/// A symbol property (`Reference`, `Value`, `Footprint`, user fields …).
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub value: String,
    pub at: Option<Pose>,
    pub hidden: bool,
    pub(crate) raw: Retained,
}

impl Field {
    fn decode(node: &Node) -> Option<(String, Field)> {
        let name = sexpr::text(items(node).get(1)?)?.to_string();
        let value = sexpr::text(items(node).get(2)?).unwrap_or_default().to_string();
        let at = child(node, "at").map(decode_pose);
        let hidden = sexpr::flag_present(node, "hide")
            || child(node, "effects").is_some_and(|e| sexpr::flag_present(e, "hide"));
        Some((
            name,
            Field {
                value,
                at,
                hidden,
                raw: Retained::parsed(node.clone()),
            },
        ))
    }

    fn new(name: &str, value: &str, at: Pose) -> Field {
        Field {
            value: value.to_string(),
            at: Some(at),
            hidden: false,
            raw: Retained::owned(tagged(
                "property",
                vec![quoted(name), quoted(value), encode_pose(at)],
            )),
        }
    }

    fn encode(&self) -> Node {
        let mut node = self.raw.node.clone();
        if let Some(children) = sexpr::items_mut(&mut node)
            && let Some(slot) = children.get_mut(2)
        {
            *slot = quoted(self.value.clone());
        }
        if let Some(at) = self.at {
            sexpr::set_child(&mut node, encode_pose(at));
        }
        set_hide(&mut node, self.hidden);
        node
    }
}

/// A placed symbol instance.
#[derive(Debug, Clone, PartialEq)]
pub struct SymbolInst {
    pub uuid: String,
    pub lib_id: String,
    pub unit: u32,
    pub at: Pose,
    pub mirror: Mirror,
    pub dnp: bool,
    pub in_bom: bool,
    pub on_board: bool,
    pub exclude_from_sim: bool,
    /// Properties in document order, keyed by name.
    pub fields: IndexMap<String, Field>,
    /// Pin number to the UUID KiCAD assigned that pin on this instance.
    pub pin_uuids: IndexMap<String, String>,
    pub(crate) raw: Retained,
}

impl SymbolInst {
    pub(crate) fn decode(node: &Node) -> SymbolInst {
        let mut fields = IndexMap::new();
        let mut pin_uuids = IndexMap::new();
        for child in items(node) {
            match sexpr::head(child) {
                Some("property") => {
                    if let Some((name, field)) = Field::decode(child) {
                        fields.insert(name, field);
                    }
                }
                Some("pin") => {
                    if let (Some(number), Some(uuid)) = (
                        items(child).get(1).and_then(sexpr::text),
                        child_text(child, "uuid"),
                    ) {
                        pin_uuids.insert(number.to_string(), uuid.to_string());
                    }
                }
                _ => {}
            }
        }
        SymbolInst {
            uuid: child_text(node, "uuid").unwrap_or_default().to_string(),
            lib_id: child_text(node, "lib_id").unwrap_or_default().to_string(),
            unit: child_text(node, "unit").and_then(|s| s.parse().ok()).unwrap_or(1),
            at: child(node, "at").map(decode_pose).unwrap_or_default(),
            mirror: child_text(node, "mirror").map(Mirror::parse).unwrap_or_default(),
            dnp: child_flag(node, "dnp").unwrap_or(false),
            in_bom: child_flag(node, "in_bom").unwrap_or(true),
            on_board: child_flag(node, "on_board").unwrap_or(true),
            exclude_from_sim: child_flag(node, "exclude_from_sim").unwrap_or(false),
            fields,
            pin_uuids,
            raw: Retained::parsed(node.clone()),
        }
    }

    /// Reference designator, from the `Reference` property.
    pub fn refdes(&self) -> &str {
        self.fields.get("Reference").map_or("", |f| f.value.as_str())
    }

    /// Value, from the `Value` property.
    pub fn value(&self) -> &str {
        self.fields.get("Value").map_or("", |f| f.value.as_str())
    }

    pub(crate) fn encode(&self) -> Node {
        let mut node = self.raw.node.clone();
        sexpr::set_child(&mut node, tagged("lib_id", vec![quoted(self.lib_id.clone())]));
        sexpr::set_child(&mut node, encode_pose(self.at));
        sexpr::set_child(&mut node, tagged("unit", vec![num(self.unit as f64)]));
        sexpr::set_child(&mut node, tagged("dnp", vec![yes_no(self.dnp)]));
        sexpr::set_child(&mut node, tagged("in_bom", vec![yes_no(self.in_bom)]));
        sexpr::set_child(&mut node, tagged("on_board", vec![yes_no(self.on_board)]));
        sexpr::set_child(
            &mut node,
            tagged("exclude_from_sim", vec![yes_no(self.exclude_from_sim)]),
        );
        sexpr::set_child(&mut node, tagged("uuid", vec![quoted(self.uuid.clone())]));
        match self.mirror.token() {
            Some(axis) => sexpr::set_child(&mut node, tagged("mirror", vec![sym(axis)])),
            None => sexpr::remove_children(&mut node, "mirror"),
        }
        sync_properties(&mut node, &self.fields);
        sync_instance_reference(&mut node, self.refdes(), self.unit);
        node
    }
}

/// A wire segment. KiCAD writes exactly two points; more are tolerated.
#[derive(Debug, Clone, PartialEq)]
pub struct Wire {
    pub uuid: String,
    pub points: Vec<Point2>,
    pub(crate) raw: Retained,
}

impl Wire {
    fn decode(node: &Node) -> Wire {
        Wire {
            uuid: child_text(node, "uuid").unwrap_or_default().to_string(),
            points: decode_pts(node),
            raw: Retained::parsed(node.clone()),
        }
    }

    pub(crate) fn encode(&self) -> Node {
        let mut node = self.raw.node.clone();
        sexpr::set_child(&mut node, encode_pts(&self.points));
        sexpr::set_child(&mut node, tagged("uuid", vec![quoted(self.uuid.clone())]));
        node
    }
}

/// An explicit connection dot.
#[derive(Debug, Clone, PartialEq)]
pub struct Junction {
    pub uuid: String,
    pub at: Point2,
    pub(crate) raw: Retained,
}

/// An intentional "nothing is attached here" marker.
#[derive(Debug, Clone, PartialEq)]
pub struct NoConnect {
    pub uuid: String,
    pub at: Point2,
    pub(crate) raw: Retained,
}

macro_rules! point_item {
    ($ty:ident) => {
        impl $ty {
            fn decode(node: &Node) -> $ty {
                let pose = child(node, "at").map(decode_pose).unwrap_or_default();
                $ty {
                    uuid: child_text(node, "uuid").unwrap_or_default().to_string(),
                    at: pose.point(),
                    raw: Retained::parsed(node.clone()),
                }
            }

            pub(crate) fn encode(&self) -> Node {
                let mut node = self.raw.node.clone();
                sexpr::set_child(
                    &mut node,
                    tagged("at", vec![num(self.at.x), num(self.at.y)]),
                );
                sexpr::set_child(&mut node, tagged("uuid", vec![quoted(self.uuid.clone())]));
                node
            }
        }
    };
}

point_item!(Junction);
point_item!(NoConnect);

/// Which naming scope a label participates in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelKind {
    Local,
    Global,
    Hier,
}

impl LabelKind {
    pub(crate) fn head(self) -> &'static str {
        match self {
            LabelKind::Local => "label",
            LabelKind::Global => "global_label",
            LabelKind::Hier => "hierarchical_label",
        }
    }

    fn from_head(head: &str) -> Option<Self> {
        match head {
            "label" => Some(LabelKind::Local),
            "global_label" => Some(LabelKind::Global),
            "hierarchical_label" => Some(LabelKind::Hier),
            _ => None,
        }
    }
}

/// A net label of any scope.
#[derive(Debug, Clone, PartialEq)]
pub struct Label {
    pub uuid: String,
    pub kind: LabelKind,
    pub text: String,
    pub at: Pose,
    pub(crate) raw: Retained,
}

impl Label {
    fn decode(node: &Node, kind: LabelKind) -> Label {
        Label {
            uuid: child_text(node, "uuid").unwrap_or_default().to_string(),
            kind,
            text: items(node)
                .get(1)
                .and_then(sexpr::text)
                .unwrap_or_default()
                .to_string(),
            at: child(node, "at").map(decode_pose).unwrap_or_default(),
            raw: Retained::parsed(node.clone()),
        }
    }

    pub(crate) fn encode(&self) -> Node {
        let mut node = self.raw.node.clone();
        if let Some(children) = sexpr::items_mut(&mut node) {
            children[0] = sym(self.kind.head());
            if let Some(slot) = children.get_mut(1) {
                *slot = quoted(self.text.clone());
            }
        }
        sexpr::set_child(&mut node, encode_pose(self.at));
        sexpr::set_child(&mut node, tagged("uuid", vec![quoted(self.uuid.clone())]));
        node
    }
}

/// Free-standing sheet annotation text.
#[derive(Debug, Clone, PartialEq)]
pub struct Text {
    pub uuid: String,
    pub text: String,
    pub at: Pose,
    pub(crate) raw: Retained,
}

impl Text {
    fn decode(node: &Node) -> Text {
        Text {
            uuid: child_text(node, "uuid").unwrap_or_default().to_string(),
            text: items(node)
                .get(1)
                .and_then(sexpr::text)
                .unwrap_or_default()
                .to_string(),
            at: child(node, "at").map(decode_pose).unwrap_or_default(),
            raw: Retained::parsed(node.clone()),
        }
    }

    pub(crate) fn encode(&self) -> Node {
        let mut node = self.raw.node.clone();
        if let Some(children) = sexpr::items_mut(&mut node)
            && let Some(slot) = children.get_mut(1)
        {
            *slot = quoted(self.text.clone());
        }
        sexpr::set_child(&mut node, encode_pose(self.at));
        sexpr::set_child(&mut node, tagged("uuid", vec![quoted(self.uuid.clone())]));
        node
    }
}

/// A hierarchical sheet symbol and the pins on its border.
#[derive(Debug, Clone, PartialEq)]
pub struct Sheet {
    pub uuid: String,
    pub at: Point2,
    pub size: Point2,
    pub name: String,
    pub file: String,
    pub pins: Vec<SheetPin>,
    pub(crate) raw: Retained,
}

/// A pin on a hierarchical sheet's border.
#[derive(Debug, Clone, PartialEq)]
pub struct SheetPin {
    pub uuid: String,
    pub name: String,
    pub at: Pose,
}

impl Sheet {
    fn decode(node: &Node) -> Sheet {
        let property = |name: &str| {
            items(node)
                .iter()
                .filter(|c| sexpr::head(c) == Some("property"))
                .find(|c| items(c).get(1).and_then(sexpr::text) == Some(name))
                .and_then(|c| items(c).get(2).and_then(sexpr::text))
                .unwrap_or_default()
                .to_string()
        };
        let pins = items(node)
            .iter()
            .filter(|c| sexpr::head(c) == Some("pin"))
            .map(|c| SheetPin {
                uuid: child_text(c, "uuid").unwrap_or_default().to_string(),
                name: items(c).get(1).and_then(sexpr::text).unwrap_or_default().to_string(),
                at: child(c, "at").map(decode_pose).unwrap_or_default(),
            })
            .collect();
        let size = child(node, "size")
            .map(|c| {
                Point2::new(
                    items(c).get(1).and_then(sexpr::number).unwrap_or_default(),
                    items(c).get(2).and_then(sexpr::number).unwrap_or_default(),
                )
            })
            .unwrap_or(Point2::new(0.0, 0.0));
        Sheet {
            uuid: child_text(node, "uuid").unwrap_or_default().to_string(),
            at: child(node, "at").map(decode_pose).unwrap_or_default().point(),
            size,
            name: property("Sheetname"),
            file: property("Sheetfile"),
            pins,
            raw: Retained::parsed(node.clone()),
        }
    }

    pub(crate) fn encode(&self) -> Node {
        let mut node = self.raw.node.clone();
        sexpr::set_child(&mut node, tagged("at", vec![num(self.at.x), num(self.at.y)]));
        sexpr::set_child(
            &mut node,
            tagged("size", vec![num(self.size.x), num(self.size.y)]),
        );
        node
    }
}

/// The embedded `(lib_symbols …)` set, keyed by fully-qualified `Lib:Name`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LibSymbols {
    pub(crate) defs: IndexMap<String, Retained>,
    pub(crate) raw: Option<Retained>,
}

impl LibSymbols {
    fn decode(node: &Node) -> LibSymbols {
        let defs = items(node)
            .iter()
            .filter(|c| sexpr::head(c) == Some("symbol"))
            .filter_map(|c| {
                let name = items(c).get(1).and_then(sexpr::text)?.to_string();
                Some((name, Retained::parsed(c.clone())))
            })
            .collect();
        LibSymbols {
            defs,
            raw: Some(Retained::parsed(node.clone())),
        }
    }

    /// Whether a definition for `lib_id` is embedded.
    pub fn contains(&self, lib_id: &str) -> bool {
        self.defs.contains_key(lib_id)
    }

    /// The embedded definition node for `lib_id`.
    pub fn get(&self, lib_id: &str) -> Option<&Node> {
        self.defs.get(lib_id).map(|r| &r.node)
    }

    /// Embedded `Lib:Name` keys in document order.
    pub fn lib_ids(&self) -> impl Iterator<Item = &str> {
        self.defs.keys().map(String::as_str)
    }

    pub(crate) fn encode(&self) -> Node {
        let mut children = vec![sym("lib_symbols")];
        children.extend(self.defs.values().map(|r| r.node.clone()));
        list(children)
    }
}

/// One top-level entry of a schematic, in document order. `Other` covers every
/// node kind the typed model does not decode (buses, images, rule areas, the
/// header scalars …); it is retained verbatim and never dropped.
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    Symbol(SymbolInst),
    Wire(Wire),
    Junction(Junction),
    NoConnect(NoConnect),
    Label(Label),
    Text(Text),
    Sheet(Sheet),
    LibSymbols(LibSymbols),
    Other(Box<Retained>),
}

impl Item {
    pub(crate) fn decode(node: &Node) -> Item {
        match sexpr::head(node) {
            Some("symbol") => Item::Symbol(SymbolInst::decode(node)),
            Some("wire") => Item::Wire(Wire::decode(node)),
            Some("junction") => Item::Junction(Junction::decode(node)),
            Some("no_connect") => Item::NoConnect(NoConnect::decode(node)),
            Some("text") => Item::Text(Text::decode(node)),
            Some("sheet") => Item::Sheet(Sheet::decode(node)),
            Some("lib_symbols") => Item::LibSymbols(LibSymbols::decode(node)),
            Some(head) => match LabelKind::from_head(head) {
                Some(kind) => Item::Label(Label::decode(node, kind)),
                None => Item::Other(Box::new(Retained::parsed(node.clone()))),
            },
            None => Item::Other(Box::new(Retained::parsed(node.clone()))),
        }
    }

    /// The head token this item writes as.
    pub fn head(&self) -> &str {
        match self {
            Item::Symbol(_) => "symbol",
            Item::Wire(_) => "wire",
            Item::Junction(_) => "junction",
            Item::NoConnect(_) => "no_connect",
            Item::Label(l) => l.kind.head(),
            Item::Text(_) => "text",
            Item::Sheet(_) => "sheet",
            Item::LibSymbols(_) => "lib_symbols",
            Item::Other(raw) => sexpr::head(&raw.node).unwrap_or(""),
        }
    }

    /// Source bytes this item can be re-emitted from, when no edit invalidated
    /// them.
    pub(crate) fn pristine_span(&self) -> Option<Span> {
        match self {
            Item::Symbol(s) => s.raw.span,
            Item::Wire(w) => w.raw.span,
            Item::Junction(j) => j.raw.span,
            Item::NoConnect(n) => n.raw.span,
            Item::Label(l) => l.raw.span,
            Item::Text(t) => t.raw.span,
            Item::Sheet(s) => s.raw.span,
            Item::LibSymbols(l) => l.raw.as_ref().and_then(|r| r.span),
            Item::Other(raw) => raw.span,
        }
    }

    pub(crate) fn encode(&self) -> Node {
        match self {
            Item::Symbol(s) => s.encode(),
            Item::Wire(w) => w.encode(),
            Item::Junction(j) => j.encode(),
            Item::NoConnect(n) => n.encode(),
            Item::Label(l) => l.encode(),
            Item::Text(t) => t.encode(),
            Item::Sheet(s) => s.encode(),
            Item::LibSymbols(l) => l.encode(),
            Item::Other(raw) => raw.node.clone(),
        }
    }
}

fn decode_pose(node: &Node) -> Pose {
    let at = items(node);
    Pose {
        x: at.get(1).and_then(sexpr::number).unwrap_or_default(),
        y: at.get(2).and_then(sexpr::number).unwrap_or_default(),
        rot: at.get(3).and_then(sexpr::number).unwrap_or_default(),
    }
}

fn encode_pose(pose: Pose) -> Node {
    tagged("at", vec![num(pose.x), num(pose.y), num(pose.rot)])
}

fn decode_pts(node: &Node) -> Vec<Point2> {
    let Some(pts) = child(node, "pts") else {
        return Vec::new();
    };
    items(pts)
        .iter()
        .filter(|c| sexpr::head(c) == Some("xy"))
        .map(|c| {
            Point2::new(
                items(c).get(1).and_then(sexpr::number).unwrap_or_default(),
                items(c).get(2).and_then(sexpr::number).unwrap_or_default(),
            )
        })
        .collect()
}

fn encode_pts(points: &[Point2]) -> Node {
    tagged(
        "pts",
        points
            .iter()
            .map(|p| tagged("xy", vec![num(p.x), num(p.y)]))
            .collect(),
    )
}

pub(crate) fn yes_no(value: bool) -> Node {
    sym(if value { "yes" } else { "no" })
}

/// Set or clear a `(hide yes)` flag, preferring the location KiCAD uses for the
/// node at hand: symbol properties carry it inside `(effects …)`.
fn set_hide(node: &mut Node, hidden: bool) {
    let nested = sexpr::child(node, "effects").is_some_and(|e| sexpr::flag_present(e, "hide"));
    let target = if nested {
        sexpr::child_mut(node, "effects").expect("just checked")
    } else {
        node
    };
    if hidden {
        sexpr::set_child(target, tagged("hide", vec![sym("yes")]));
    } else {
        sexpr::remove_children(target, "hide");
    }
}

/// Rewrite the `(property …)` children to match `fields`, keeping document
/// order and every sub-node (effects, fonts, justification) the fields carry.
fn sync_properties(node: &mut Node, fields: &IndexMap<String, Field>) {
    let Some(children) = sexpr::items_mut(node) else {
        return;
    };
    let mut encoded: IndexMap<&str, Node> = fields
        .iter()
        .map(|(name, field)| (name.as_str(), field.encode()))
        .collect();
    let mut insert_at = children.len();
    let mut cursor = 0;
    while cursor < children.len() {
        if sexpr::head(&children[cursor]) != Some("property") {
            cursor += 1;
            continue;
        }
        let name = items(&children[cursor])
            .get(1)
            .and_then(sexpr::text)
            .unwrap_or_default()
            .to_string();
        match encoded.shift_remove(name.as_str()) {
            Some(replacement) => {
                children[cursor] = replacement;
                insert_at = cursor + 1;
                cursor += 1;
            }
            None => {
                children.remove(cursor);
            }
        }
    }
    for (offset, (_, added)) in encoded.into_iter().enumerate() {
        children.insert(insert_at + offset, added);
    }
}

/// Keep `(instances (project … (path … (reference …) (unit …))))` in step with
/// the typed refdes and unit, wherever the paths happen to point.
fn sync_instance_reference(node: &mut Node, refdes: &str, unit: u32) {
    let Some(instances) = sexpr::child_mut(node, "instances") else {
        return;
    };
    let Some(projects) = sexpr::items_mut(instances) else {
        return;
    };
    for project in projects.iter_mut() {
        let Some(paths) = sexpr::items_mut(project) else {
            continue;
        };
        for path in paths.iter_mut() {
            if sexpr::head(path) != Some("path") {
                continue;
            }
            sexpr::set_child(path, tagged("reference", vec![quoted(refdes)]));
            sexpr::set_child(path, tagged("unit", vec![num(unit as f64)]));
        }
    }
}

/// Build a fresh `(property …)` node for a symbol field.
pub(crate) fn new_field(name: &str, value: &str, at: Pose) -> Field {
    Field::new(name, value, at)
}
