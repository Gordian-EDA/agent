//! The embedded `(lib_symbols)` set: splicing definitions in, and collecting
//! the ones nothing references any more.

use std::path::{Path, PathBuf};

use kicad_symbol::geometry::SymbolGeometry;
use kiutils_sexpr::parse_one;

use crate::doc::SchDoc;
use crate::error::{Error, Result};
use crate::model::{Item, Retained};

/// Where `ensure_lib_symbol` fetches definitions from: an installed KiCAD
/// symbol directory.
#[derive(Debug, Clone)]
pub struct SymbolSource {
    symbol_dir: PathBuf,
}

impl SymbolSource {
    pub fn new(symbol_dir: impl Into<PathBuf>) -> Self {
        Self {
            symbol_dir: symbol_dir.into(),
        }
    }

    pub fn symbol_dir(&self) -> &Path {
        &self.symbol_dir
    }

    /// The embeddable `(symbol "Lib:Name" …)` block for `lib_id`, with derived
    /// symbols already resolved to their parent's body.
    fn definition(&self, lib_id: &str) -> Result<kiutils_sexpr::Node> {
        let geometry = SymbolGeometry::load(&self.symbol_dir, lib_id)
            .map_err(|e| Error::Library(format!("{lib_id}: {e}")))?;
        let cst = parse_one(geometry.definition_sexpr())?;
        cst.nodes
            .into_iter()
            .next()
            .ok_or_else(|| Error::Library(format!("{lib_id}: empty definition")))
    }
}

impl SchDoc {
    /// Splice `lib_id`'s definition into `(lib_symbols)` if it is not there yet.
    ///
    /// Pin geometry for placed symbols always comes from this embedded copy, so
    /// a symbol must be ensured before it is placed.
    pub fn ensure_lib_symbol(&mut self, lib_id: &str, source: &SymbolSource) -> Result<()> {
        if self.lib_symbols().is_some_and(|l| l.contains(lib_id)) {
            return Ok(());
        }
        let def = source.definition(lib_id)?;
        let libs = self.lib_symbols_mut();
        libs.defs.insert(lib_id.to_string(), Retained::owned(def));
        libs.defs.sort_keys();
        if let Some(raw) = libs.raw.as_mut() {
            raw.touch();
        }
        self.mark_edited();
        Ok(())
    }

    /// Drop `(lib_symbols)` entries no placed symbol refers to. Called for you
    /// on [`SchDoc::write`] whenever the document has been edited.
    pub fn gc_lib_symbols(&mut self) {
        let used: Vec<String> = self.symbols().map(|s| s.lib_id.clone()).collect();
        let Some(libs) = self.items_mut().iter_mut().find_map(|item| match item {
            Item::LibSymbols(l) => Some(l),
            _ => None,
        }) else {
            return;
        };
        let before = libs.defs.len();
        libs.defs.retain(|lib_id, _| used.iter().any(|u| u == lib_id));
        if libs.defs.len() != before
            && let Some(raw) = libs.raw.as_mut()
        {
            raw.touch();
        }
    }
}
