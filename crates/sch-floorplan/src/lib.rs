//! `sch-floorplan` — the COMPOSITION ROOT of schematic layout: it turns a
//! `sch_check::Design` into a real `.kicad_sch` (and back), and it supplies the concrete
//! collaborators `sch_flex::typeset` is handed.
//!
//! Owns the [`floorplan`] pipeline: **infer → place → wire → write**, and implements the
//! `sch-model` contracts the typesetter's output is drawn through:
//!
//! - [`floorplan::place::RoutedEvaluator`] — measures a placed sheet once it is routed:
//!   its truthfulness breaks, its readability warnings, its crossings.
//! - [`wire::ElbowRouter`] — the `SchRouter` that draws the Manhattan wires.
//! - [`label::GreedyText`] — the `TextSolver` that seats fields and net labels.
//!
//! The other surfaces:
//! - [`floorplan`] — the pipeline entry points callers run (`infer_ir`, `place_problem`,
//!   `emit_strategy`).
//! - [`region`] — place a SUBSET of a sheet among fixed neighbours and obstacles, for
//!   live editing (`arrange(selection)`) and bulk part creation.
//! - [`realize`] — a finished placement → [`sch_doc::SchDoc`] items.
//! - [`live`] — the in-place editing surface: `place_parts` / `arrange` / `rewire` over a
//!   live document, each gated on the extracted net partition.
//!
//! The shared layout vocabulary lives in `sch-model`; pure geometry and grid snapping live
//! in `geom`; live `.kicad_sch` editing lives in `sch-doc`.

pub mod bench;
pub mod floorplan;
pub mod label;
pub mod live;
pub mod realize;
pub mod region;
pub mod visual;
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
