//! Resolving a component pin-map key to the symbol's physical pins.
//!
//! A key is a pin NUMBER or a pin NAME — number first, so a symbol whose pin is
//! named "2" never shadows physical pin 2. Every checker and every front end
//! resolves keys through here, so they all agree on what a key means.

use crate::model::*;
use crate::{PinMeta, SymbolMeta, SymbolTable};

/// Every physical pin `key` names. A name may cover several pins (a stacked
/// `VDD`); a number covers exactly one. Empty when the key names nothing.
pub fn resolve<'a>(meta: &'a SymbolMeta, key: &str) -> Vec<&'a PinMeta> {
    let by_number: Vec<&PinMeta> = meta.pins.iter().filter(|p| p.number == key).collect();
    if !by_number.is_empty() {
        return by_number;
    }
    if is_unnamed(key) {
        return Vec::new(); // `~` is the absence of a name, not a name every unnamed pin shares
    }
    meta.pins.iter().filter(|p| p.name == key).collect()
}

/// KiCAD writes an unnamed pin's name as `~`. It is not a name a caller can use.
pub fn is_unnamed(name: &str) -> bool {
    name.is_empty() || name == "~"
}

/// The pin name or number closest to an unresolvable `key`, for a "did you
/// mean" suggestion. `None` when nothing is within two edits.
pub fn nearest<'a>(meta: &'a SymbolMeta, key: &str) -> Option<&'a str> {
    meta.pins
        .iter()
        .flat_map(|p| [p.name.as_str(), p.number.as_str()])
        .filter(|n| !is_unnamed(n))
        .map(|n| (strsim::levenshtein(key, n), n))
        .filter(|(d, _)| *d <= 2)
        .min_by_key(|(d, _)| *d)
        .map(|(_, n)| n)
}

/// Make every unconnected signal pin an explicit no-connect.
///
/// For each component whose symbol is known, a physical pin no key covers and
/// whose type is not `PowerInput` becomes `PinTarget::NoConnect` — the schematic
/// then carries an NC marker instead of a silently dangling pin. Power inputs are
/// left alone; [`crate::lint`] errors when those are unconnected. Markers are
/// keyed by pin number and inserted in symbol pin order, so the pass is
/// deterministic and idempotent.
pub fn mark_unused_no_connect(d: &mut Design, provider: &SymbolTable) {
    for block in d.blocks.values_mut() {
        for comp in block.components.values_mut() {
            let Some(meta) = provider.symbol(&comp.part) else {
                continue; // unknown symbol — leave the pins as given
            };
            let keys: Vec<String> = comp
                .pins
                .keys()
                .chain(comp.units.values().flatten().map(|(k, _)| k))
                .cloned()
                .collect();
            let covered: std::collections::HashSet<&str> = keys
                .iter()
                .flat_map(|key| resolve(&meta, key))
                .map(|p| p.number.as_str())
                .collect();
            let unused: Vec<String> = meta
                .pins
                .iter()
                .filter(|p| p.etype != crate::PinType::PowerInput)
                .filter(|p| !covered.contains(p.number.as_str()))
                .map(|p| p.number.clone())
                .collect();
            for number in unused {
                comp.pins.insert(number, PinTarget::NoConnect);
            }
        }
    }
}
