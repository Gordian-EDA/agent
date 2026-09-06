//! Mutators. Every one of them invalidates only the items it rewrote, so the
//! rest of the file is still written back from its original bytes.

use geom::{EPS, Point2, Rect, stable_uuid};
use kiutils_sexpr::Node;

use crate::doc::SchDoc;
use crate::error::{Error, Result};
use crate::libsyms::SymbolSource;
use crate::model::{
    Rectangle,
    Item, Junction, Label, LabelKind, Mirror, NoConnect, Pose, Retained, SymbolInst, Wire,
    instance_path, instance_paths, new_field, property_node, retarget_instances,
    set_instance_reference, set_pin_uuid, yes_no,
};
use crate::sexpr::{list, num, quoted, sym, tagged};

/// The result of cutting wire geometry out of a rectangle.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WireClip {
    /// Original wire items replaced or removed.
    pub wires: usize,
    /// Boundary endpoints created on the surviving outside wire fragments.
    pub cut_points: Vec<Point2>,
}

struct WireFragments {
    uuid: String,
    outside: Vec<(Point2, Point2)>,
    cuts: Vec<Point2>,
}

/// Landscape dimensions of the ISO page sizes KiCAD names, in millimetres.
fn iso_page(name: &str) -> Option<[f64; 2]> {
    Some(match name {
        "A5" => [210.0, 148.0],
        "A4" => [297.0, 210.0],
        "A3" => [420.0, 297.0],
        "A2" => [594.0, 420.0],
        "A1" => [841.0, 594.0],
        "A0" => [1189.0, 841.0],
        _ => return None,
    })
}

/// Whether an item is part of the DRAWING rather than the design: wires,
/// junctions, no-connect markers, labels, free text, and the generated symbols
/// a drawing is made of — power-rail terminals and PWR_FLAGs, which carry a
/// hidden `#`-prefixed reference and belong to no bill of materials.
///
/// This is the set a re-wire owns: erase it around a selection and draw it again,
/// and the parts themselves are untouched.
pub fn is_drawing(item: &Item) -> bool {
    match item {
        Item::Wire(_)
        | Item::Junction(_)
        | Item::NoConnect(_)
        | Item::Label(_)
        | Item::Text(_)
        | Item::Rectangle(_) => true,
        Item::Symbol(symbol) => generated(symbol),
        _ => false,
    }
}

/// A symbol the drawing generated rather than the design declared. KiCAD marks
/// these with a `#`-prefixed reference so they stay out of the netlist's component
/// list.
fn generated(symbol: &SymbolInst) -> bool {
    symbol.refdes().starts_with('#')
}

/// `refdes` itself when free, else the first `<refdes>_<n>` that is.
fn free_reference(refdes: &str, taken: &std::collections::HashSet<String>) -> String {
    if !taken.contains(refdes) {
        return refdes.to_string();
    }
    (2..)
        .map(|n| format!("{refdes}_{n}"))
        .find(|candidate| !taken.contains(candidate))
        .expect("an unbounded sequence has a free name")
}

/// Which way a label's text runs, given the angle it is drawn at.
fn justify_for(rot: f64) -> &'static str {
    match rot.rem_euclid(360.0) as i64 {
        180 | 270 => "right",
        _ => "left",
    }
}

fn rename_generated(symbol: &mut SymbolInst, sheet_path: &str, refdes: &str) {
    let origin = symbol.at;
    // KiCAD never draws a `#`-prefixed reference: `#PWR5` is bookkeeping for
    // the netlister, and printing it puts a stray token on the drawing.
    let generated = refdes.starts_with('#');
    match symbol.fields.get_mut("Reference") {
        Some(field) => {
            field.value = refdes.to_string();
            field.hidden |= generated;
        }
        None => {
            symbol.fields.insert(
                "Reference".to_string(),
                new_field(
                    "Reference",
                    refdes,
                    Pose::new(origin.x, origin.y, 0.0),
                    true,
                ),
            );
        }
    }
    set_instance_reference(&mut symbol.raw.node, sheet_path, refdes);
}

impl SchDoc {
    /// A UUID derived from the root UUID, a kind and a content key, made unique
    /// against the UUIDs already in the document.
    pub fn derive_uuid(&self, kind: &str, content: &str) -> String {
        let root = self.root_uuid().to_string();
        let mut attempt = 0;
        loop {
            let key = if attempt == 0 {
                format!("{root}|{content}")
            } else {
                format!("{root}|{content}|{attempt}")
            };
            let candidate = stable_uuid(kind, &key);
            if !self.uuid_taken(&candidate) {
                return candidate;
            }
            attempt += 1;
        }
    }

    /// Every UUID already in the document, pins and sheet pins included — a
    /// collision check that missed those would not be one.
    fn uuid_taken(&self, uuid: &str) -> bool {
        self.items().iter().any(|item| {
            item.uuid() == Some(uuid)
                || match item {
                    Item::Symbol(s) => s.pin_uuids.values().any(|u| u == uuid),
                    Item::Sheet(s) => s.pins.iter().any(|p| p.uuid == uuid),
                    _ => false,
                }
        })
    }

    /// Insert after the last item of the same kind, else before the trailing
    /// blocks, so the file keeps KiCAD's grouping.
    pub(crate) fn insert_item(&mut self, item: Item) {
        let head = item.head().to_string();
        let items = self.items_mut();
        let at = items
            .iter()
            .rposition(|i| i.head() == head)
            .map(|i| i + 1)
            .or_else(|| items.iter().position(|i| crate::doc::is_trailer(i.head())))
            .unwrap_or(items.len());
        items.insert(at, item);
        self.mark_edited();
    }

    /// Move a symbol, carrying its field positions with it.
    ///
    /// `id` is a reference designator, or a UUID when several units share a
    /// reference — as it is on every mutator here.
    pub fn move_symbol(&mut self, id: &str, x: f64, y: f64) -> Result<()> {
        let uuid = self.uuid_of(id)?;
        let mut symbol = self.symbol_mut(&uuid)?;
        let (dx, dy) = (x - symbol.at.x, y - symbol.at.y);
        symbol.at.x = x;
        symbol.at.y = y;
        for field in symbol.fields.values_mut() {
            if let Some(at) = field.at.as_mut() {
                at.x += dx;
                at.y += dy;
            }
        }
        drop(symbol);
        self.mark_edited();
        Ok(())
    }

    /// Set a symbol's rotation and mirroring, leaving its position alone.
    pub fn set_symbol_orientation(&mut self, id: &str, rot: f64, mirror: Mirror) -> Result<()> {
        let uuid = self.uuid_of(id)?;
        let mut symbol = self.symbol_mut(&uuid)?;
        symbol.at.rot = rot;
        symbol.mirror = mirror;
        drop(symbol);
        self.mark_edited();
        Ok(())
    }

    /// Set a symbol's build attributes, leaving alone the ones not given.
    pub fn set_flags(&mut self, id: &str, dnp: Option<bool>, in_bom: Option<bool>) -> Result<()> {
        let uuid = self.uuid_of(id)?;
        let mut symbol = self.symbol_mut(&uuid)?;
        if let Some(dnp) = dnp {
            symbol.dnp = dnp;
        }
        if let Some(in_bom) = in_bom {
            symbol.in_bom = in_bom;
        }
        drop(symbol);
        self.mark_edited();
        Ok(())
    }

    /// Set a symbol property, creating it hidden at the symbol's origin if it is
    /// not there yet.
    ///
    /// Setting `Reference` also renames this sheet's own `(instances)` entry.
    /// A sheet placed several times in a hierarchy has no such entry — its
    /// references belong to the parent paths — so renaming one is refused
    /// rather than flattening the table.
    pub fn set_field(&mut self, id: &str, name: &str, value: &str) -> Result<()> {
        let uuid = self.uuid_of(id)?;
        if name == "Reference" {
            return self.set_reference(&[uuid], value);
        }
        let mut symbol = self.symbol_mut(&uuid)?;
        let origin = symbol.at;
        match symbol.fields.get_mut(name) {
            Some(field) => field.value = value.to_string(),
            None => {
                let field = new_field(name, value, Pose::new(origin.x, origin.y, 0.0), true);
                symbol.fields.insert(name.to_string(), field);
            }
        }
        drop(symbol);
        self.mark_edited();
        Ok(())
    }

    /// Move one existing field to an absolute sheet pose.
    pub fn set_field_pose(&mut self, id: &str, name: &str, at: Pose) -> Result<()> {
        let uuid = self.uuid_of(id)?;
        let mut symbol = self.symbol_mut(&uuid)?;
        let field = symbol
            .fields
            .get_mut(name)
            .ok_or_else(|| Error::UnknownField(name.to_string()))?;
        field.at = Some(at);
        drop(symbol);
        self.mark_edited();
        Ok(())
    }

    /// Rename one symbol or every unit of a part without allowing a duplicate
    /// reference designator on the sheet.
    pub fn set_reference(&mut self, ids: &[String], value: &str) -> Result<()> {
        let uuids: Vec<String> = ids
            .iter()
            .map(|id| self.uuid_of(id))
            .collect::<Result<_>>()?;
        if self
            .symbols()
            .any(|symbol| symbol.refdes() == value && !uuids.contains(&symbol.uuid))
        {
            return Err(Error::ReferenceInUse(value.to_string()));
        }
        for uuid in &uuids {
            if !self.owns_annotation(uuid) {
                return Err(Error::ForeignInstances(uuid.clone()));
            }
        }

        let sheet_path = self.sheet_path();
        for uuid in uuids {
            let mut symbol = self.symbol_mut(&uuid)?;
            let origin = symbol.at;
            match symbol.fields.get_mut("Reference") {
                Some(field) => field.value = value.to_string(),
                None => {
                    let field =
                        new_field("Reference", value, Pose::new(origin.x, origin.y, 0.0), true);
                    symbol.fields.insert("Reference".to_string(), field);
                }
            }
            set_instance_reference(&mut symbol.raw.node, &sheet_path, value);
        }
        self.mark_edited();
        Ok(())
    }

    /// Place every unit of a new symbol, embedding its library definition first.
    ///
    /// Refused on a sheet the hierarchy places more than once: each placement
    /// needs its own reference, and one call cannot say what the others are.
    /// Returns the new unit UUIDs in unit order.
    pub fn add_symbol(
        &mut self,
        lib_id: &str,
        refdes: &str,
        value: &str,
        at: Pose,
        source: &SymbolSource,
    ) -> Result<Vec<String>> {
        if self
            .symbols()
            .any(|s| instance_paths(s.retained().node()) > 1)
        {
            return Err(Error::ReInstantiatedSheet);
        }
        self.ensure_lib_symbol(lib_id, source)?;
        let unit_count = self
            .lib_symbols()
            .and_then(|libs| crate::pins::resolve(libs, lib_id))
            .map(crate::pins::unit_count)
            .unwrap_or(1);
        let mut uuids = Vec::new();
        let mut previous_bottom = None;
        for unit in 1..=unit_count {
            let uuid = self.add_symbol_unit(lib_id, refdes, value, at, unit);
            if let (Some(bottom), Some(extent)) = (previous_bottom, self.symbol_extent(&uuid)) {
                let dy = bottom + 2.54 - extent.min_y;
                self.move_symbol(&uuid, at.x, at.y + dy)?;
            }
            previous_bottom = self.symbol_extent(&uuid).map(|extent| extent.max_y);
            uuids.push(uuid);
        }
        Ok(uuids)
    }

    fn add_symbol_unit(
        &mut self,
        lib_id: &str,
        refdes: &str,
        value: &str,
        at: Pose,
        unit: u32,
    ) -> String {
        let uuid = self.derive_uuid("symbol", &format!("{lib_id}|{refdes}|{unit}"));
        let pins: Vec<String> = self
            .lib_symbols()
            .and_then(|libs| crate::pins::resolve(libs, lib_id))
            .map(|def| crate::pins::pin_numbers(def, unit, 1))
            .unwrap_or_default();
        let pin_nodes: Vec<Node> = pins
            .iter()
            .map(|number| {
                let pin_uuid = self.derive_uuid("pin", &format!("{uuid}|{number}"));
                list(vec![
                    sym("pin"),
                    quoted(number.clone()),
                    tagged("uuid", vec![quoted(pin_uuid)]),
                ])
            })
            .collect();

        let mut children = vec![
            sym("symbol"),
            tagged("lib_id", vec![quoted(lib_id)]),
            tagged("at", vec![num(at.x), num(at.y), num(at.rot)]),
            tagged("unit", vec![num(unit as f64)]),
            tagged("exclude_from_sim", vec![yes_no(false)]),
            tagged("in_bom", vec![yes_no(true)]),
            tagged("on_board", vec![yes_no(true)]),
            tagged("dnp", vec![yes_no(false)]),
            tagged("uuid", vec![quoted(uuid.clone())]),
            // KiCAD never draws a `#`-prefixed reference: `#PWR5` is
            // bookkeeping for the netlister, and printing it puts a stray
            // token on the drawing beside the rail name.
            property_node(
                "Reference",
                refdes,
                Pose::new(at.x, at.y - 2.54, 0.0),
                refdes.starts_with('#'),
            ),
            property_node("Value", value, Pose::new(at.x, at.y + 2.54, 0.0), false),
            property_node("Footprint", "", Pose::new(at.x, at.y, 0.0), true),
            property_node("Datasheet", "", Pose::new(at.x, at.y, 0.0), true),
            property_node("Description", "", Pose::new(at.x, at.y, 0.0), true),
        ];
        children.extend(pin_nodes);
        children.push(self.instances_node(refdes, unit));

        self.insert_item(Item::Symbol(SymbolInst::decode(&list(children))));
        uuid
    }

    fn symbol_extent(&self, uuid: &str) -> Option<Rect> {
        let symbol = self.symbol(uuid)?;
        let mut points: Vec<Point2> = crate::placed_pins(self)
            .into_iter()
            .filter(|pin| pin.owner == uuid)
            .map(|pin| pin.at)
            .collect();
        if let Some(body) = crate::body_rect(self, symbol) {
            points.push(Point2::new(body.min_x, body.min_y));
            points.push(Point2::new(body.max_x, body.max_y));
        }
        Rect::bounding(&points)
    }

    /// Remove a symbol and every field it owned.
    pub fn remove_symbol(&mut self, id: &str) -> Result<()> {
        let uuid = self.uuid_of(id)?;
        self.items_mut()
            .retain(|item| !matches!(item, Item::Symbol(s) if s.uuid == uuid));
        self.mark_edited();
        Ok(())
    }

    /// Relocate drawing attached at several old points in one pass.
    ///
    /// Every item is matched against its original position, so transposing two
    /// pin positions cannot carry the first pin's drawing a second time when
    /// the destination is also another source position.
    pub fn move_attached_many(&mut self, moves: &[(Point2, Point2)]) -> usize {
        let destination = |at: Point2| {
            moves
                .iter()
                .find(|(from, to)| !from.near_eq(*to, geom::EPS) && at.near_eq(*from, geom::EPS))
                .map(|(_, to)| *to)
        };
        let mut moved = 0;
        for item in self.items_mut() {
            let hit = match item {
                Item::Wire(wire) => {
                    let mut hit = false;
                    for point in wire.points.iter_mut() {
                        if let Some(to) = destination(*point) {
                            *point = to;
                            hit = true;
                        }
                    }
                    if hit {
                        wire.raw.touch();
                    }
                    hit
                }
                Item::Junction(j) if destination(j.at).is_some() => {
                    let to = destination(j.at).expect("matched destination");
                    j.at = to;
                    j.raw.touch();
                    true
                }
                Item::NoConnect(n) if destination(n.at).is_some() => {
                    let to = destination(n.at).expect("matched destination");
                    n.at = to;
                    n.raw.touch();
                    true
                }
                Item::Label(l) if destination(l.at.point()).is_some() => {
                    let to = destination(l.at.point()).expect("matched destination");
                    l.at.x = to.x;
                    l.at.y = to.y;
                    l.raw.touch();
                    true
                }
                _ => false,
            };
            moved += usize::from(hit);
        }
        if moved > 0 {
            self.mark_edited();
        }
        moved
    }

    /// Remove the wires, junctions, labels, no-connects and text carrying these
    /// UUIDs, returning how many went. Symbols are not removable this way —
    /// they carry annotation state, so they go through [`Self::remove_symbol`].
    pub fn remove_drawing(&mut self, uuids: &[String]) -> usize {
        let matches = |uuid: &str| uuids.iter().any(|u| u == uuid);
        let before = self.items().len();
        self.items_mut().retain(|item| match item {
            Item::Wire(w) => !matches(&w.uuid),
            Item::Junction(j) => !matches(&j.uuid),
            Item::Label(l) => !matches(&l.uuid),
            Item::NoConnect(n) => !matches(&n.uuid),
            Item::Text(t) => !matches(&t.uuid),
            _ => true,
        });
        let removed = before - self.items().len();
        if removed > 0 {
            self.mark_edited();
        }
        removed
    }

    /// Remove wire portions in `bounds`, keeping outside fragments terminated
    /// exactly at the rectangle boundary.
    pub fn clip_wires_outside(&mut self, bounds: Rect) -> WireClip {
        let touched: Vec<WireFragments> = self
            .wires()
            .filter_map(|wire| {
                let mut fragments = Vec::new();
                let mut cuts = Vec::new();
                let mut hit = false;
                for pair in wire.points.windows(2) {
                    let (outside, boundary) = segment_outside_rect(pair[0], pair[1], bounds);
                    hit |= !boundary.is_empty() || outside.is_empty();
                    fragments.extend(outside);
                    cuts.extend(boundary);
                }
                hit.then(|| WireFragments {
                    uuid: wire.uuid.clone(),
                    outside: fragments,
                    cuts,
                })
            })
            .collect();
        let uuids = touched
            .iter()
            .map(|wire| wire.uuid.clone())
            .collect::<Vec<_>>();
        self.remove_drawing(&uuids);
        let mut cut_points = Vec::new();
        for wire in &touched {
            for &(from, to) in &wire.outside {
                if !from.near_eq(to, EPS) {
                    self.add_wire(from, to);
                }
            }
            for &point in &wire.cuts {
                if !cut_points
                    .iter()
                    .any(|seen: &Point2| seen.near_eq(point, EPS))
                {
                    cut_points.push(point);
                }
            }
        }
        WireClip {
            wires: touched.len(),
            cut_points,
        }
    }

    /// Give every label naming `net` the same scope, returning how many changed.
    ///
    /// A net drawn with both a `label` and a `global_label` of the same name is
    /// KiCAD's `same_local_global_label` warning: the two scopes do not merge, so
    /// the drawing says one thing and the netlist another. A sheet may hold only
    /// one scope per net, and this is where that is enforced.
    ///
    /// Hierarchical labels are left alone — they name a sheet pin, not a sheet
    /// net. Each rewritten label is rebuilt from scratch rather than re-headed,
    /// because the two scopes do not share a node shape: a `global_label` carries
    /// a `shape` and an `Intersheetrefs` property a plain `label` must not keep.
    pub fn set_label_scope(&mut self, net: &str, kind: LabelKind) -> usize {
        let doomed: Vec<(String, Pose)> = self
            .labels()
            .filter(|label| {
                label.kind != kind
                    && label.kind != LabelKind::Hier
                    && crate::text::unescape(&label.text) == net
            })
            .map(|label| (label.uuid.clone(), label.at))
            .collect();
        let uuids: Vec<String> = doomed.iter().map(|(uuid, _)| uuid.clone()).collect();
        self.remove_drawing(&uuids);
        for (_, at) in &doomed {
            self.add_label(kind, net, *at);
        }
        doomed.len()
    }

    /// Retarget a placed symbol at a different library part, keeping its
    /// position, orientation and properties.
    ///
    /// The new definition is embedded first, and the instance's `(pin …)` UUID
    /// table is rebuilt for the new pin set — a stale table would leave KiCAD
    /// with pins it cannot address. Returns the pin numbers the old part had
    /// and the new one does not, which is exactly what a caller has to re-map.
    pub fn set_lib_id(
        &mut self,
        id: &str,
        lib_id: &str,
        source: &SymbolSource,
    ) -> Result<Vec<String>> {
        let uuid = self.uuid_of(id)?;
        self.ensure_lib_symbol(lib_id, source)?;
        let unit = self.symbol(&uuid).map_or(1, |s| s.unit);
        let numbers: Vec<String> = self
            .lib_symbols()
            .and_then(|libs| crate::pins::resolve(libs, lib_id))
            .map(|def| crate::pins::pin_numbers(def, unit, 1))
            .unwrap_or_default();
        let fresh: Vec<(String, String)> = numbers
            .iter()
            .map(|n| {
                (
                    n.clone(),
                    self.derive_uuid("pin", &format!("{uuid}|{lib_id}|{n}")),
                )
            })
            .collect();

        let mut symbol = self.symbol_mut(&uuid)?;
        let dropped = symbol
            .pin_uuids
            .keys()
            .filter(|n| !numbers.contains(n))
            .cloned()
            .collect();
        let old = std::mem::take(&mut symbol.pin_uuids);
        symbol.lib_id = lib_id.to_string();
        symbol.pin_uuids = fresh
            .into_iter()
            .map(|(number, pin_uuid)| {
                let existing = old.get(&number).cloned();
                (number, existing.unwrap_or(pin_uuid))
            })
            .collect();
        let pin_nodes: Vec<Node> = symbol
            .pin_uuids
            .iter()
            .map(|(number, pin_uuid)| {
                list(vec![
                    sym("pin"),
                    quoted(number.clone()),
                    tagged("uuid", vec![quoted(pin_uuid.clone())]),
                ])
            })
            .collect();
        crate::sexpr::remove_children(&mut symbol.raw.node, "pin");
        if let Some(children) = crate::sexpr::items_mut(&mut symbol.raw.node) {
            let at = children
                .iter()
                .position(|c| crate::sexpr::head(c) == Some("instances"))
                .unwrap_or(children.len());
            children.splice(at..at, pin_nodes);
        }
        drop(symbol);
        self.mark_edited();
        Ok(dropped)
    }

    /// Draw a wire between two points. Returns its UUID.
    pub fn add_wire(&mut self, from: Point2, to: Point2) -> String {
        let uuid = self.derive_uuid("wire", &format!("{},{}->{},{}", from.x, from.y, to.x, to.y));
        let node = list(vec![
            sym("wire"),
            tagged(
                "pts",
                vec![
                    tagged("xy", vec![num(from.x), num(from.y)]),
                    tagged("xy", vec![num(to.x), num(to.y)]),
                ],
            ),
            tagged(
                "stroke",
                vec![
                    tagged("width", vec![num(0.0)]),
                    tagged("type", vec![sym("default")]),
                ],
            ),
            tagged("uuid", vec![quoted(uuid.clone())]),
        ]);
        self.insert_item(Item::Wire(Wire {
            uuid: uuid.clone(),
            points: vec![from, to],
            raw: Retained::owned(node),
        }));
        uuid
    }

    /// Attach a label. `text` is plain text: characters KiCAD cannot store
    /// literally, `/` among them, are escaped on the way in. Returns its UUID.
    pub fn add_label(&mut self, kind: LabelKind, text: &str, at: Pose) -> String {
        let uuid = self.derive_uuid(kind.head(), &format!("{text}|{},{}", at.x, at.y));
        let text = crate::text::escape(text);
        let node = list(vec![
            sym(kind.head()),
            quoted(text.clone()),
            tagged("at", vec![num(at.x), num(at.y), num(at.rot)]),
            tagged(
                "effects",
                vec![
                    tagged("font", vec![tagged("size", vec![num(1.27), num(1.27)])]),
                    // KiCAD folds a label's angle into [0, 180) so the text
                    // never reads upside down, which leaves the justification
                    // as the only thing saying WHICH WAY it runs. Without it a
                    // 180/270 label straddles its anchor and half the text
                    // runs back over whatever it is naming.
                    tagged("justify", vec![sym(justify_for(at.rot)), sym("bottom")]),
                ],
            ),
            tagged("uuid", vec![quoted(uuid.clone())]),
        ]);
        self.insert_item(Item::Label(Label {
            uuid: uuid.clone(),
            kind,
            text,
            at,
            raw: Retained::owned(node),
        }));
        uuid
    }

    /// Draw a dashed frame — the outline a block is drawn in. Returns its UUID.
    pub fn add_rectangle(&mut self, start: Point2, end: Point2) -> String {
        let uuid = self.derive_uuid(
            "rectangle",
            &format!("{},{}|{},{}", start.x, start.y, end.x, end.y),
        );
        let node = list(vec![
            sym("rectangle"),
            tagged("start", vec![num(start.x), num(start.y)]),
            tagged("end", vec![num(end.x), num(end.y)]),
            tagged(
                "stroke",
                vec![
                    tagged("width", vec![num(0.1524)]),
                    tagged("type", vec![sym("dash")]),
                ],
            ),
            tagged("fill", vec![tagged("type", vec![sym("none")])]),
            tagged("uuid", vec![quoted(uuid.clone())]),
        ]);
        self.insert_item(Item::Rectangle(Rectangle {
            uuid: uuid.clone(),
            start,
            end,
            raw: Retained::owned(node),
        }));
        uuid
    }

    /// Place a connection dot. Returns its UUID.
    pub fn add_junction(&mut self, at: Point2) -> String {
        let uuid = self.derive_uuid("junction", &format!("{},{}", at.x, at.y));
        let node = list(vec![
            sym("junction"),
            tagged("at", vec![num(at.x), num(at.y)]),
            tagged("diameter", vec![num(0.0)]),
            list(vec![sym("color"), num(0.0), num(0.0), num(0.0), num(0.0)]),
            tagged("uuid", vec![quoted(uuid.clone())]),
        ]);
        self.insert_item(Item::Junction(Junction {
            uuid: uuid.clone(),
            at,
            raw: Retained::owned(node),
        }));
        uuid
    }

    /// Mark a pin intentionally unconnected. Returns the marker's UUID.
    ///
    /// A point carries at most one marker: two markers at one coordinate say
    /// nothing a single one does not, and KiCAD counts each of them separately
    /// when it reports the pin. A repeat call returns the marker already there.
    pub fn add_no_connect(&mut self, at: Point2) -> String {
        if let Some(existing) = self.items().iter().find_map(|item| match item {
            Item::NoConnect(marker) if marker.at.near_eq(at, geom::EPS) => Some(&marker.uuid),
            _ => None,
        }) {
            return existing.clone();
        }
        let uuid = self.derive_uuid("no_connect", &format!("{},{}", at.x, at.y));
        let node = list(vec![
            sym("no_connect"),
            tagged("at", vec![num(at.x), num(at.y)]),
            tagged("uuid", vec![quoted(uuid.clone())]),
        ]);
        self.insert_item(Item::NoConnect(NoConnect {
            uuid: uuid.clone(),
            at,
            raw: Retained::owned(node),
        }));
        uuid
    }

    /// Merge the drawable content of `source` into this document.
    ///
    /// Symbols, wires, junctions, no-connects, labels and text are appended;
    /// `(lib_symbols)` definitions are unioned by `lib_id`; header and trailer
    /// sections are ignored. Every adopted item is given a UUID derived here, and
    /// a symbol's `(instances)` path is retargeted at this sheet — otherwise the
    /// graft would point at the sheet it was drawn on and drop out of the
    /// netlist. Returns the UUIDs of the symbols adopted, in source order.
    pub fn adopt(&mut self, source: &SchDoc) -> Result<Vec<String>> {
        self.merge(source, true)
    }

    /// The page size in millimetres, landscape as KiCAD lays a schematic out.
    ///
    /// A named size resolves to its landscape dimensions; a `User` page reads its own.
    pub fn page(&self) -> Option<[f64; 2]> {
        let node = self.items().iter().find_map(|item| match item {
            Item::Other(raw) if crate::sexpr::head(&raw.node) == Some("paper") => Some(&raw.node),
            _ => None,
        })?;
        let args = crate::sexpr::items(node);
        let name = crate::sexpr::text(args.get(1)?)?;
        if name == "User" {
            let w = crate::sexpr::number(args.get(2)?)?;
            let h = crate::sexpr::number(args.get(3)?)?;
            return Some([w, h]);
        }
        iso_page(name)
    }

    /// Merge only `source`'s DRAWING — see [`is_drawing`]. What a re-wire of parts
    /// this document already holds needs: the wires and labels come across, the
    /// parts themselves do not.
    pub fn adopt_drawing(&mut self, source: &SchDoc) -> Result<()> {
        self.merge(source, false)?;
        Ok(())
    }

    fn merge(&mut self, source: &SchDoc, symbols: bool) -> Result<Vec<String>> {
        if symbols
            && self
                .symbols()
                .any(|s| instance_paths(s.retained().node()) > 1)
        {
            return Err(Error::ReInstantiatedSheet);
        }
        self.union_lib_symbols(source);
        let sheet_path = self.sheet_path();
        // The realiser names its power symbols and flags after the nets they sit
        // on, so a second graft onto the same sheet brings the same `#PWR_GND_0`
        // again — two symbols answering to one reference, and a netlist that
        // cannot tell their pins apart.
        let mut taken: std::collections::HashSet<String> =
            self.symbols().map(|s| s.refdes().to_string()).collect();
        let mut adopted = Vec::new();
        for item in source.items().to_vec() {
            match item {
                Item::Symbol(mut symbol) if symbols || generated(&symbol) => {
                    if generated(&symbol) {
                        let free = free_reference(symbol.refdes(), &taken);
                        taken.insert(free.clone());
                        if free != symbol.refdes() {
                            rename_generated(&mut symbol, &sheet_path, &free);
                        }
                    }
                    let symbol = self.regraft_symbol(symbol, &sheet_path);
                    adopted.push(symbol.uuid.clone());
                    self.insert_item(Item::Symbol(symbol));
                }
                Item::Wire(_)
                | Item::Junction(_)
                | Item::NoConnect(_)
                | Item::Label(_)
                | Item::Text(_)
                | Item::Rectangle(_) => {
                    let item = self.regraft(item);
                    self.insert_item(item);
                }
                // The block the realiser drew carries the design's title. A sheet
                // has one title block, so it is adopted only onto a sheet with none —
                // the first block to land names the sheet, later ones leave it alone.
                Item::Other(mut raw)
                    if crate::sexpr::head(&raw.node) == Some("title_block")
                        && !self.has_title_block() =>
                {
                    // The span it carries indexes the SOURCE's bytes, which this document
                    // does not have; adopt the node itself.
                    raw.touch();
                    self.insert_item(Item::Other(raw));
                }
                Item::Symbol(_) | Item::Sheet(_) | Item::LibSymbols(_) | Item::Other(_) => {}
            }
        }
        Ok(adopted)
    }

    /// Drop the drawing items — see [`is_drawing`] — that `keep` rejects. Placed
    /// parts, sheets and the header sections are never offered to it. Returns how
    /// many items were removed.
    pub fn retain_drawing(&mut self, mut keep: impl FnMut(&Item) -> bool) -> usize {
        let before = self.items().len();
        self.items_mut()
            .retain(|item| !is_drawing(item) || keep(item));
        let removed = before - self.items().len();
        if removed > 0 {
            self.mark_edited();
        }
        removed
    }

    /// Add every definition `source` embeds that this document does not have.
    fn union_lib_symbols(&mut self, source: &SchDoc) {
        let Some(incoming) = source.lib_symbols() else {
            return;
        };
        let missing: Vec<(String, Retained)> = incoming
            .defs
            .iter()
            .filter(|(lib_id, _)| {
                !self
                    .lib_symbols()
                    .is_some_and(|libs| libs.contains(lib_id.as_str()))
            })
            .map(|(lib_id, def)| (lib_id.clone(), def.clone()))
            .collect();
        if missing.is_empty() {
            return;
        }
        let libs = self.lib_symbols_mut();
        libs.defs.extend(missing);
        libs.defs.sort_keys();
        if let Some(raw) = libs.raw.as_mut() {
            raw.touch();
        }
        self.mark_edited();
    }

    /// A UUID for a grafted item, keyed on everything about it but its old UUID.
    fn graft_uuid(&self, kind: &str, node: &Node) -> String {
        let mut keyed = node.clone();
        crate::sexpr::remove_children(&mut keyed, "uuid");
        self.derive_uuid(kind, &crate::sexpr::flat(&keyed))
    }

    fn regraft(&mut self, item: Item) -> Item {
        macro_rules! reuuid {
            ($value:expr, $head:expr) => {{
                let mut value = $value;
                value.uuid = self.graft_uuid($head, &value.raw.node);
                value.raw.touch();
                value
            }};
        }
        let head = item.head().to_string();
        match item {
            Item::Wire(w) => Item::Wire(reuuid!(w, &head)),
            Item::Junction(j) => Item::Junction(reuuid!(j, &head)),
            Item::NoConnect(n) => Item::NoConnect(reuuid!(n, &head)),
            Item::Label(l) => Item::Label(reuuid!(l, &head)),
            Item::Text(t) => Item::Text(reuuid!(t, &head)),
            Item::Rectangle(r) => Item::Rectangle(reuuid!(r, &head)),
            other => other,
        }
    }

    fn regraft_symbol(&mut self, mut symbol: SymbolInst, sheet_path: &str) -> SymbolInst {
        symbol.uuid = self.graft_uuid("symbol", &symbol.raw.node);
        let pins: Vec<(String, String)> = symbol
            .pin_uuids
            .keys()
            .map(|number| {
                let key = format!("{}|{number}", symbol.uuid);
                (number.clone(), self.derive_uuid("pin", &key))
            })
            .collect();
        for (number, uuid) in pins {
            set_pin_uuid(&mut symbol.raw.node, &number, &uuid);
            symbol.pin_uuids.insert(number, uuid);
        }
        retarget_instances(&mut symbol.raw.node, sheet_path);
        symbol.raw.touch();
        symbol
    }

    /// Whether this sheet, rather than a parent hierarchy, decides what this
    /// symbol is called. A symbol with no `(instances)` at all is this sheet's.
    fn owns_annotation(&self, uuid: &str) -> bool {
        let sheet_path = self.sheet_path();
        self.symbol(uuid).is_some_and(|symbol| {
            let node = symbol.retained().node();
            crate::sexpr::child(node, "instances").is_none()
                || instance_path(node, &sheet_path).is_some()
        })
    }

    /// Resolve a symbol identifier — a UUID, or a reference designator.
    ///
    /// The units of a multi-unit part share a reference, so a reference alone
    /// does not name a symbol there; editing one unit and leaving its siblings
    /// behind would make a part whose halves disagree. Those are addressed by
    /// UUID instead.
    fn uuid_of(&self, id: &str) -> Result<String> {
        if self.symbol(id).is_some() {
            return Ok(id.to_string());
        }
        let mut matching = self.symbols().filter(|s| s.refdes() == id);
        let first = matching
            .next()
            .ok_or_else(|| Error::UnknownReference(id.to_string()))?;
        match matching.next() {
            None => Ok(first.uuid.clone()),
            Some(_) => Err(Error::AmbiguousReference(id.to_string())),
        }
    }

    /// Clone the project and path an existing symbol uses, so a new symbol
    /// lands in the same hierarchy; fall back to this sheet's own root path.
    /// Only reached once the sheet is known to have a single placement.
    fn instances_node(&self, refdes: &str, unit: u32) -> Node {
        let project = self
            .symbols()
            .find_map(|s| {
                let instances = crate::sexpr::child(&s.raw.node, "instances")?;
                crate::sexpr::items(instances)
                    .iter()
                    .find(|c| crate::sexpr::head(c) == Some("project"))
            })
            .cloned();
        let (project_name, path) = match project.as_ref() {
            Some(node) => (
                crate::sexpr::items(node)
                    .get(1)
                    .and_then(crate::sexpr::text)
                    .unwrap_or_default()
                    .to_string(),
                crate::sexpr::items(node)
                    .iter()
                    .find(|c| crate::sexpr::head(c) == Some("path"))
                    .and_then(|c| crate::sexpr::items(c).get(1))
                    .and_then(crate::sexpr::text)
                    .unwrap_or_default()
                    .to_string(),
            ),
            None => (String::new(), format!("/{}", self.root_uuid())),
        };
        tagged(
            "instances",
            vec![list(vec![
                sym("project"),
                quoted(project_name),
                list(vec![
                    sym("path"),
                    quoted(path),
                    tagged("reference", vec![sym(refdes)]),
                    tagged("unit", vec![num(unit as f64)]),
                ]),
            ])],
        )
    }
}

/// The pieces of `a..b` outside `bounds`, plus newly exposed boundary points.
fn segment_outside_rect(
    a: Point2,
    b: Point2,
    bounds: Rect,
) -> (Vec<(Point2, Point2)>, Vec<Point2>) {
    let delta = Point2::new(b.x - a.x, b.y - a.y);
    let mut enter = 0.0_f64;
    let mut exit = 1.0_f64;
    for (p, q) in [
        (-delta.x, a.x - bounds.min_x),
        (delta.x, bounds.max_x - a.x),
        (-delta.y, a.y - bounds.min_y),
        (delta.y, bounds.max_y - a.y),
    ] {
        if p.abs() <= EPS {
            if q < -EPS {
                return (vec![(a, b)], Vec::new());
            }
            continue;
        }
        let ratio = q / p;
        if p < 0.0 {
            enter = enter.max(ratio);
        } else {
            exit = exit.min(ratio);
        }
        if enter > exit {
            return (vec![(a, b)], Vec::new());
        }
    }
    let at = |t: f64| Point2::new(a.x + delta.x * t, a.y + delta.y * t);
    let middle = at((enter + exit) / 2.0);
    let crosses_interior = exit - enter > EPS
        && middle.x > bounds.min_x + EPS
        && middle.x < bounds.max_x - EPS
        && middle.y > bounds.min_y + EPS
        && middle.y < bounds.max_y - EPS;
    if !crosses_interior {
        return (vec![(a, b)], Vec::new());
    }
    let mut outside = Vec::new();
    let mut cuts = Vec::new();
    if enter > EPS {
        let point = at(enter);
        outside.push((a, point));
        cuts.push(point);
    }
    if exit < 1.0 - EPS {
        let point = at(exit);
        outside.push((point, b));
        cuts.push(point);
    }
    (outside, cuts)
}
