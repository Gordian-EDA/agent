//! Production [`symbol_contract::SymbolProvider`] backed by the installed
//! KiCAD symbol libraries.
//!
//! Libraries are loaded lazily, one `.kicad_sym` file per referenced lib,
//! and looked-up [`SymbolMeta`]s are memoized. The trait hands out
//! `Option<&SymbolMeta>`, so memoized metas need stable addresses across an
//! append-only cache: [`elsa::sync::FrozenMap`] (boxed values, interior
//! mutability, no `unsafe` here, nothing leaked) provides exactly that.
//!
//! The provider is `Send + Sync` (sync `FrozenMap` + `Mutex`'d lib cache) so
//! tool execution can run on a blocking thread pool without freezing a UI.

use std::collections::HashMap;
use std::sync::Mutex;

use symbol_contract::{PinDir, PinMeta, PinType, SymbolMeta, SymbolProvider};
use elsa::sync::FrozenMap;

use kicad_cli_rs::env::KicadEnv;
use crate::symlib::SymbolLib;

/// Maximum levenshtein distance for a name to qualify as a suggestion.
const SUGGEST_MAX_DISTANCE: usize = 6;
/// Maximum number of suggestions returned.
const SUGGEST_LIMIT: usize = 3;

/// Symbol provider over a detected KiCAD installation.
pub struct RealSymbolProvider {
    env: KicadEnv,
    /// Lazily loaded libraries; `None` records a missing/unparsable lib so
    /// it is only attempted once.
    libs: Mutex<HashMap<String, Option<SymbolLib>>>,
    /// Memoized per-lib_id metadata with address stability (append-only).
    metas: FrozenMap<String, Box<SymbolMeta>>,
}

impl RealSymbolProvider {
    pub fn new(env: KicadEnv) -> Self {
        Self {
            env,
            libs: Mutex::new(HashMap::new()),
            metas: FrozenMap::new(),
        }
    }

    /// Run `f` against the named library, loading it on first reference.
    /// Returns `None` if the library does not exist or fails to parse.
    ///
    /// The closure must not reenter the provider — the lock on the lib cache
    /// is held across `f`.
    fn with_lib<R>(&self, lib: &str, f: impl FnOnce(&SymbolLib) -> R) -> Option<R> {
        let mut libs = self.libs.lock().expect("symbol lib cache poisoned");
        let slot = libs.entry(lib.to_string()).or_insert_with(|| {
            let path = self.env.symbol_dir.join(format!("{lib}.kicad_sym"));
            SymbolLib::load(&path).ok()
        });
        slot.as_ref().map(f)
    }
}

impl SymbolProvider for RealSymbolProvider {
    fn symbol(&self, lib_id: &str) -> Option<&SymbolMeta> {
        if let Some(meta) = self.metas.get(lib_id) {
            return Some(meta);
        }
        // The `label:global` net-label marker is not a real KiCAD library symbol —
        // synthesise a single-pin meta so the compiler validates/resolves it like any
        // part. The floorplan engine skips it in `gather` (no geometry) and draws the
        // net's global-label port pennant instead.
        if lib_id == "label:global" {
            let meta = SymbolMeta {
                pins: vec![PinMeta {
                    number: "1".into(),
                    name: "~".into(),
                    etype: PinType::Passive,
                    dir: PinDir::Passive,
                    unit: 1,
                }],
            };
            return Some(self.metas.insert(lib_id.to_string(), Box::new(meta)));
        }
        let (lib, name) = lib_id.split_once(':')?;
        let meta = self.with_lib(lib, |l| l.symbol(name).cloned())??;
        Some(self.metas.insert(lib_id.to_string(), Box::new(meta)))
    }

    fn suggest(&self, lib_id: &str) -> Vec<String> {
        let Some((lib, name)) = lib_id.split_once(':') else {
            return Vec::new();
        };
        let needle = name.to_lowercase();
        self.with_lib(lib, |l| {
            let mut hits: Vec<(usize, &str)> = l
                .names()
                .map(|n| (strsim::levenshtein(&needle, &n.to_lowercase()), n))
                .filter(|(d, _)| *d <= SUGGEST_MAX_DISTANCE)
                .collect();
            hits.sort();
            hits.into_iter()
                .take(SUGGEST_LIMIT)
                .map(|(_, n)| format!("{lib}:{n}"))
                .collect()
        })
        .unwrap_or_default()
    }
}
