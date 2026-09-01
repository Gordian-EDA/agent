//! Mutators. Every one of them invalidates only the items it rewrote, so the
//! rest of the file is still written back from its original bytes.

use geom::{Point2, stable_uuid};
use kiutils_sexpr::Node;

use crate::doc::SchDoc;
use crate::error::{Error, Result};
use crate::libsyms::SymbolSource;
use crate::model::{
    Item, Junction, Label, LabelKind, Mirror, NoConnect, Pose, Retained, SymbolInst, Wire,
    instance_path, instance_paths, new_field, property_node, retarget_instances,
    set_instance_reference, set_pin_uuid, yes_no,
};
use crate::sexpr::{list, num, quoted, sym, tagged};

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
        self.items().iter().any(|item| match item {
            Item::Symbol(s) => s.uuid == uuid || s.pin_uuids.values().any(|u| u == uuid),
            Item::Wire(w) => w.uuid == uuid,
            Item::Junction(j) => j.uuid == uuid,
            Item::NoConnect(n) => n.uuid == uuid,
            Item::Label(l) => l.uuid == uuid,
            Item::Text(t) => t.uuid == uuid,
            Item::Sheet(s) => s.uuid == uuid || s.pins.iter().any(|p| p.uuid == uuid),
            Item::LibSymbols(_) => false,
            Item::Other(raw) => crate::sexpr::child_text(&raw.node, "uuid") == Some(uuid),
        })
    }

    /// Insert after the last item of the same kind, else before the trailing
    /// blocks, so the file keeps KiCAD's grouping.
    fn insert_item(&mut self, item: Item) {
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

    /// Set a symbol property, creating it hidden at the symbol's origin if it is
    /// not there yet.
    ///
    /// Setting `Reference` also renames this sheet's own `(instances)` entry.
    /// A sheet placed several times in a hierarchy has no such entry — its
    /// references belong to the parent paths — so renaming one is refused
    /// rather than flattening the table.
    pub fn set_field(&mut self, id: &str, name: &str, value: &str) -> Result<()> {
        let uuid = self.uuid_of(id)?;
        let sheet_path = self.sheet_path();
        if name == "Reference" && !self.owns_annotation(&uuid) {
            return Err(Error::ForeignInstances(id.to_string()));
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
        if name == "Reference" {
            set_instance_reference(&mut symbol.raw.node, &sheet_path, value);
        }
        drop(symbol);
        self.mark_edited();
        Ok(())
    }

    /// Place a new symbol at unit 1, embedding its library definition first.
    ///
    /// Refused on a sheet the hierarchy places more than once: each placement
    /// needs its own reference, and one call cannot say what the others are.
    /// Returns the new symbol's UUID.
    pub fn add_symbol(
        &mut self,
        lib_id: &str,
        refdes: &str,
        value: &str,
        at: Pose,
        source: &SymbolSource,
    ) -> Result<String> {
        if self
            .symbols()
            .any(|s| instance_paths(s.retained().node()) > 1)
        {
            return Err(Error::ReInstantiatedSheet);
        }
        self.ensure_lib_symbol(lib_id, source)?;
        let uuid = self.derive_uuid("symbol", &format!("{lib_id}|{refdes}"));
        // One `(pin …)` uuid per pin of the unit being placed, as KiCAD writes.
        let pins: Vec<String> = self
            .lib_symbols()
            .and_then(|libs| crate::pins::resolve(libs, lib_id))
            .map(|def| crate::pins::pin_numbers(def, 1, 1))
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
            tagged("unit", vec![num(1.0)]),
            tagged("exclude_from_sim", vec![yes_no(false)]),
            tagged("in_bom", vec![yes_no(true)]),
            tagged("on_board", vec![yes_no(true)]),
            tagged("dnp", vec![yes_no(false)]),
            tagged("uuid", vec![quoted(uuid.clone())]),
            property_node(
                "Reference",
                refdes,
                Pose::new(at.x, at.y - 2.54, 0.0),
                false,
            ),
            property_node("Value", value, Pose::new(at.x, at.y + 2.54, 0.0), false),
            property_node("Footprint", "", Pose::new(at.x, at.y, 0.0), true),
            property_node("Datasheet", "", Pose::new(at.x, at.y, 0.0), true),
            property_node("Description", "", Pose::new(at.x, at.y, 0.0), true),
        ];
        children.extend(pin_nodes);
        children.push(self.instances_node(refdes));

        self.insert_item(Item::Symbol(SymbolInst::decode(&list(children))));
        Ok(uuid)
    }

    /// Remove a symbol and every field it owned.
    pub fn remove_symbol(&mut self, id: &str) -> Result<()> {
        let uuid = self.uuid_of(id)?;
        self.items_mut()
            .retain(|item| !matches!(item, Item::Symbol(s) if s.uuid == uuid));
        self.mark_edited();
        Ok(())
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
                vec![tagged(
                    "font",
                    vec![tagged("size", vec![num(1.27), num(1.27)])],
                )],
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
    pub fn add_no_connect(&mut self, at: Point2) -> String {
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

    /// Merge only `source`'s drawing — wires, junctions, no-connects, labels and
    /// text — leaving its symbols behind. What a re-wire of symbols this document
    /// already holds needs.
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
        if symbols {
            self.union_lib_symbols(source);
        }
        let sheet_path = self.sheet_path();
        let mut adopted = Vec::new();
        for item in source.items().to_vec() {
            match item {
                Item::Symbol(symbol) if symbols => {
                    let symbol = self.regraft_symbol(symbol, &sheet_path);
                    adopted.push(symbol.uuid.clone());
                    self.insert_item(Item::Symbol(symbol));
                }
                Item::Wire(_)
                | Item::Junction(_)
                | Item::NoConnect(_)
                | Item::Label(_)
                | Item::Text(_) => {
                    let item = self.regraft(item);
                    self.insert_item(item);
                }
                Item::Symbol(_) | Item::Sheet(_) | Item::LibSymbols(_) | Item::Other(_) => {}
            }
        }
        Ok(adopted)
    }

    /// Drop the drawing items — wires, junctions, no-connects, labels, text —
    /// that `keep` rejects. Symbols, sheets and the header sections are never
    /// offered to it. Returns how many items were removed.
    pub fn retain_drawing(&mut self, mut keep: impl FnMut(&Item) -> bool) -> usize {
        let before = self.items().len();
        self.items_mut().retain(|item| match item {
            Item::Wire(_)
            | Item::Junction(_)
            | Item::NoConnect(_)
            | Item::Label(_)
            | Item::Text(_) => keep(item),
            _ => true,
        });
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
    fn instances_node(&self, refdes: &str) -> Node {
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
                    tagged("reference", vec![quoted(refdes)]),
                    tagged("unit", vec![num(1.0)]),
                ]),
            ])],
        )
    }
}
