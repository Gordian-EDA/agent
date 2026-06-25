//! Parsing of `.kicad_sym` symbol libraries into `crate::SymbolMeta`.
//!
//! Backend: `kiutils_kicad`. Its AST nests `<NAME>_<unit>_<bodystyle>`
//! sub-symbol blocks inside the parent [`kiutils_kicad::Symbol`] as `units`,
//! so unit blocks are merged structurally and never appear as top-level
//! symbols. We add `extends` chain resolution (arbitrary depth, cycle-safe
//! via a visited set) and unit-number extraction from the sub-block names
//! on top.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::Path;

use crate::{PinDir, PinMeta, PinType, SymbolMeta};
use kiutils_kicad::{SymPin, Symbol, SymbolLibFile};

/// A loaded `.kicad_sym` library: symbol name → merged, extends-resolved
/// pin metadata.
#[derive(Debug, Clone)]
pub struct SymbolLib {
    symbols: HashMap<String, SymbolMeta>,
}

impl SymbolLib {
    /// Load and fully resolve a symbol library file.
    pub fn load(path: &Path) -> io::Result<SymbolLib> {
        let doc = SymbolLibFile::read(path).map_err(|e| match e {
            kiutils_kicad::Error::Io(io) => io,
            other => io::Error::new(io::ErrorKind::InvalidData, other.to_string()),
        })?;

        // First pass: own pins + extends target per top-level symbol.
        let raw: HashMap<String, (Vec<PinMeta>, Option<String>)> = doc
            .ast()
            .symbols
            .iter()
            .filter_map(|sym| {
                let name = sym.name.clone()?;
                let pins = own_pins(sym);
                Some((name, (pins, sym.extends.clone())))
            })
            .collect();

        // Second pass: resolve extends chains.
        let symbols = raw
            .keys()
            .map(|name| {
                let pins = resolve_pins(&raw, name, &mut HashSet::new());
                (name.clone(), SymbolMeta { pins })
            })
            .collect();

        Ok(SymbolLib { symbols })
    }

    /// Look up a symbol by its bare name (no `Lib:` prefix).
    pub fn symbol(&self, name: &str) -> Option<&SymbolMeta> {
        self.symbols.get(name)
    }

    /// Names of all top-level symbols in the library.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.symbols.keys().map(String::as_str)
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

/// Resolve a symbol's pins, following `extends` when it has none of its own.
///
/// Chains of arbitrary depth are supported; `visited` guarantees termination
/// on `extends` cycles. A cycle or a missing parent is not an error: the
/// symbol simply keeps the pins it has (none, since we only descend past
/// pinless symbols) and downstream lookups surface unknown-pin diagnostics
/// against that honest, empty pin set.
fn resolve_pins<'a>(
    raw: &'a HashMap<String, (Vec<PinMeta>, Option<String>)>,
    name: &'a str,
    visited: &mut HashSet<&'a str>,
) -> Vec<PinMeta> {
    if !visited.insert(name) {
        return Vec::new(); // extends cycle: terminate deliberately
    }
    let Some((pins, extends)) = raw.get(name) else {
        return Vec::new(); // missing parent: keep the pins we have
    };
    if !pins.is_empty() {
        return pins.clone();
    }
    match extends {
        Some(parent) => resolve_pins(raw, parent, visited),
        None => Vec::new(),
    }
}
