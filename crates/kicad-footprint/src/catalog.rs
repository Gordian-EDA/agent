//! The footprint library catalog: discovery, inventory, lazy parsing, and
//! search over the `.pretty` libraries reachable from one or more roots.
//!
//! [`FootprintCatalog`] is the crate's primary entry point. It is the inventory
//! and resolver — not merely a ranking index — so it owns library/entry
//! enumeration, raw-source access, parsed-footprint loading (memoized), fuzzy
//! search, and did-you-mean suggestions. Platform discovery stays one layer
//! down in [`kicad`]; the catalog only ever indexes a *resolved* root.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::discover::discover;
use crate::error::{Error, Result};
use crate::id::{FootprintId, LibraryId};
use crate::search::{self, FootprintSearchHit, SearchQuery};
use crate::types::Footprint;

const SUGGEST_LIMIT: usize = 3;

/// One discovered `.pretty` library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FootprintLibrary {
    id: LibraryId,
    path: PathBuf,
}

impl FootprintLibrary {
    pub(crate) fn new(id: LibraryId, path: PathBuf) -> Self {
        FootprintLibrary { id, path }
    }

    /// The library nickname.
    pub fn id(&self) -> &LibraryId {
        &self.id
    }

    /// The `.pretty` directory path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// One discovered footprint: a file-backed handle yielding raw source or a
/// parsed [`Footprint`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FootprintEntry {
    id: FootprintId,
    path: PathBuf,
}

impl FootprintEntry {
    pub(crate) fn new(id: FootprintId, path: PathBuf) -> Self {
        FootprintEntry { id, path }
    }

    /// The fully-qualified id.
    pub fn id(&self) -> &FootprintId {
        &self.id
    }

    /// The backing `.kicad_mod` file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The raw `.kicad_mod` source text.
    pub fn source(&self) -> Result<String> {
        std::fs::read_to_string(&self.path).map_err(|e| Error::Io {
            path: self.path.clone(),
            source: e,
        })
    }

    /// Parse the footprint, tagging it with this entry's [`FootprintId`].
    pub fn parse(&self) -> Result<Footprint> {
        let mut fp = Footprint::from_file(&self.path)?;
        fp.id = Some(self.id.clone());
        Ok(fp)
    }
}

/// An entry plus its pre-normalized searchable texts (full id and bare name)
/// and canonical lib-id string (the deterministic search tie-break key).
struct Indexed {
    entry: FootprintEntry,
    normalized: String,
    normalized_name: String,
    lib_id: String,
}

/// Configures and builds a [`FootprintCatalog`] from one or more roots.
#[derive(Debug, Default, Clone)]
pub struct FootprintCatalogBuilder {
    roots: Vec<PathBuf>,
}

impl FootprintCatalogBuilder {
    /// An empty builder with no roots.
    pub fn new() -> Self {
        FootprintCatalogBuilder::default()
    }

    /// Add a footprint root (a directory of `.pretty` libraries).
    pub fn root(mut self, root: impl Into<PathBuf>) -> Self {
        self.roots.push(root.into());
        self
    }

    /// Scan every configured root and build the catalog. When roots overlap,
    /// the first occurrence of an id wins.
    pub fn build(self) -> Result<FootprintCatalog> {
        let mut libraries: Vec<FootprintLibrary> = Vec::new();
        let mut indexed: Vec<Indexed> = Vec::new();
        let mut by_id: HashMap<FootprintId, usize> = HashMap::new();

        for root in &self.roots {
            let (libs, entries) = discover(root)?;
            for lib in libs {
                if !libraries.iter().any(|l| l.id() == lib.id()) {
                    libraries.push(lib);
                }
            }
            for entry in entries {
                if by_id.contains_key(entry.id()) {
                    continue;
                }
                let lib_id = entry.id().to_string();
                let normalized = search::normalize(&lib_id);
                let normalized_name = search::normalize(entry.id().name());
                by_id.insert(entry.id().clone(), indexed.len());
                indexed.push(Indexed {
                    entry,
                    normalized,
                    normalized_name,
                    lib_id,
                });
            }
        }

        libraries.sort_by(|a, b| a.id().cmp(b.id()));

        Ok(FootprintCatalog {
            libraries,
            indexed,
            by_id,
            cache: Mutex::new(HashMap::new()),
        })
    }
}

/// Inventory of, and resolver for, footprints across one or more roots.
///
/// `Send + Sync`: lookups take `&self` and memoize behind an internal mutex.
pub struct FootprintCatalog {
    libraries: Vec<FootprintLibrary>,
    indexed: Vec<Indexed>,
    by_id: HashMap<FootprintId, usize>,
    cache: Mutex<HashMap<FootprintId, Footprint>>,
}

impl FootprintCatalog {
    /// Start configuring a catalog.
    pub fn builder() -> FootprintCatalogBuilder {
        FootprintCatalogBuilder::new()
    }

    /// Build from a single explicit footprint root.
    pub fn from_root(root: impl AsRef<Path>) -> Result<Self> {
        FootprintCatalogBuilder::new().root(root.as_ref()).build()
    }

    /// Number of indexed footprints.
    pub fn len(&self) -> usize {
        self.indexed.len()
    }

    /// Whether the catalog indexed no footprints.
    pub fn is_empty(&self) -> bool {
        self.indexed.is_empty()
    }

    /// Number of discovered `.pretty` libraries.
    pub fn library_count(&self) -> usize {
        self.libraries.len()
    }

    /// The discovered libraries, sorted by nickname.
    pub fn libraries(&self) -> impl Iterator<Item = &FootprintLibrary> {
        self.libraries.iter()
    }

    /// Every indexed footprint entry.
    pub fn entries(&self) -> impl Iterator<Item = &FootprintEntry> {
        self.indexed.iter().map(|i| &i.entry)
    }

    /// Entries belonging to `library`.
    pub fn entries_in<'a>(
        &'a self,
        library: &'a LibraryId,
    ) -> impl Iterator<Item = &'a FootprintEntry> {
        self.indexed
            .iter()
            .map(|i| &i.entry)
            .filter(move |e| e.id().library() == library)
    }

    /// Whether `id` is indexed.
    pub fn contains(&self, id: &FootprintId) -> bool {
        self.by_id.contains_key(id)
    }

    /// The entry for `id`, or [`Error::NotFound`].
    pub fn entry(&self, id: &FootprintId) -> Result<&FootprintEntry> {
        self.by_id
            .get(id)
            .map(|&i| &self.indexed[i].entry)
            .ok_or_else(|| Error::NotFound { id: id.clone() })
    }

    /// The raw `.kicad_mod` source for `id`.
    pub fn source(&self, id: &FootprintId) -> Result<String> {
        self.entry(id)?.source()
    }

    /// Parse (and memoize) the footprint for `id`.
    ///
    /// Returns [`Error::NotFound`] for an unknown id and [`Error::Parse`] /
    /// [`Error::Io`] for a known-but-unreadable footprint — the two are never
    /// conflated. Only successful parses are cached, so a transient parse error
    /// keeps its diagnostic on retry.
    pub fn footprint(&self, id: &FootprintId) -> Result<Footprint> {
        {
            let cache = self.cache.lock().expect("footprint cache poisoned");
            if let Some(fp) = cache.get(id) {
                return Ok(fp.clone());
            }
        }
        let fp = self.entry(id)?.parse()?;
        self.cache
            .lock()
            .expect("footprint cache poisoned")
            .insert(id.clone(), fp.clone());
        Ok(fp)
    }

    /// "Did-you-mean" ids for unresolved raw text, highest fuzzy score first.
    ///
    /// Accepting raw text gives malformed and well-formed ids the same ranking
    /// path. Full ids and their component token runs are scored with
    /// `SkimMatcherV2`, which tolerates an invented library or package prefix
    /// while preserving the meaningful family and dimension tokens.
    pub fn suggest(&self, text: &str) -> Vec<FootprintId> {
        let normalized = search::normalize(text);
        let normalized_name = text
            .split_once(':')
            .map(|(_, name)| search::normalize(name));
        search::rank_suggestions(
            &self.indexed,
            &normalized,
            normalized_name.as_deref(),
            SUGGEST_LIMIT,
            |i| i.normalized.as_str(),
            |i| i.normalized_name.as_str(),
            |i| i.lib_id.as_str(),
        )
        .into_iter()
        .map(|i| self.indexed[i].entry.id().clone())
        .collect()
    }

    /// The best matches for `query`, best first. Pad counts are resolved lazily
    /// for the returned hits only, and are `None` when a hit fails to parse.
    pub fn search(&self, query: impl Into<SearchQuery>) -> Vec<FootprintSearchHit> {
        let query = query.into();
        let needle = search::normalize(query.query());
        if needle.is_empty() {
            return Vec::new();
        }
        search::rank(
            &self.indexed,
            &needle,
            query.limit_value(),
            |i| i.normalized.as_str(),
            |i| i.lib_id.as_str(),
        )
        .into_iter()
        .map(|i| {
            let id = self.indexed[i].entry.id().clone();
            let score = search::fuzzy_score(&self.indexed[i].normalized, &needle);
            let pad_count = self.footprint(&id).ok().map(|fp| fp.pad_count());
            FootprintSearchHit {
                id,
                score,
                pad_count,
            }
        })
        .collect()
    }
}
