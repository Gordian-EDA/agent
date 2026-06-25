//! The placement VOCABULARY: the [`PlaceProblem`] an engine reads, the [`PlaceResult`]
//! it returns, the caller's [`PlaceOptions`], and the [`Crossings`] triple. These are
//! pure DATA and live in the neutral kernel (`sch-place`); the engine TRAIT
//! (`PlacementEngine`) lives in `sch-floorplan` beside the measurement library, because
//! every production engine scores routed sheets and so needs that library to place.
//!
//! [`PlaceProblem`] describes ONLY the problem — the connectivity, the intent, a seed,
//! the caller's [`PlaceOptions`] — and is SILENT on METHOD. It carries no cost, no
//! evaluator, no objective, no search knob: a force-directed, analytical, ML, or
//! constraint-solver placer has no cost-candidate loop, so baking one into the problem
//! would be a category error. An engine reads the problem, writes final positions into
//! the `items` slice, and returns a [`PlaceResult`]; *how* (cost-search, learned,
//! template, portfolio) is the engine's own business.

use serde::{Deserialize, Serialize};

use crate::ir::LayoutIr;
use crate::item::Incidence;

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
/// SELF-SUFFICIENT against `sch-place` alone: no `KicadEnv`, no CLI handle, no scorer.
pub struct PlaceProblem<'a> {
    pub inc: &'a Incidence,
    pub ir: &'a LayoutIr,
    pub seed: u64,
    pub options: PlaceOptions,
}

/// What a placement engine reports about the placement it just wrote into
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
