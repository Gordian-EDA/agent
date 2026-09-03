//! What a front end must check about the input it just accepted: the lib_id and
//! the pin-map keys someone *wrote* have to name something the symbol table has.
//!
//! A design extracted from a live `.kicad_sch` cannot break these rules — its
//! symbols and pins come from the document itself — which is why they are not
//! semantic lints ([`crate::lint`]). Both front ends that accept written input
//! (the YAML language and [`crate::place_parts`]) run them.

use crate::model::*;
use crate::{Diagnostic, Diagnostics, SymbolMeta, SymbolTable, pins};

/// Report unresolvable parts, unresolvable pin keys, and two keys claiming the
/// same physical pin. All errors — the input does not describe a real circuit.
pub fn lint(d: &Design, provider: &SymbolTable) -> Diagnostics {
    let mut diags = Diagnostics::default();
    for block in d.blocks.values() {
        for (refdes, comp) in &block.components {
            let Some(meta) = provider.symbol(&comp.part) else {
                diags.push(unknown_part(refdes, &comp.part, provider));
                continue;
            };
            let keys = comp
                .pins
                .keys()
                .chain(comp.units.values().flatten().map(|(k, _)| k));
            check_pin_keys(refdes, &comp.part, &meta, keys, &mut diags);
        }
    }
    diags
}

/// `refdes` names a symbol no library has.
///
/// The message carries the closest real lib_ids — a caller that guessed the library
/// (`Fuse:Fuse`) needs the whole shortlist, not just the single best, to pick the
/// part it meant. The first is also attached as the machine-readable suggestion.
pub fn unknown_part(refdes: &str, part: &str, provider: &SymbolTable) -> Diagnostic {
    let (message, near) = unknown_part_details(part, provider);
    let mut diagnostic = Diagnostic::error("unknown-part", format!("{refdes}: {message}"));
    if let Some(suggestion) = near.into_iter().next() {
        diagnostic = diagnostic.with_suggestion(suggestion);
    }
    diagnostic
}

/// Explain an unknown symbol without assuming which tool asked for it.
pub fn unknown_part_details(part: &str, provider: &SymbolTable) -> (String, Vec<String>) {
    if looks_like_footprint(part) {
        let near = footprint_symbol_suggestions(part);
        let did_you_mean = near
            .first()
            .map(|symbol| format!(" Did you mean `{symbol}`?"))
            .unwrap_or_default();
        return (
            format!(
                "`{part}` is a FOOTPRINT name, not a symbol. `part` takes a symbol \
                 lib_id like `Device:R` or `Connector_Generic:Conn_01x11`; put the footprint in \
                 this part's `footprint` field instead. Use search_symbols to find the symbol.{did_you_mean}"
            ),
            near,
        );
    }
    let near = provider.suggest(part);
    let did_you_mean = match near.first() {
        Some(_) => format!("; did you mean {}?", near.join(", ")),
        None => "; search_symbols will find the right lib_id".to_string(),
    };
    (format!("{part} not found in any library{did_you_mean}"), near)
}

/// Whether `part` reads as a KiCAD footprint identifier rather than a symbol one.
///
/// The two namespaces look alike — both are `Library:Name` — so a model that has
/// the footprint to hand readily writes it where the symbol belongs. Footprint
/// names carry package geometry that symbol names never do: a pitch, a pad count
/// in `NxM` form, or a mounting word.
pub fn looks_like_footprint(part: &str) -> bool {
    let name = part.rsplit(':').next().unwrap_or(part);
    let lower = name.to_ascii_lowercase();
    lower.contains("mm")
        && (lower.contains("_p")
            || lower.contains("pitch")
            || lower.ends_with("vertical")
            || lower.ends_with("horizontal")
            || lower.contains("handsolder"))
}

/// Infer the generic connector symbol represented by a pin-header footprint.
pub fn footprint_symbol_suggestions(footprint: &str) -> Vec<String> {
    let name = footprint.rsplit(':').next().unwrap_or(footprint);
    let dimensions = name.split('_').find_map(|word| {
        let (columns, rows) = word.split_once('x')?;
        let columns = columns.parse::<u32>().ok()?;
        let rows = rows.parse::<u32>().ok()?;
        (matches!(columns, 1 | 2) && rows > 0).then_some((columns, rows))
    });
    match dimensions {
        Some((1, rows)) => vec![format!("Connector_Generic:Conn_01x{rows:02}")],
        Some((2, rows)) => vec![format!("Connector_Generic:Conn_02x{rows:02}_Odd_Even")],
        _ => Vec::new(),
    }
}

/// `key` is not a pin of `part`, with ranked physical and alternate names.
pub fn unknown_pin(refdes: &str, part: &str, meta: &SymbolMeta, key: &str) -> Diagnostic {
    let suggestions = pins::ranked_suggestions(meta, key, 8);
    let did_you_mean = if suggestions.is_empty() {
        "none".to_string()
    } else {
        suggestions.join(", ")
    };
    let mut e = Diagnostic::error(
        "unknown-pin",
        format!(
            "pin `{key}` not found on {refdes} ({part}); did_you_mean: [{did_you_mean}]"
        ),
    );
    if let Some(name) = suggestions.first() {
        e = e.with_suggestion(name);
    }
    e
}

/// Two keys resolve to one physical pin — whichever they connect to, only one
/// of them can be what the author meant.
pub fn pin_conflict(refdes: &str, number: &str, first: &str, second: &str) -> Diagnostic {
    Diagnostic::error(
        "pin-conflict",
        format!("{refdes}: physical pin {number} claimed by both `{first}` and `{second}`"),
    )
}

fn check_pin_keys<'a>(
    refdes: &str,
    part: &str,
    meta: &SymbolMeta,
    keys: impl Iterator<Item = &'a String>,
    diags: &mut Diagnostics,
) {
    let mut claimed: std::collections::HashMap<&str, &str> = Default::default();
    for key in keys {
        let hits = pins::resolve(meta, key);
        if hits.is_empty() {
            diags.push(unknown_pin(refdes, part, meta, key));
        }
        for p in hits {
            if let Some(prev) = claimed.insert(&p.number, key)
                && prev != key.as_str()
            {
                diags.push(pin_conflict(refdes, &p.number, prev, key));
            }
        }
    }
}
