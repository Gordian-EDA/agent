//! Checks that only *authored text* can fail: the lib_id and the pin-map keys
//! the author typed must name something the symbol table actually has.
//!
//! A design extracted from a live `.kicad_sch` cannot break these rules — its
//! symbols and pins come from the document itself — so they are not semantic
//! lints ([`sch_check::lint`]) but part of accepting the source.

use sch_check::model::*;
use sch_check::pins;
use sch_check::{Diagnostic, Diagnostics, SymbolMeta, SymbolTable};

/// Report unresolvable parts, unresolvable pin keys, and two keys claiming the
/// same physical pin. All errors — the design does not compile.
pub fn lint(d: &Design, provider: &SymbolTable) -> Diagnostics {
    let mut diags = Diagnostics::default();
    for block in d.blocks.values() {
        for (refdes, comp) in &block.components {
            let Some(meta) = provider.symbol(&comp.part) else {
                let mut e = Diagnostic::error(
                    "unknown-part",
                    format!("{refdes}: symbol `{}` not found in any library", comp.part),
                );
                if let Some(s) = provider.suggest(&comp.part).into_iter().next() {
                    e = e.with_suggestion(s);
                }
                diags.push(e);
                continue;
            };
            check_pin_keys(refdes, comp, &meta, &mut diags);
        }
    }
    diags
}

fn check_pin_keys(refdes: &str, comp: &Component, meta: &SymbolMeta, diags: &mut Diagnostics) {
    let mut claimed: std::collections::HashMap<&str, &str> = Default::default();
    for (key, _) in comp.pins.iter().chain(comp.units.values().flatten()) {
        let hits = pins::resolve(meta, key);
        if hits.is_empty() {
            diags.push(unknown_pin(refdes, comp, meta, key));
        }
        for p in hits {
            if let Some(prev) = claimed.insert(&p.number, key)
                && prev != key.as_str()
            {
                diags.push(Diagnostic::error(
                    "pin-conflict",
                    format!(
                        "{refdes}: physical pin {} claimed by both `{prev}` and `{key}`",
                        p.number
                    ),
                ));
            }
        }
    }
}

fn unknown_pin(refdes: &str, comp: &Component, meta: &SymbolMeta, key: &str) -> Diagnostic {
    let mut e = Diagnostic::error(
        "unknown-pin",
        format!("pin `{key}` not found on {refdes} ({})", comp.part),
    );
    if let Some(name) = pins::nearest(meta, key) {
        e = e.with_suggestion(name);
    }
    e
}
