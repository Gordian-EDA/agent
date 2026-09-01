//! Resolving a component pin-map key to the symbol's physical pins.
//!
//! A key is a pin NUMBER or a pin NAME — number first, so a symbol whose pin is
//! named "2" never shadows physical pin 2. Every checker and every front end
//! resolves keys through here, so they all agree on what a key means.

use crate::{PinMeta, SymbolMeta};

/// Every physical pin `key` names. A name may cover several pins (a stacked
/// `VDD`); a number covers exactly one. Empty when the key names nothing.
pub fn resolve<'a>(meta: &'a SymbolMeta, key: &str) -> Vec<&'a PinMeta> {
    let by_number: Vec<&PinMeta> = meta.pins.iter().filter(|p| p.number == key).collect();
    if by_number.is_empty() {
        meta.pins.iter().filter(|p| p.name == key).collect()
    } else {
        by_number
    }
}

/// The pin name or number closest to an unresolvable `key`, for a "did you
/// mean" suggestion. `None` when nothing is within two edits.
pub fn nearest<'a>(meta: &'a SymbolMeta, key: &str) -> Option<&'a str> {
    meta.pins
        .iter()
        .flat_map(|p| [p.name.as_str(), p.number.as_str()])
        .map(|n| (strsim::levenshtein(key, n), n))
        .filter(|(d, _)| *d <= 2)
        .min_by_key(|(d, _)| *d)
        .map(|(_, n)| n)
}
