//! Mutators. Every one of them invalidates only the items it rewrote, so the
//! rest of the file is still written back from its original bytes.

use geom::{Point2, stable_uuid};
use kiutils_sexpr::Node;

use crate::doc::SchDoc;
use crate::error::{Error, Result};
use crate::libsyms::SymbolSource;
use crate::model::{
    Item, Junction, Label, LabelKind, Mirror, NoConnect, Pose, Retained, SymbolInst, Wire,
    new_field, property_node, set_instance_reference, yes_no,
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

    fn uuid_taken(&self, uuid: &str) -> bool {
        self.items().iter().any(|item| match item {
            Item::Symbol(s) => s.uuid == uuid,
            Item::Wire(w) => w.uuid == uuid,
            Item::Junction(j) => j.uuid == uuid,
            Item::NoConnect(n) => n.uuid == uuid,
            Item::Label(l) => l.uuid == uuid,
            Item::Text(t) => t.uuid == uuid,
            Item::Sheet(s) => s.uuid == uuid,
            _ => false,
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
    pub fn move_symbol(&mut self, refdes: &str, x: f64, y: f64) -> Result<()> {
        let uuid = self.uuid_of(refdes)?;
        let symbol = self.symbol_mut(&uuid)?;
        let (dx, dy) = (x - symbol.at.x, y - symbol.at.y);
        symbol.at.x = x;
        symbol.at.y = y;
        for field in symbol.fields.values_mut() {
            if let Some(at) = field.at.as_mut() {
                at.x += dx;
                at.y += dy;
            }
        }
        symbol.raw.touch();
        self.mark_edited();
        Ok(())
    }

    /// Set a symbol's rotation and mirroring, leaving its position alone.
    pub fn set_symbol_orientation(&mut self, refdes: &str, rot: f64, mirror: Mirror) -> Result<()> {
        let uuid = self.uuid_of(refdes)?;
        let symbol = self.symbol_mut(&uuid)?;
        symbol.at.rot = rot;
        symbol.mirror = mirror;
        symbol.raw.touch();
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
    pub fn set_field(&mut self, refdes: &str, name: &str, value: &str) -> Result<()> {
        let uuid = self.uuid_of(refdes)?;
        let sheet_path = self.sheet_path();
        let symbol = self.symbol_mut(&uuid)?;
        let origin = symbol.at;
        match symbol.fields.get_mut(name) {
            Some(field) => field.value = value.to_string(),
            None => {
                let field = new_field(name, value, Pose::new(origin.x, origin.y, 0.0), true);
                symbol.fields.insert(name.to_string(), field);
            }
        }
        if name == "Reference"
            && has_instances(&symbol.raw.node)
            && !set_instance_reference(&mut symbol.raw.node, &sheet_path, value)
        {
            return Err(Error::ForeignInstances(refdes.to_string()));
        }
        symbol.raw.touch();
        self.mark_edited();
        Ok(())
    }

    /// Place a new symbol, embedding its library definition first.
    ///
    /// Returns the new symbol's UUID.
    pub fn add_symbol(
        &mut self,
        lib_id: &str,
        refdes: &str,
        value: &str,
        at: Pose,
        source: &SymbolSource,
    ) -> Result<String> {
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
            property_node("Reference", refdes, Pose::new(at.x, at.y - 2.54, 0.0), false),
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
    pub fn remove_symbol(&mut self, refdes: &str) -> Result<()> {
        let uuid = self.uuid_of(refdes)?;
        self.items_mut()
            .retain(|item| !matches!(item, Item::Symbol(s) if s.uuid == uuid));
        self.mark_edited();
        Ok(())
    }

    /// Draw a wire between two points. Returns its UUID.
    pub fn add_wire(&mut self, from: Point2, to: Point2) -> String {
        let uuid = self.derive_uuid(
            "wire",
            &format!("{},{}->{},{}", from.x, from.y, to.x, to.y),
        );
        let node = list(vec![
            sym("wire"),
            tagged(
                "pts",
                vec![
                    tagged("xy", vec![num(from.x), num(from.y)]),
                    tagged("xy", vec![num(to.x), num(to.y)]),
                ],
            ),
            tagged("stroke", vec![tagged("width", vec![num(0.0)]), tagged("type", vec![sym("default")])]),
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

    fn uuid_of(&self, refdes: &str) -> Result<String> {
        self.symbol_by_ref(refdes)
            .map(|s| s.uuid.clone())
            .ok_or_else(|| Error::UnknownReference(refdes.to_string()))
    }

    /// Clone the project/path shape an existing symbol uses, so a new symbol
    /// lands in the same hierarchy; fall back to this sheet's own root path.
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

/// Whether a symbol carries an `(instances …)` table at all.
fn has_instances(node: &Node) -> bool {
    crate::sexpr::child(node, "instances").is_some()
}
