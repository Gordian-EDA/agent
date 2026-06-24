//! The placement-engine SDK: the [`PlaceProblem`] an engine reads, the
//! [`PlacementCost`] evaluator it scores against, and the [`PlacementEngine`]
//! contract it implements. All of it lives here in the neutral kernel (`sch-model`)
//! so a THIRD-PARTY engine can be written against `sch-model` ALONE — it never
//! touches the incumbent layout crate (`sch-place-core`) nor any KiCAD CLI. An
//! engine: depends on `sch-model`, `impl PlacementEngine for MyEngine`, reads the
//! problem's items + [`PlaceOptions`], scores candidates via the injected
//! `&dyn PlacementCost`, and returns a [`PlaceResult`].
//!
//! The PCB router/placer SDK uses the identical silhouette (`name`/`caps`/a
//! self-contained result/an injected evaluator), so the two tiers stay symmetric.

use serde::{Deserialize, Serialize};

use crate::ir::LayoutIr;
use crate::item::{Incidence, Item};

/// The billing/feature tier an engine belongs to. The engine SELECTOR compares
/// this against the user's entitlement to decide whether an engine may run. New
/// engines default to [`Tier::Free`] (fail-safe: a premium engine opts up
/// explicitly).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Tier {
    /// The always-available baseline (Greedy).
    #[default]
    Free,
    /// Gated behind entitlement (the SA search, Anneal).
    Premium,
}

/// What an engine OFFERS — the capability descriptor a selector queries before
/// dispatch. Defaulted so a minimal engine need not implement [`PlacementEngine::caps`]
/// (it then reads as the Free tier).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineCaps {
    /// The tier this engine is gated to.
    pub tier: Tier,
}

/// Caller-chosen knobs an engine reads from the [`PlaceProblem`]. Replaces the
/// ad-hoc `std::env` flags the engines used to read directly (`DEBUG_SA_TIME`,
/// `MULTISHEET_REFINE`, `MOTIF_TILE`) so the engine never touches the environment;
/// the agent sets these fields, and `sch-place-core` derives them from the
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
/// triple (mirrors `crate::result::EmitOutput`'s `*_crossings` fields). A failed
/// (un-buildable) unit reports all three as [`usize::MAX`].
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

/// The EVALUATOR an engine scores candidate placements against — the boundary that
/// lets an engine avoid importing the incumbent layout crate. `sch-place-core`
/// implements it ONCE (it owns the `KicadEnv` + the routed writer assembly the
/// scoring needs) and the agent injects `&dyn PlacementCost` into the
/// [`PlaceProblem`], exactly as it injects the `Box<dyn PlacementEngine>`.
///
/// An implementation captures the fixed-per-problem scoring state (the env, the
/// connectivity, the intent IR, the ERC needs) so the only argument that varies
/// across a candidate-scoring loop — the candidate geometry — is the lone method
/// parameter.
///
/// ## Contract
/// - **Deterministic given the candidate**: equal `items` ⇒ equal result.
/// - **Never panics.** A unit that cannot be built or scored (e.g. the writer
///   errors) is reported as a *saturated* failure, never `panic!`/`unwrap`:
///   [`Self::cost`] / [`Self::premium_cost`] return [`f64::INFINITY`];
///   [`Self::warnings`] / [`Self::truthfulness_breaks`] return [`usize::MAX`];
///   [`Self::crossings`] returns all-[`usize::MAX`]. A saturated cost makes the
///   candidate un-acceptable to any `min`-based search, so a failed unit can never
///   win the pick — the engine ships its last finite best, and emits no artifact
///   for the failed unit. (The human REASON for the failure is the emit boundary's
///   job to surface; the scorer only makes the candidate lose.)
pub trait PlacementCost {
    /// The base (free-tier) routed cost of `items` — lower is better.
    fn cost(&self, items: &[Item]) -> f64;

    /// The premium routed cost when the caller already has the shipped
    /// [`Self::warnings`] count `warnings` (the candidate-pick's primary sort key).
    /// Identical result to [`Self::premium_cost`] but skips one redundant
    /// text-solve — use this in candidate-scoring loops, which already compute the
    /// warning count. A faithful impl satisfies
    /// `premium_cost(it) == premium_cost_with_warnings(it, warnings(it))`.
    fn premium_cost_with_warnings(&self, items: &[Item], warnings: usize) -> f64;

    /// The premium (paid-tier) routed cost of `items`. Defaulted in terms of
    /// [`Self::premium_cost_with_warnings`] so a fresh evaluator need only write the
    /// hot variant once.
    fn premium_cost(&self, items: &[Item]) -> f64 {
        self.premium_cost_with_warnings(items, self.warnings(items))
    }

    /// Readability warnings (overlapping symbol/label pairs) on the shipped sheet.
    fn warnings(&self, items: &[Item]) -> usize;

    /// The body / IC / wire crossing triple of the shipped sheet.
    fn crossings(&self, items: &[Item]) -> Crossings;

    /// Geometric TRUTHFULNESS breaks (net merges / shorts / foreign taps) — a HARD
    /// count the candidate pick uses to REJECT any placement that mis-wires. The
    /// readability [`Self::warnings`] do NOT detect a merge (a short can LOWER
    /// length+junctions), so this is the gate that keeps a router-free search
    /// truthful.
    fn truthfulness_breaks(&self, items: &[Item]) -> usize;
}

/// The placement problem an engine works on: the connectivity (`inc`), the intent
/// (`ir` — rails/frozen/zones), a `seed` for stochastic engines, the caller's
/// [`PlaceOptions`], and the injected `cost` evaluator. It is SELF-SUFFICIENT — it
/// carries no `KicadEnv` and no CLI handle: an engine scores candidates purely
/// through `cost`, so it depends on `sch-model` alone.
pub struct PlaceProblem<'a> {
    pub inc: &'a Incidence,
    pub ir: &'a LayoutIr,
    pub seed: u64,
    pub options: PlaceOptions,
    pub cost: &'a dyn PlacementCost,
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
/// is "produce a placement" — *how* (cost-search, learned, constraint, template,
/// portfolio) is the engine's own business; the trait assumes nothing beyond the
/// injected [`PlacementCost`].
///
/// ## Contract
/// - **Deterministic given the [`PlaceProblem`].** No clock, no I/O beyond the
///   injected `cost`; a fixed `seed` reproduces.
/// - **Never panics.** A unit it cannot place reports through the result's counts;
///   a failed candidate is rejected via its saturated cost (see [`PlacementCost`]),
///   never by unwinding.
/// - The returned [`PlaceResult`] describes the FINAL `items` it wrote — the
///   placement the caller will ship.
pub trait PlacementEngine {
    /// Open provenance: the engine's stable name (e.g. `"greedy"`, `"anneal"`).
    fn name(&self) -> &'static str;

    /// What this engine offers, for the selector. Defaults to the Free tier;
    /// premium engines override.
    fn caps(&self) -> EngineCaps {
        EngineCaps::default()
    }

    /// Write the final placement into `items` and return its diagnostics.
    fn place(&self, problem: &PlaceProblem, items: &mut [Item]) -> PlaceResult;
}
