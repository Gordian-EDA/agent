//! The placement vocabulary shared by schematic engines: caller knobs
//! ([`PlaceOptions`]), the search's wall-clock ceiling ([`Deadline`]), which engine
//! to run ([`PlacementEngineKind`]), emitted crossing counts ([`Crossings`]), and
//! engine diagnostics ([`PlaceResult`]). The problem the engines search over, and the
//! traits they and their collaborators implement, live in [`crate::engine`].

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Which placement engine to run. Named here, below every engine crate, so the
/// config, the tool schema and the deadline policy all speak of the same three.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlacementEngineKind {
    /// Annealing followed by cluster-pose and compaction polish.
    #[default]
    Cluster,
    /// Simulated annealing only.
    #[serde(alias = "sa")]
    Anneal,
    /// Deterministic grammar placement only.
    Spine,
}

/// The wall-clock instant a placement search must stop by.
///
/// A search is stochastic and unbounded in principle; each engine keeps its
/// best-so-far, so honouring a deadline costs convergence, never correctness. Checks
/// are cooperative — an engine polls [`Deadline::expired`] at its loop heads and
/// returns what it has.
#[derive(Debug, Clone, Copy)]
pub struct Deadline(Instant);

impl Deadline {
    /// A deadline `budget` from now.
    pub fn after(budget: Duration) -> Self {
        Deadline(Instant::now() + budget)
    }

    pub fn expired(&self) -> bool {
        Instant::now() >= self.0
    }

    /// Time left, zero once passed.
    pub fn remaining(&self) -> Duration {
        self.0.saturating_duration_since(Instant::now())
    }
}

/// Whether an optional deadline has passed. `None` means unbounded.
pub fn expired(deadline: Option<Deadline>) -> bool {
    deadline.is_some_and(|d| d.expired())
}

/// Caller-chosen knobs an engine reads from the placement problem. The caller
/// sets these fields when it constructs the placement problem; engines never
/// inspect process-global environment state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PlaceOptions {
    /// Emit per-phase SA timing to stderr.
    pub debug_timing: bool,
    /// Tile repeated same-part anchor blocks on a regular lattice.
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
