//! [`SchDoc`]: the document itself — parse, query, snapshot, write.

use std::path::Path;

use kiutils_sexpr::parse_one;

use crate::error::{Error, Result};
use crate::model::{Item, Label, LibSymbols, SymbolInst, Wire};
use crate::sexpr::{self, print};

/// The header sections KiCAD writes before the sheet content, in order.
const HEADER: [&str; 7] = [
    "version",
    "generator",
    "generator_version",
    "uuid",
    "paper",
    "title_block",
    "lib_symbols",
];

/// The sections KiCAD writes after it; new content goes before them.
const TRAILER: [&str; 2] = ["sheet_instances", "embedded_fonts"];

/// Whether a top-level head belongs to the file's trailing sections.
pub(crate) fn is_trailer(head: &str) -> bool {
    TRAILER.contains(&head)
}

/// Handle to a document state captured by [`SchDoc::snapshot`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SnapshotId(usize);

/// A `.kicad_sch` file held open for editing.
///
/// Items live in document order. Anything the typed model does not decode is
/// retained as [`Item::Other`], and any item an edit has not touched is written
/// back from the exact bytes it was parsed from — so an untouched file
/// round-trips byte-identically and an edited one differs only where it should.
#[derive(Debug, Clone)]
pub struct SchDoc {
    source: String,
    items: Vec<Item>,
    snapshots: Vec<(Vec<Item>, bool)>,
    edited: bool,
    /// KiCAD does not always terminate the file with a newline; match it.
    trailing_newline: bool,
}

impl SchDoc {
    /// Parse schematic text.
    pub fn parse(text: &str) -> Result<SchDoc> {
        let cst = parse_one(text)?;
        let root = &cst.nodes[0];
        match sexpr::head(root) {
            Some("kicad_sch") => {}
            other => return Err(Error::NotSchematic(other.unwrap_or("<atom>").to_string())),
        }
        let items = sexpr::items(root)[1..].iter().map(Item::decode).collect();
        Ok(SchDoc {
            source: text.to_string(),
            items,
            snapshots: Vec::new(),
            edited: false,
            trailing_newline: text.ends_with('\n'),
        })
    }

    /// Read and parse a `.kicad_sch` file.
    pub fn read(path: impl AsRef<Path>) -> Result<SchDoc> {
        SchDoc::parse(&std::fs::read_to_string(path)?)
    }

    /// Render the document back to schematic text.
    pub fn to_text(&self) -> String {
        let mut out = String::with_capacity(self.source.len() + 1024);
        out.push_str("(kicad_sch\n");
        for item in &self.items {
            match item.pristine_span() {
                Some(span) => {
                    out.push('\t');
                    out.push_str(&self.source[span.start..span.end]);
                    out.push('\n');
                }
                None => print(&item.encode(), 1, &mut out),
            }
        }
        out.push(')');
        if self.trailing_newline {
            out.push('\n');
        }
        out
    }

    /// Write the document to `path`, collecting `(lib_symbols)` entries that
    /// edits left unreferenced.
    pub fn write(&mut self, path: impl AsRef<Path>) -> Result<()> {
        if self.edited {
            self.gc_lib_symbols();
        }
        std::fs::write(path, self.to_text())?;
        Ok(())
    }

    /// Whether any mutator has run on this document.
    pub fn is_edited(&self) -> bool {
        self.edited
    }

    pub(crate) fn mark_edited(&mut self) {
        self.edited = true;
    }

    /// This sheet's own `(instances)` path, `"/" + root_uuid`. A symbol placed
    /// here carries its reference under that path; a sheet the hierarchy
    /// instantiates elsewhere carries none.
    pub fn sheet_path(&self) -> String {
        format!("/{}", self.root_uuid())
    }

    /// The schematic's root UUID, which identifies the sheet in `(instances)`
    /// paths and seeds every UUID this crate derives.
    pub fn root_uuid(&self) -> &str {
        self.items
            .iter()
            .find_map(|item| match item {
                Item::Other(raw) if sexpr::head(&raw.node) == Some("uuid") => {
                    sexpr::items(&raw.node).get(1).and_then(sexpr::text)
                }
                _ => None,
            })
            .unwrap_or_default()
    }

    /// Every top-level item, in document order.
    pub fn items(&self) -> &[Item] {
        &self.items
    }

    pub(crate) fn items_mut(&mut self) -> &mut Vec<Item> {
        &mut self.items
    }

    /// Placed symbols in document order.
    pub fn symbols(&self) -> impl Iterator<Item = &SymbolInst> {
        self.items.iter().filter_map(|item| match item {
            Item::Symbol(s) => Some(s),
            _ => None,
        })
    }

    /// Wires in document order.
    pub fn wires(&self) -> impl Iterator<Item = &Wire> {
        self.items.iter().filter_map(|item| match item {
            Item::Wire(w) => Some(w),
            _ => None,
        })
    }

    /// Labels of every scope, in document order.
    pub fn labels(&self) -> impl Iterator<Item = &Label> {
        self.items.iter().filter_map(|item| match item {
            Item::Label(l) => Some(l),
            _ => None,
        })
    }

    /// The embedded symbol definitions, if the file has a `(lib_symbols …)`.
    pub fn lib_symbols(&self) -> Option<&LibSymbols> {
        self.items.iter().find_map(|item| match item {
            Item::LibSymbols(l) => Some(l),
            _ => None,
        })
    }

    pub(crate) fn lib_symbols_mut(&mut self) -> &mut LibSymbols {
        if !self.items.iter().any(|i| matches!(i, Item::LibSymbols(_))) {
            let at = self
                .items
                .iter()
                .rposition(|i| HEADER.contains(&i.head()))
                .map_or(0, |i| i + 1);
            self.items.insert(at, Item::LibSymbols(LibSymbols::default()));
        }
        self.items
            .iter_mut()
            .find_map(|item| match item {
                Item::LibSymbols(l) => Some(l),
                _ => None,
            })
            .expect("just inserted")
    }

    /// The symbol with this UUID.
    pub fn symbol(&self, uuid: &str) -> Option<&SymbolInst> {
        self.symbols().find(|s| s.uuid == uuid)
    }

    /// The symbol carrying this reference designator.
    pub fn symbol_by_ref(&self, refdes: &str) -> Option<&SymbolInst> {
        self.symbols().find(|s| s.refdes() == refdes)
    }

    pub(crate) fn symbol_mut(&mut self, uuid: &str) -> Result<&mut SymbolInst> {
        self.items
            .iter_mut()
            .find_map(|item| match item {
                Item::Symbol(s) if s.uuid == uuid => Some(s),
                _ => None,
            })
            .ok_or_else(|| Error::UnknownSymbol(uuid.to_string()))
    }

    /// Capture the current state so [`Self::restore`] can come back to it.
    pub fn snapshot(&mut self) -> SnapshotId {
        self.snapshots.push((self.items.clone(), self.edited));
        SnapshotId(self.snapshots.len() - 1)
    }

    /// Restore a state captured by [`Self::snapshot`]. Snapshots stay valid
    /// after restoring, so a caller can bounce between two states.
    pub fn restore(&mut self, id: SnapshotId) -> Result<()> {
        let (items, edited) = self
            .snapshots
            .get(id.0)
            .ok_or(Error::UnknownSnapshot(id.0))?
            .clone();
        self.items = items;
        self.edited = edited;
        Ok(())
    }
}
