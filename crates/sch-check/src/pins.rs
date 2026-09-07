//! Resolving a component pin-map key to the symbol's physical pins.
//!
//! A key is a pin number, name, or alternate function — number first, so a
//! symbol whose pin is named "2" never shadows physical pin 2. Every checker
//! and every front end resolves keys through here, so they all agree.

use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;

use crate::model::*;
use crate::{PinMeta, SymbolMeta, SymbolTable};

/// Every physical pin `key` names. A name may cover several pins (a stacked
/// `VDD`); a number covers exactly one. Empty when the key names nothing.
pub fn resolve<'a>(meta: &'a SymbolMeta, key: &str) -> Vec<&'a PinMeta> {
    let by_number: Vec<&PinMeta> = meta
        .pins
        .iter()
        .filter(|pin| pin.number.eq_ignore_ascii_case(key))
        .collect();
    if !by_number.is_empty() {
        return by_number;
    }
    if is_unnamed(key) {
        return Vec::new(); // `~` is the absence of a name, not a name every unnamed pin shares
    }
    let by_name: Vec<&PinMeta> = meta
        .pins
        .iter()
        .filter(|pin| pin.name.eq_ignore_ascii_case(key))
        .collect();
    if !by_name.is_empty() {
        return by_name;
    }
    let by_alternate: Vec<&PinMeta> = meta
        .pins
        .iter()
        .filter(|pin| {
            pin.alternates
                .iter()
                .any(|alternate| alternate.eq_ignore_ascii_case(key))
        })
        .collect();
    if !by_alternate.is_empty() {
        return by_alternate;
    }
    let Some((pin_name, alternate_name)) = key.split_once('-') else {
        return unique_alternate_suffix(meta, key);
    };
    meta.pins
        .iter()
        .filter(|pin| {
            pin.name.eq_ignore_ascii_case(pin_name)
                && pin
                    .alternates
                    .iter()
                    .any(|alternate| alternate_matches_suffix(alternate, alternate_name))
        })
        .collect()
}

fn unique_alternate_suffix<'a>(meta: &'a SymbolMeta, key: &str) -> Vec<&'a PinMeta> {
    if !key.contains('_') {
        return Vec::new();
    }
    let matches = meta
        .pins
        .iter()
        .filter(|pin| {
            pin.alternates
                .iter()
                .any(|alternate| alternate_matches_suffix(alternate, key))
        })
        .collect::<Vec<_>>();
    let numbers = matches
        .iter()
        .map(|pin| pin.number.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    if numbers.len() == 1 {
        matches
    } else {
        Vec::new()
    }
}

fn alternate_matches_suffix(alternate: &str, key: &str) -> bool {
    alternate.eq_ignore_ascii_case(key)
        || alternate
            .to_ascii_uppercase()
            .ends_with(&format!("_{}", key.to_ascii_uppercase()))
}

/// KiCAD writes an unnamed pin's name as `~`. It is not a name a caller can use.
pub fn is_unnamed(name: &str) -> bool {
    name.is_empty() || name == "~"
}

/// Pin numbers, names, and alternate functions ranked for an unresolvable key.
pub fn ranked_suggestions(meta: &SymbolMeta, key: &str, limit: usize) -> Vec<String> {
    let matcher = SkimMatcherV2::default().ignore_case();
    let mut candidates = meta
        .pins
        .iter()
        .flat_map(|pin| {
            std::iter::once((pin.number.as_str(), None))
                .chain(std::iter::once((pin.name.as_str(), None)))
                .chain(pin.alternates.iter().map(|alternate| {
                    let suffix = alternate
                        .split_once('_')
                        .map(|(_, suffix)| suffix)
                        .filter(|suffix| suffix.contains('_'));
                    (alternate.as_str(), suffix)
                }))
        })
        .filter(|(candidate, _)| !is_unnamed(candidate))
        .filter_map(|(candidate, alias)| {
            let direct = matcher
                .fuzzy_match(candidate, key)
                .into_iter()
                .chain(matcher.fuzzy_match(key, candidate))
                .max();
            let alias = alias.and_then(|alias| {
                matcher
                    .fuzzy_match(alias, key)
                    .into_iter()
                    .chain(matcher.fuzzy_match(key, alias))
                    .max()
            });
            let score = direct.into_iter().chain(alias).max()?;
            Some((score, candidate.to_owned()))
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| left.1.to_ascii_lowercase().cmp(&right.1.to_ascii_lowercase()))
            .then_with(|| left.1.cmp(&right.1))
    });
    candidates.dedup_by(|left, right| left.1.eq_ignore_ascii_case(&right.1));
    candidates.truncate(limit);
    candidates.into_iter().map(|(_, candidate)| candidate).collect()
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
            // No pin map at all: the part is placed whole, its pins open for a later
            // `connect`; marking them would only have that call clear the markers.
            if comp.pins.is_empty() && comp.units.values().all(|u| u.is_empty()) {
                continue;
            }
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
