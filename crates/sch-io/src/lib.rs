//! `sch-io` — the schematic I/O layer:
//!
//! - [`wire`] — the elbow (Manhattan) router.
//! - [`label`] — the text-placement collision solver.
//! - [`write`] — the `SchematicWriter`: placed symbols + routed wires → `.kicad_sch`.
//! - [`read`] — recover a `Design` view from an emitted `.kicad_sch`.
//!
//! `wire` and `write` are mutually coupled (the writer hands the router a `RouteScene`;
//! the router fills the writer), so they share one crate rather than a forced split.
//! Geometry/grid/ids vocabulary comes from `sch-place` (re-exported for the modules'
//! `crate::grid` / `crate::ids` paths).

pub use sch_place::{grid, ids};

pub mod label;
pub mod read;
pub mod wire;
pub mod write;

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
