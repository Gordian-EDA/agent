//! Parsing of `.kicad_sym` symbol libraries into `circuit_lang::SymbolMeta`.
//!
//! Backend: `kiutils_kicad`. Its AST nests `<NAME>_<unit>_<bodystyle>`
//! sub-symbol blocks inside the parent [`kiutils_kicad::Symbol`] as `units`,
//! so unit blocks are merged structurally and never appear as top-level
//! symbols. We add `extends` chain resolution (≤ 4 hops, cycle-safe) and
//! unit-number extraction from the sub-block names on top.

use std::collections::HashMap;
use std::io;
use std::path::Path;

use circuit_lang::{PinMeta, PinType, SymbolMeta};
use kiutils_kicad::{SymPin, Symbol, SymbolLibFile};

/// Maximum `extends` hops before giving up (guards against cycles).
const MAX_EXTENDS_HOPS: u8 = 4;

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
                let pins = resolve_pins(&raw, name, 0).unwrap_or_default();
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
    Some(PinMeta {
        number: pin.number.clone()?,
        name: pin.name.clone()?,
        etype,
        unit,
    })
}

/// Resolve a symbol's pins, following `extends` when it has none of its own.
fn resolve_pins(
    raw: &HashMap<String, (Vec<PinMeta>, Option<String>)>,
    name: &str,
    hops: u8,
) -> Option<Vec<PinMeta>> {
    if hops > MAX_EXTENDS_HOPS {
        return None; // extends cycle / runaway chain guard
    }
    let (pins, extends) = raw.get(name)?;
    if !pins.is_empty() {
        return Some(pins.clone());
    }
    match extends {
        Some(parent) => resolve_pins(raw, parent, hops + 1),
        None => Some(Vec::new()),
    }
}
