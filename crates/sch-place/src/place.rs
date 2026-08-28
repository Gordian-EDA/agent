//! The placement vocabulary shared by schematic engines: caller knobs
//! ([`PlaceOptions`]), emitted crossing counts ([`Crossings`]), and engine diagnostics
//! ([`PlaceResult`]). The neutral placement+routing problem itself lives in
//! `sch-floorplan`, beside the routing/measurement library it uses.

use serde::{Deserialize, Serialize};

/// Caller-chosen knobs an engine reads from the placement problem. Replaces the
/// ad-hoc `std::env` flags the engines used to read directly (`DEBUG_SA_TIME`,
/// `MULTISHEET_REFINE`, `MOTIF_TILE`) so the engine never touches the environment;
/// the caller sets these fields when it constructs the placement problem.
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
