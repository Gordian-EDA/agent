//! Install-backed and in-memory symbol metadata provider.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::symlib;
use crate::types::{PinDir, PinMeta, PinType, SymbolMeta};

/// Maximum levenshtein distance for a real-library name to qualify as a suggestion.
const SUGGEST_MAX_DISTANCE: usize = 6;
/// Maximum number of suggestions returned.
const SUGGEST_LIMIT: usize = 3;

/// The single concrete symbol oracle: resolves `Lib:Name` ids to pin metadata.
///
/// Backed either by installed KiCAD symbol libraries
/// ([`from_symbol_dir`](SymbolTable::from_symbol_dir)) -- each KiCad 9 flat
/// `.kicad_sym` or KiCad 10 split `.kicad_symdir` library is parsed once on
/// first reference and cached -- or by an in-memory fixture set for tests
/// ([`mock`](SymbolTable::mock) / [`with_basics`](SymbolTable::with_basics)).
///
/// `Send + Sync` (the only interior mutability is the `Mutex`'d library cache),
/// so it can live in a long-lived context shared across blocking threads.
#[derive(Default)]
pub struct SymbolTable {
    /// Symbol directory for on-disk libraries; `None` for a pure in-memory table.
    symbol_dir: Option<PathBuf>,
    /// Lazily parsed libraries: lib name -> (bare name -> meta), or `None` when
    /// the library is missing/unparsable (recorded so it is attempted only once).
    libs: Mutex<HashMap<String, Option<HashMap<String, SymbolMeta>>>>,
    /// In-memory symbols (test fixtures), keyed by full `Lib:Name`.
    inline: HashMap<String, SymbolMeta>,
}

impl SymbolTable {
    /// A table over KiCad symbol libraries in `symbol_dir`.
    pub fn from_symbol_dir(symbol_dir: PathBuf) -> Self {
        Self {
            symbol_dir: Some(symbol_dir),
            ..Default::default()
        }
    }

    /// An empty in-memory table (no disk backing); seed it with [`mock_add`](Self::mock_add).
    pub fn mock() -> Self {
        Self::default()
    }

    /// Add an in-memory symbol (test fixture). `dir` is derived from `etype`.
    pub fn mock_add(&mut self, lib_id: &str, pins: Vec<(&str, &str, PinType, u8)>) -> &mut Self {
        let pins = pins
            .into_iter()
            .map(|(number, name, etype, unit)| PinMeta {
                number: number.into(),
                name: name.into(),
                etype,
                dir: match etype {
                    PinType::PowerInput | PinType::PowerOutput => PinDir::Power,
                    PinType::Passive => PinDir::Passive,
                    PinType::NoConnect | PinType::Other => PinDir::Unknown,
                },
                unit,
            })
            .collect();
        self.inline.insert(
            lib_id.into(),
            SymbolMeta {
                pins,
                ..Default::default()
            },
        );
        self
    }

    /// An in-memory table preloaded with Device:R/C/L/D/LED and the common power
    /// rails -- enough for most schematic tests.
    pub fn with_basics() -> Self {
        use PinType::*;
        let mut t = Self::mock();
        for id in ["Device:R", "Device:C", "Device:L"] {
            t.mock_add(id, vec![("1", "~", Passive, 1), ("2", "~", Passive, 1)]);
        }
        t.mock_add(
            "Device:D",
            vec![("1", "K", Passive, 1), ("2", "A", Passive, 1)],
        );
        t.mock_add(
            "Device:LED",
            vec![("1", "K", Passive, 1), ("2", "A", Passive, 1)],
        );
        for (id, net) in [
            ("power:GND", "GND"),
            ("power:VCC", "VCC"),
            ("power:+3V3", "+3V3"),
            ("power:+5V", "+5V"),
            ("power:+12V", "+12V"),
            ("power:VBUS", "VBUS"),
        ] {
            t.mock_add(id, vec![("1", net, PowerInput, 1)]);
        }
        t
    }

    /// Pin metadata for `lib_id` (`"Lib:Name"`), or `None` if unknown.
    pub fn symbol(&self, lib_id: &str) -> Option<SymbolMeta> {
        if let Some(meta) = self.inline.get(lib_id) {
            return Some(meta.clone());
        }
        if lib_id == "label:global" {
            return Some(global_label_meta());
        }
        let (lib, name) = lib_id.split_once(':')?;
        self.with_lib(lib, |syms| syms.get(name).cloned()).flatten()
    }

    /// Closest known `lib_id`s for an unknown one (for diagnostics).
    pub fn suggest(&self, lib_id: &str) -> Vec<String> {
        if !self.inline.is_empty() {
            return suggest_inline(&self.inline, lib_id);
        }
        let Some((lib, name)) = lib_id.split_once(':') else {
            return Vec::new();
        };
        let needle = name.to_lowercase();
        self.with_lib(lib, |syms| {
            let mut hits: Vec<(usize, &str)> = syms
                .keys()
                .map(|n| (strsim::levenshtein(&needle, &n.to_lowercase()), n.as_str()))
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

    /// Run `f` against the parsed library `lib`, loading it on first reference.
    /// `None` if there is no symbol directory, or the library is missing/unparsable.
    fn with_lib<R>(
        &self,
        lib: &str,
        f: impl FnOnce(&HashMap<String, SymbolMeta>) -> R,
    ) -> Option<R> {
        let dir = self.symbol_dir.as_ref()?;
        let mut libs = self.libs.lock().expect("symbol lib cache poisoned");
        let slot = libs
            .entry(lib.to_string())
            .or_insert_with(|| read_symbol_library(dir, lib).ok());
        slot.as_ref().map(f)
    }
}

fn read_symbol_library(dir: &Path, lib: &str) -> std::io::Result<HashMap<String, SymbolMeta>> {
    let flat = dir.join(format!("{lib}.kicad_sym"));
    if flat.is_file() {
        return symlib::read_lib(&flat);
    }
    symlib::read_lib_dir(&dir.join(format!("{lib}.kicad_symdir")))
}

fn global_label_meta() -> SymbolMeta {
    // `label:global` is not a real KiCAD library symbol. It is represented as a
    // single-pin meta so the compiler can resolve it like any other part.
    SymbolMeta {
        pins: vec![PinMeta {
            number: "1".into(),
            name: "~".into(),
            etype: PinType::Passive,
            dir: PinDir::Passive,
            unit: 1,
        }],
        ..Default::default()
    }
}

/// Suggestions over an in-memory fixture set: closest full `Lib:Name` keys,
/// surfacing only the closest distance tier (so an exact match pulls in no
/// near-neighbours).
fn suggest_inline(inline: &HashMap<String, SymbolMeta>, lib_id: &str) -> Vec<String> {
    let mut hits: Vec<(usize, &String)> = inline
        .keys()
        .map(|k| {
            (
                strsim::levenshtein(&lib_id.to_lowercase(), &k.to_lowercase()),
                k,
            )
        })
        .filter(|(d, _)| *d <= 3)
        .collect();
    hits.sort();
    let best = match hits.first() {
        Some((d, _)) => *d,
        None => return Vec::new(),
    };
    hits.into_iter()
        .take_while(|(d, _)| *d == best)
        .take(3)
        .map(|(_, k)| k.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_table_serves_symbols_and_suggestions() {
        let t = SymbolTable::with_basics();
        let r = t.symbol("Device:R").unwrap();
        assert_eq!(r.pins.len(), 2);
        assert!(t.symbol("Device:Q").is_none());
        assert_eq!(t.suggest("Device:r"), vec!["Device:R".to_string()]);
    }

    #[test]
    fn label_global_is_synthesised() {
        let t = SymbolTable::mock();
        assert_eq!(t.symbol("label:global").unwrap().pins.len(), 1);
    }
}
