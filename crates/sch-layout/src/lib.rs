//! `sch-layout` — the floorplan layout engine: turns a [`circuit_lang::Design`]
//! into a real `.kicad_sch` file (and back), deterministically.
//!
//! This crate owns the engine:
//!
//! - [`floorplan`] — the cost-scored placement + routing engine and its IR.
//! - [`emit`] — the `SchematicWriter` that renders placements to `.kicad_sch`.
//! - [`lift`] — recovering a `Design` view from an emitted schematic.
//!
//! The shared vocabulary — geometry (`Dir`/segment math), grid snapping, ids, and
//! the [`EmitOutput`] result types — lives in the `sch-model` crate, re-exported
//! here for back-compat.

pub mod emit;
pub mod floorplan;
pub mod lift;
mod route;
mod textplace;

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
            crate::emit::fmt_coord(new[0]),
            crate::emit::fmt_coord(new[1]),
            angle,
            &sch[end..]
        )
    }
}
