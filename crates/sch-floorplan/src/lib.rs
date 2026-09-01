//! `sch-floorplan` — the engine-agnostic schematic placement core that turns a
//! `sch_check::Design` into a real `.kicad_sch` (and back).
//!
//! Owns the [`floorplan`] pipeline: **infer → place → wire → write**. It is
//! engine-agnostic by design — placement engines (`anneal-place` today)
//! depend on this core and drive it through the
//! [`contract`] boundary, never the reverse. The cost a placement engine
//! minimises *is* a routed-sheet score, so the cost/scaffold and the router/writer
//! assembly stay together here.
//!
//! Two surfaces, kept strictly apart:
//! - [`floorplan`] — the pipeline ENTRY POINTS callers run (`infer_ir` / `emit_strategy`
//!   / `emit_writer` / `compose_writers`). The `place` submodule's internals are
//!   `pub(crate)`: a caller cannot reach `floorplan::place::<internal>`.
//! - [`contract`] — the small stable engine API.
//! - [`engine_support`] — lower-level geometry and realization helpers for engine
//!   implementations; public because engines live in separate crates.
//! - [`region`] — place a SUBSET of a sheet among fixed neighbours and obstacles, for
//!   live editing (`arrange(selection)`) and bulk part creation.
//! - [`realize`] — a finished placement → [`sch_doc::SchDoc`] items.
//! - [`live`] — the in-place editing surface: `place_parts` / `arrange` / `rewire` over a
//!   live document, each gated on the extracted net partition.
//!
//! The REALISER — [`wire`] (the elbow router), [`label`] (the text-placement solver) and
//! [`write`] (the `SchematicWriter`) — lives here too: what it draws is what the engines'
//! cost is measured on, so it cannot sit in another crate without the two drifting apart.
//!
//! The shared placement vocabulary lives in `sch-place`; pure geometry and grid
//! snapping live in `geom`; live `.kicad_sch` editing lives in `sch-doc`.

pub mod contract;
pub mod engine_support;
pub mod floorplan;
pub mod label;
pub mod live;
pub mod realize;
pub mod region;
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
