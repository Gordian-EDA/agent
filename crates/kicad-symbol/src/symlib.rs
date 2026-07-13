//! Parsing of `.kicad_sym` symbol libraries into `crate::SymbolMeta`.
//!
//! Backend: `kiutils_kicad`. Its AST nests `<NAME>_<unit>_<bodystyle>`
//! sub-symbol blocks inside the parent [`kiutils_kicad::Symbol`] as `units`,
//! so unit blocks are merged structurally and never appear as top-level
//! symbols. We add `extends` chain resolution (arbitrary depth, cycle-safe
//! via a visited set) and unit-number extraction from the sub-block names
//! on top.
//!
//! [`read_lib`] and [`read_lib_dir`] are the entry points; [`crate::SymbolTable`]
//! owns the per-library cache and the `Lib:Name` lookup/suggest layer on top.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};

use crate::types::{PinDir, PinMeta, PinType, SymbolMeta};
use kiutils_kicad::{SymPin, Symbol, SymbolLibFile};

/// Load and fully resolve one `.kicad_sym` file into `bare name → SymbolMeta`
/// (extends chains followed, multi-unit pins merged, sub-blocks hidden).
pub(crate) fn read_lib(path: &Path) -> io::Result<HashMap<String, SymbolMeta>> {
    let mut reader = LibReader::default();
    reader.add_file(path)?;
    Ok(reader.finish())
}

/// Load a KiCad 10 split library directory (`Foo.kicad_symdir`) as one logical
/// library so `extends` chains can resolve across sibling symbol files.
pub(crate) fn read_lib_dir(path: &Path) -> io::Result<HashMap<String, SymbolMeta>> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(path)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "kicad_sym"))
        .collect();
    paths.sort();

    let mut reader = LibReader::default();
    for path in paths {
        reader.add_file(&path)?;
    }
    Ok(reader.finish())
}

#[derive(Default)]
struct LibReader {
    raw: HashMap<String, RawSymbol>,
}

#[derive(Clone, Default)]
struct RawSymbol {
    pins: Vec<PinMeta>,
    extends: Option<String>,
    description: Option<String>,
    datasheet: Option<String>,
    footprint: Option<String>,
    keywords: Option<String>,
}

impl LibReader {
    fn add_file(&mut self, path: &Path) -> io::Result<()> {
        let doc = SymbolLibFile::read(path).map_err(|e| match e {
            kiutils_kicad::Error::Io(io) => io,
            other => io::Error::new(io::ErrorKind::InvalidData, other.to_string()),
        })?;

        // First pass: own pins/properties + extends target per top-level symbol.
        self.raw.extend(doc.ast().symbols.iter().filter_map(|sym| {
            let name = sym.name.clone()?;
            let property = |key: &str| {
                sym.properties
                    .iter()
                    .find(|property| property.key.eq_ignore_ascii_case(key))
                    .map(|property| property.value.clone())
                    .filter(|value| !value.trim().is_empty())
            };
            Some((
                name,
                RawSymbol {
                    pins: own_pins(sym),
                    extends: sym.extends.clone(),
                    description: property("Description"),
                    datasheet: property("Datasheet"),
                    footprint: property("Footprint"),
                    keywords: property("ki_keywords"),
                },
            ))
        }));
        Ok(())
    }

    fn finish(self) -> HashMap<String, SymbolMeta> {
        // Second pass: resolve extends chains.
        self.raw
            .keys()
            .map(|name| {
                let meta = resolve_meta(&self.raw, name, &mut HashSet::new());
                (name.clone(), meta)
            })
            .collect()
    }
}

/// Pins owned by a symbol: direct pins plus pins of all
/// `<NAME>_<unit>_<bodystyle>` sub-blocks, tagged with their unit number.
fn own_pins(sym: &Symbol) -> Vec<PinMeta> {
    let mut pins: Vec<PinMeta> = sym.pins.iter().filter_map(|p| pin_meta(p, 1)).collect();
    for unit in &sym.units {
        let unit_no = unit
            .name
            .as_deref()
            .and_then(unit_number)
            // Unit 0 holds graphics / pins common to all units; PinMeta units
            // are 1-based, so fold it into unit 1.
            .map_or(1, |u| u.max(1));
        pins.extend(unit.pins.iter().filter_map(|p| pin_meta(p, unit_no)));
    }
    pins
}

/// Extract the unit number from a sub-block name like `STM32H743VITx_1_1`
/// (`<NAME>_<unit>_<bodystyle>`).
fn unit_number(block_name: &str) -> Option<u8> {
    let mut parts = block_name.rsplitn(3, '_');
    let _bodystyle = parts.next()?;
    parts.next()?.parse().ok()
}

fn pin_meta(pin: &SymPin, unit: u8) -> Option<PinMeta> {
    let etype = match pin.electrical_type.as_deref() {
        Some("power_in") => PinType::PowerInput,
        Some("power_out") => PinType::PowerOutput,
        Some("passive") => PinType::Passive,
        _ => PinType::Other,
    };
    // Preserve the full signal DIRECTION (PinType collapses input/output → Other).
    let dir = match pin.electrical_type.as_deref() {
        Some("input") => PinDir::In,
        Some("output") => PinDir::Out,
        Some("bidirectional") | Some("tri_state") => PinDir::Bidir,
        // Open-collector/emitter drive the net low — treat as a driver (Out).
        Some("open_collector") | Some("open_emitter") => PinDir::Out,
        Some("power_in") | Some("power_out") => PinDir::Power,
        Some("passive") => PinDir::Passive,
        _ => PinDir::Unknown,
    };
    Some(PinMeta {
        number: pin.number.clone()?,
        name: pin.name.clone()?,
        etype,
        dir,
        unit,
    })
}

/// Resolve a symbol's metadata, following `extends` for inherited properties
/// and for pins when the child has none of its own.
///
/// Chains of arbitrary depth are supported; `visited` guarantees termination
/// on `extends` cycles. A cycle or a missing parent is not an error: the
/// symbol keeps its own metadata, and a pinless symbol may therefore resolve
/// to an honest empty pin set for downstream unknown-pin diagnostics.
fn resolve_meta<'a>(
    raw: &'a HashMap<String, RawSymbol>,
    name: &'a str,
    visited: &mut HashSet<&'a str>,
) -> SymbolMeta {
    if !visited.insert(name) {
        return SymbolMeta::default(); // extends cycle: terminate deliberately
    }
    let Some(symbol) = raw.get(name) else {
        return SymbolMeta::default(); // missing parent
    };
    let parent = symbol
        .extends
        .as_deref()
        .map(|parent| resolve_meta(raw, parent, visited))
        .unwrap_or_default();
    SymbolMeta {
        pins: if symbol.pins.is_empty() {
            parent.pins
        } else {
            symbol.pins.clone()
        },
        description: symbol.description.clone().or(parent.description),
        datasheet: symbol.datasheet.clone().or(parent.datasheet),
        footprint: symbol.footprint.clone().or(parent.footprint),
        keywords: symbol.keywords.clone().or(parent.keywords),
    }
}
