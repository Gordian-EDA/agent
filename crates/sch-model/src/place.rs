//! The placement-engine SDK: the [`PlaceProblem`] an engine reads and the
//! [`PlacementEngine`] contract it implements. Both live in the neutral kernel
//! (`sch-model`) so a THIRD-PARTY engine can be written against `sch-model` ALONE —
//! it never touches the incumbent layout crate (`sch-floorplan`) nor any KiCAD CLI.
//!
//! [`PlaceProblem`] describes ONLY the problem — the connectivity, the intent, a seed,
//! the caller's [`PlaceOptions`] — and is SILENT on METHOD. It carries no cost, no
//! evaluator, no objective, no search knob: a force-directed, analytical, ML, or
//! constraint-solver placer has no cost-candidate loop, so baking one into the problem
//! would be a category error. An engine reads the problem, writes final positions into
//! the `items` slice, and returns a [`PlaceResult`]; *how* (cost-search, learned,
//! template, portfolio) is the engine's own business.
//!
//! A measurement-based engine that CHOOSES to score routed sheets obtains its
//! measurement machinery from `sch-floorplan` (the routed-sheet realization library) —
//! that is the engine's choice, reflected in its dependency on `sch-floorplan`, never a
//! field of the neutral problem here. An engine that does not measure depends on
//! `sch-model` alone.

use serde::{Deserialize, Serialize};

use crate::ir::LayoutIr;
use crate::item::{Incidence, Item};

/// Caller-chosen knobs an engine reads from the [`PlaceProblem`]. Replaces the
/// ad-hoc `std::env` flags the engines used to read directly (`DEBUG_SA_TIME`,
/// `MULTISHEET_REFINE`, `MOTIF_TILE`) so the engine never touches the environment;
/// the agent sets these fields, and `sch-floorplan` derives them from the
/// environment at problem construction.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PlaceOptions {
    /// Emit per-phase SA timing to stderr (was `DEBUG_SA_TIME`).
    pub debug_timing: bool,
    /// Force the router-free fast lane even below the pin threshold — the
    /// multi-sheet sub-sheet refine path (was `MULTISHEET_REFINE`).
    pub force_fast: bool,
    /// Tile repeated same-part anchor blocks on a regular lattice (was `MOTIF_TILE`).
    pub motif_tile: bool,
}

/// The three "a wire runs through something" counts of a placement as it would
/// SHIP. They are NOT interchangeable, so they are named rather than a positional
/// triple (mirrors `crate::result::EmitOutput`'s `*_crossings` fields).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Crossings {
    /// Wires routed THROUGH a 2-pin part body (transverse / collinear / parallel).
    pub body: usize,
    /// Wires routed through a 3+-pin IC package body.
    pub ic: usize,
    /// Wire-wire crossings between different nets.
    pub wire: usize,
}

impl Crossings {
    /// Total crossings — the candidate-pick sort key.
    pub fn total(&self) -> usize {
        self.body + self.ic + self.wire
    }
}

/// The placement problem an engine works on: the connectivity (`inc`), the intent
/// (`ir` — rails/frozen/zones/grid/groups, the DECLARATIVE constraints + hints), a
/// `seed` for stochastic engines, and the caller's [`PlaceOptions`]. It describes the
/// PROBLEM and nothing about METHOD — no cost, no evaluator, no objective, no search
/// knob. The parts' geometry every engine needs to place at all lives on each [`Item`]
/// (the slice the engine writes into); the connectivity + intent live here. It is
/// SELF-SUFFICIENT against `sch-model` alone: no `KicadEnv`, no CLI handle, no scorer.
pub struct PlaceProblem<'a> {
    pub inc: &'a Incidence,
    pub ir: &'a LayoutIr,
    pub seed: u64,
    pub options: PlaceOptions,
}

/// What a [`PlacementEngine`] reports about the placement it just wrote into
/// `items`. Purely DIAGNOSTIC: the final geometry lives in the mutated `items`
/// slice (the caller reads positions from there), so this never carries a second,
/// driftable copy of the layout — it mirrors `pcb_place::PlaceResult`'s silhouette
/// without the redundant positions. The counts are measured against the engine's
/// FINAL geometry (every field is one the engine already computes mid-search).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaceResult {
    /// The engine that produced this (e.g. `"anneal"`, `"greedy"`).
    pub engine: String,
    /// Truthfulness breaks of the shipped placement — any value > 0 mis-wires.
    pub truthfulness_breaks: usize,
    /// Readability warnings on the shipped sheet.
    pub warnings: usize,
    /// The body / IC / wire crossing triple of the shipped sheet.
    pub crossings: Crossings,
    /// The engine's final cost (comparable only WITHIN one engine — it is the
    /// selection objective, not an absolute quality scale).
    pub cost: f64,
}

/// A schematic placement ENGINE: given the [`PlaceProblem`], write final positions
/// into `items` AND return the [`PlaceResult`] describing them. The only contract
/// is "produce a placement"; *how* is the engine's own business (see the module doc).
///
/// ## Contract
/// - **Deterministic given the [`PlaceProblem`].** No clock; a fixed `seed` reproduces.
/// - **Never panics.** A unit it cannot place reports through the result's counts,
///   never by unwinding.
/// - The returned [`PlaceResult`] describes the FINAL `items` it wrote — the
///   placement the caller will ship.
pub trait PlacementEngine {
    /// Open provenance: the engine's stable name (e.g. `"greedy"`, `"anneal"`).
    fn name(&self) -> &'static str;

    /// Write the final placement into `items` and return its diagnostics.
    fn place(&self, problem: &PlaceProblem, items: &mut [Item]) -> PlaceResult;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::Incidence;
    use kicad_symbol::geometry::SymbolGeometry;
    use std::collections::BTreeMap;

    /// THE INVARIANT CHECK. A trivial fixed-grid placer that IGNORES connectivity and
    /// scores NOTHING: it lays parts on a lattice. It compiles + runs against `sch-model`
    /// ALONE — no cost, no measurer, no `KicadEnv` — proving [`PlaceProblem`] encodes only
    /// the problem and is silent on method. If `PlaceProblem` ever regrows a cost/evaluator
    /// field, this engine stops compiling against `sch-model` alone and the test breaks.
    struct FixedGrid {
        cols: usize,
        pitch: f64,
    }

    impl PlacementEngine for FixedGrid {
        fn name(&self) -> &'static str {
            "fixed-grid"
        }
        fn place(&self, _problem: &PlaceProblem, items: &mut [Item]) -> PlaceResult {
            // Drop each part onto a lattice, in order. No connectivity, no cost, no routing.
            for (i, it) in items.iter_mut().enumerate() {
                let (c, r) = (i % self.cols, i / self.cols);
                it.at = [c as f64 * self.pitch, r as f64 * self.pitch];
                it.angle = 0.0;
            }
            PlaceResult {
                engine: self.name().to_string(),
                truthfulness_breaks: 0,
                warnings: 0,
                crossings: Crossings::default(),
                cost: 0.0,
            }
        }
    }

    fn item(refdes: &str) -> Item {
        Item {
            refdes: refdes.into(),
            part: "Device:R".into(),
            value: String::new(),
            footprint: None,
            geom: SymbolGeometry { lib_id: "Device:R".into(), pins: Vec::new(), raw_definition: String::new() },
            pins: Vec::new(),
            at: [0.0, 0.0],
            angle: 0.0,
            unit: 1,
            mirror: false,
            frozen: false,
        }
    }

    #[test]
    fn trivial_fixed_grid_engine_runs_against_sch_model_alone() {
        let inc: Incidence = BTreeMap::new();
        let ir = LayoutIr::default();
        let problem = PlaceProblem { inc: &inc, ir: &ir, seed: 0, options: PlaceOptions::default() };
        let mut items = vec![item("R1"), item("R2"), item("R3"), item("R4"), item("R5")];

        let engine = FixedGrid { cols: 2, pitch: 10.0 };
        let report = engine.place(&problem, &mut items);

        assert_eq!(report.engine, "fixed-grid");
        // The lattice: R1=(0,0) R2=(10,0) R3=(0,10) R4=(10,10) R5=(0,20).
        assert_eq!(items[0].at, [0.0, 0.0]);
        assert_eq!(items[1].at, [10.0, 0.0]);
        assert_eq!(items[4].at, [0.0, 20.0]);
    }
}
