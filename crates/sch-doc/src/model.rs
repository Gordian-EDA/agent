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
            Node::List { span, .. } | Node::Atom { span, .. } => *span,
        };
        // A node built in memory carries an empty span; that is not a slice of
        // any source and must never be mistaken for one.
        Self {
            span: (span.end > span.start).then_some(span),
            node,
        }
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
    /// The `(property …)` node this was decoded from, so effects, fonts and
    /// justification survive a value change.
    node: Node,
}

impl Field {
    fn decode(node: &Node) -> Option<(String, Field)> {
        let name = sexpr::text(items(node).get(1)?)?.to_string();
        // A property with no value slot is malformed, but dropping it would
        // lose the field; read it as empty and let `encode` fill the slot.
        let value = items(node)
            .get(2)
            .and_then(sexpr::text)
            .unwrap_or_default()
            .to_string();
        let at = child(node, "at").map(decode_pose);
        let hidden = sexpr::flag_present(node, "hide")
            || child(node, "effects").is_some_and(|e| sexpr::flag_present(e, "hide"));
        Some((
            name,
            Field {
                value,
                at,
                hidden,
                node: node.clone(),
            },
        ))
    }

    fn new(name: &str, value: &str, at: Pose, hidden: bool) -> Field {
        Field {
            value: value.to_string(),
            at: Some(at),
            hidden,
            node: property_node(name, value, at, hidden),
        }
    }

    fn encode(&self) -> Node {
        let mut node = self.node.clone();
        set_positional(&mut node, 2, quoted(self.value.clone()));
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
            unit: child_text(node, "unit")
                .and_then(|s| s.parse().ok())
                .unwrap_or(1),
            at: child(node, "at").map(decode_pose).unwrap_or_default(),
            mirror: child_text(node, "mirror")
                .map(Mirror::parse)
                .unwrap_or_default(),
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
        self.fields
            .get("Reference")
            .map_or("", |f| f.value.as_str())
    }

    /// The node this symbol was decoded from, with every child the typed model
    /// does not cover.
    pub fn retained(&self) -> &Retained {
        &self.raw
    }

    /// Value, from the `Value` property.
    pub fn value(&self) -> &str {
        self.fields.get("Value").map_or("", |f| f.value.as_str())
    }

    pub(crate) fn encode(&self) -> Node {
        let mut node = self.raw.node.clone();
        sexpr::set_child(
            &mut node,
            tagged("lib_id", vec![quoted(self.lib_id.clone())]),
        );
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
        set_positional(&mut node, 0, sym(self.kind.head()));
        set_positional(&mut node, 1, quoted(self.text.clone()));
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
        set_positional(&mut node, 1, quoted(self.text.clone()));
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

/// A pin on a hierarchical sheet's border. `at` is in sheet coordinates, not
/// relative to the sheet box.
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
                name: items(c)
                    .get(1)
                    .and_then(sexpr::text)
                    .unwrap_or_default()
                    .to_string(),
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
            at: child(node, "at")
                .map(decode_pose)
                .unwrap_or_default()
                .point(),
            size,
            name: property("Sheetname"),
            file: property("Sheetfile"),
            pins,
            raw: Retained::parsed(node.clone()),
        }
    }

    /// Sheets are decoded so tools can see the hierarchy and its border pins;
    /// nothing edits one yet, so this hands back what was parsed rather than
    /// pretending the typed fields are writable.
    pub(crate) fn encode(&self) -> Node {
        self.raw.node.clone()
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

    /// Re-emit the block with the current definitions in place of the old ones,
    /// keeping any child the typed model does not decode.
    pub(crate) fn encode(&self) -> Node {
        let Some(raw) = self.raw.as_ref() else {
            let mut children = vec![sym("lib_symbols")];
            children.extend(self.defs.values().map(|r| r.node.clone()));
            return list(children);
        };
        let mut children = Vec::with_capacity(self.defs.len() + 1);
        let mut written = false;
        for child in items(&raw.node) {
            if sexpr::head(child) != Some("symbol") {
                children.push(child.clone());
                continue;
            }
            if !written {
                written = true;
                children.extend(self.defs.values().map(|r| r.node.clone()));
            }
        }
        if !written {
            children.extend(self.defs.values().map(|r| r.node.clone()));
        }
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

/// Overwrite a fixed-position child, padding with empty strings if the node is
/// short — a truncated node is malformed, but losing the value would be worse.
fn set_positional(node: &mut Node, index: usize, value: Node) {
    let Some(children) = sexpr::items_mut(node) else {
        return;
    };
    while children.len() <= index {
        children.push(quoted(""));
    }
    children[index] = value;
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
    // KiCAD has written this flag on the property and inside its effects, as a
    // list and as a bare atom. Clear all of them, then write one back where the
    // node already kept it.
    let nested = sexpr::child(node, "effects").is_some_and(|e| sexpr::flag_present(e, "hide"));
    sexpr::remove_children(node, "hide");
    if let Some(effects) = sexpr::child_mut(node, "effects") {
        sexpr::remove_children(effects, "hide");
    }
    if !hidden {
        return;
    }
    let target = match nested {
        true => sexpr::child_mut(node, "effects").expect("just checked"),
        false => node,
    };
    sexpr::set_child(target, tagged("hide", vec![sym("yes")]));
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
    let mut written: Vec<String> = Vec::new();
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
            // A name already written is a duplicate the typed model cannot
            // represent; leave it exactly as it was rather than drop it.
            None if written.contains(&name) => cursor += 1,
            None => {
                children.remove(cursor);
            }
        }
        written.push(name);
    }
    for (offset, (_, added)) in encoded.into_iter().enumerate() {
        children.insert((insert_at + offset).min(children.len()), added);
    }
}

/// Rewrite the reference on the one `(instances … (path …))` entry that belongs
/// to this sheet.
///
/// A sheet placed several times in a hierarchy carries one path per placement,
/// each with its own reference; only the entry whose path is this sheet's own
/// may be touched, and a re-instantiated sheet has none.
pub(crate) fn set_instance_reference(node: &mut Node, sheet_path: &str, refdes: &str) {
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
            if sexpr::head(path) == Some("path")
                && items(path).get(1).and_then(sexpr::text) == Some(sheet_path)
            {
                sexpr::set_child(path, tagged("reference", vec![quoted(refdes)]));
            }
        }
    }
}

/// How many `(instances … (path …))` entries a symbol carries — one per
/// placement of the sheet it lives on.
pub(crate) fn instance_paths(node: &Node) -> usize {
    let Some(instances) = sexpr::child(node, "instances") else {
        return 0;
    };
    items(instances)
        .iter()
        .map(|project| {
            items(project)
                .iter()
                .filter(|c| sexpr::head(c) == Some("path"))
                .count()
        })
        .sum()
}

/// The `(path …)` entry for `sheet_path`, if the symbol has one.
pub(crate) fn instance_path<'a>(node: &'a Node, sheet_path: &str) -> Option<&'a Node> {
    let instances = sexpr::child(node, "instances")?;
    items(instances).iter().find_map(|project| {
        items(project).iter().find(|path| {
            sexpr::head(path) == Some("path")
                && items(path).get(1).and_then(sexpr::text) == Some(sheet_path)
        })
    })
}

/// Build a fresh symbol field in the shape KiCAD writes.
pub(crate) fn new_field(name: &str, value: &str, at: Pose, hidden: bool) -> Field {
    Field::new(name, value, at, hidden)
}

/// A `(property …)` node with the effects block KiCAD always emits.
pub(crate) fn property_node(name: &str, value: &str, at: Pose, hidden: bool) -> Node {
    let mut effects = vec![tagged(
        "font",
        vec![tagged("size", vec![num(1.27), num(1.27)])],
    )];
    if hidden {
        effects.push(tagged("hide", vec![sym("yes")]));
    }
    list(vec![
        sym("property"),
        quoted(name),
        quoted(value),
        encode_pose(at),
        tagged("effects", effects),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiutils_sexpr::parse_one;

    fn field(text: &str) -> Field {
        let cst = parse_one(text).expect("parse");
        Field::decode(&cst.nodes[0]).expect("decode").1
    }

    fn render(field: &Field) -> String {
        crate::sexpr::flat(&field.encode())
    }

    /// KiCAD has written this flag as a bare atom and as a list; both must read
    /// as hidden, and re-encoding must not leave two of them behind.
    #[test]
    fn the_hide_flag_survives_both_spellings() {
        for source in [
            r#"(property "Datasheet" "" (at 0 0 0) hide)"#,
            r#"(property "Datasheet" "" (at 0 0 0) (hide yes))"#,
        ] {
            let mut f = field(source);
            assert!(f.hidden, "{source}");
            let once = render(&f);
            assert_eq!(once.matches("hide").count(), 1, "{once}");

            f.value = "http://x".to_string();
            let edited = render(&f);
            assert_eq!(edited.matches("hide").count(), 1, "{edited}");
            assert!(edited.contains(r#""http://x""#), "{edited}");

            f.hidden = false;
            let shown = render(&f);
            assert!(!shown.contains("hide"), "{shown}");
        }
    }

    /// A property that keeps the flag inside its effects block keeps it there.
    #[test]
    fn a_nested_hide_flag_stays_nested() {
        let mut f = field(
            r##"(property "Reference" "#PWR" (at 0 0 0) (effects (font (size 1.27 1.27)) (hide yes)))"##,
        );
        assert!(f.hidden);
        f.value = "#PWR01".to_string();
        let rendered = render(&f);
        assert_eq!(rendered.matches("hide").count(), 1, "{rendered}");
        assert!(
            rendered.contains("(font (size 1.27 1.27)) (hide yes)"),
            "{rendered}"
        );
    }

    #[test]
    fn a_short_property_node_keeps_its_new_value() {
        let mut f = field(r#"(property "MPN")"#);
        f.value = "RC0603".to_string();
        assert!(render(&f).contains(r#""RC0603""#), "{}", render(&f));
    }
}
