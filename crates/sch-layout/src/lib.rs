//! `sch-layout` — the floorplan layout engine: turns a [`circuit_lang::Design`]
//! into a real `.kicad_sch` file (and back), deterministically.
//!
//! This crate owns the engine. Pipeline: **infer → place → wire → write** (and
//! [`read`] to reverse it):
//!
//! - [`floorplan`] — the cost-scored placement + routing engine and its IR.
//! - [`wire`] — the orthogonal elbow router (was `route`).
//! - [`label`] — text/label placement solver (was `textplace`).
//! - [`write`] — the `SchematicWriter` that renders placements to `.kicad_sch`
//!   (was `emit`).
//! - [`read`] — recovering a `Design` view from an emitted schematic (was `lift`).
//!
//! The shared vocabulary — geometry (`Dir`/segment math), grid snapping, ids, and
//! the [`EmitOutput`] result types — lives in the `sch-model` crate, re-exported
//! here for back-compat.

pub mod floorplan;

// The I/O layer (elbow router + text solver + SchematicWriter + reader) lives in the
// `sch-io` crate; re-exported so the engine's `crate::wire` / `crate::write` /
// `crate::label` / `crate::read` paths resolve unchanged.
pub use sch_io::{label, read, wire, write};

pub use sch_model::{grid, ids};
pub use sch_model::result::{
    AP_BLOCK, AP_INDEX, AP_LAYOUT_REV, AP_PARENT, AP_ROLE, EmitOutput, IdiomReport, ROLE_AUTHORED,
};

/// Test support: read/rewrite a symbol's `(at x y angle)` in emitted text by
/// locating the `(property "Reference" "<refdes>"` block's parent symbol.
///
/// Not `#[cfg(test)]`: integration tests in `tests/` compile against the crate
/// as an external dependency, so these helpers must be part of the public API.
pub mod test_util {
    /// Position of `refdes`'s symbol instance in `sch` text.
    pub fn symbol_at(sch: &str, refdes: &str) -> [f64; 2] {
        let needle = format!("(property \"Reference\" \"{refdes}\"");
        let ref_idx = sch.find(&needle).expect("refdes present");
        let sym_idx = sch[..ref_idx].rfind("(symbol").expect("enclosing symbol");
        let at_idx = sch[sym_idx..].find("(at ").unwrap() + sym_idx + 4;
        let rest = &sch[at_idx..];
        let mut it = rest.split_whitespace();
        let x: f64 = it.next().unwrap().parse().unwrap();
        let y: f64 = it.next().unwrap().trim_end_matches(')').parse().unwrap();
        [x, y]
    }

    /// Rewrite `refdes`'s instance `(at …)` to `new`, preserving the angle.
    pub fn replace_symbol_at(sch: &str, refdes: &str, new: [f64; 2]) -> String {
        let needle = format!("(property \"Reference\" \"{refdes}\"");
        let ref_idx = sch.find(&needle).expect("refdes present");
        let sym_idx = sch[..ref_idx].rfind("(symbol").expect("enclosing symbol");
        let at_idx = sch[sym_idx..].find("(at ").unwrap() + sym_idx;
        let end = sch[at_idx..].find(')').unwrap() + at_idx + 1;
        let angle = sch[at_idx + 4..end - 1]
            .split_whitespace()
            .nth(2)
            .unwrap_or("0")
            .to_string();
        format!(
            "{}(at {} {} {}){}",
            &sch[..at_idx],
            crate::write::fmt_coord(new[0]),
            crate::write::fmt_coord(new[1]),
            angle,
            &sch[end..]
        )
    }
}
