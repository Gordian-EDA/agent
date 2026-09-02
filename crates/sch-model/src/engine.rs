//! The schematic placement LEAF contract: one problem in, one placement out.
//!
//! ```text
//! SchematicPlaceProblem ──► PlacementEngine::place(&dyn CandidateEvaluator) ──► PlacementOutput
//! ```
//!
//! An engine is a self-contained algorithm: it reads a [`SchematicPlaceProblem`], asks a
//! [`CandidateEvaluator`] what a candidate placement would COST, and writes its answer
//! back into the problem's items. It never realizes a sheet itself, never touches a KiCAD
//! installation, and never reads the filesystem — the evaluator that does all of that is
//! injected by the composition root (`sch-floorplan`), so an engine author can iterate
//! against a stub evaluator with `cargo test -p <engine>` alone.

use std::collections::BTreeMap;

use geom::Rect;

use crate::ir::LayoutIr;
use crate::item::{Incidence, Item};
use crate::place::{Crossings, Deadline, PlaceOptions, PlaceResult, expired};

/// A pin's electrical flow direction, resolved from the symbol library by the caller so
/// engines never open one themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PinFlow {
    /// An output pin (drives the net).
    Source,
    /// An input pin (listens to the net).
    Sink,
}

/// One block (or whole design) as a neutral placement problem: the parts to place, how
/// they connect, the layout intent to honour, and the search's knobs and ceiling.
///
/// Everything an engine may read is here. `items` is the search's working state — an
/// engine mutates positions/angles in place and the caller reads the final geometry back
/// out of it.
pub struct SchematicPlaceProblem {
    pub items: Vec<Item>,
    pub inc: Incidence,
    /// Layout intent (rails, idioms, relations, the authored grid) the engine may use as
    /// hints or constraints, or ignore.
    pub ir: LayoutIr,
    /// `(item index, pin number)` → flow direction, for the pins whose symbol declares one.
    pub pin_flow: BTreeMap<(usize, String), PinFlow>,
    pub seed: u64,
    pub options: PlaceOptions,
    /// When the search must stop. `None` searches to its full iteration budget.
    /// Engines poll it at their loop heads and ship their best-so-far.
    pub deadline: Option<Deadline>,
}

impl SchematicPlaceProblem {
    /// A problem over `items` with no layout intent and no deadline.
    pub fn new(items: Vec<Item>, inc: Incidence, ir: LayoutIr, seed: u64) -> Self {
        Self {
            items,
            inc,
            ir,
            pin_flow: BTreeMap::new(),
            seed,
            options: PlaceOptions::default(),
            deadline: None,
        }
    }

    /// Stop searching by `deadline`.
    pub fn by(mut self, deadline: Option<Deadline>) -> Self {
        self.deadline = deadline;
        self
    }

    /// Whether the search has run out of time.
    pub fn out_of_time(&self) -> bool {
        expired(self.deadline)
    }
}

/// The raw, weight-FREE measurements of a routed candidate placement — the 18 terms an
/// engine's objective combines under its own weights. Splitting the raw extraction
/// (shared infrastructure, behind [`CandidateEvaluator`]) from the weighting
/// (engine-owned method) is what lets two engines own genuinely different objectives
/// over one realization.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RawMetrics {
    /// Signal-label fallbacks (a wire degraded to a label).
    pub fallbacks: usize,
    /// Junction dots.
    pub junctions: usize,
    /// Total Manhattan wire length (mm).
    pub length: f64,
    /// Visual wire-wire crossings between different nets.
    pub crossings: usize,
    /// Wire corners (L-bends).
    pub corners: usize,
    /// Net merges + placement shorts + foreign taps (hard truthfulness failures).
    pub merges: usize,
    /// Body-overlap pairs + symbol/label-box collisions.
    pub overlaps: usize,
    /// Cramped junction/wire/body proximity.
    pub congestion: usize,
    /// Wires routed through a part body (2-pin transverse/collinear/parallel + IC).
    pub body_cross: usize,
    /// Stray distance: satellites far from the anchor pins they wire to.
    pub stray: f64,
    /// 2-pin parts on the unconventional axis (series/decoupling/leg orientation).
    pub orient_viol: usize,
    /// 1-rail pull/leg orientation+direction violations (the premium-boosted subset).
    pub leg_viol: usize,
    /// Divider/totem spine pairs not drawn in one column.
    pub spine_viol: usize,
    /// Bounding-box half-perimeter of all part bodies (compactness).
    pub spread: f64,
    /// Authored per-block `layout:` relative-order violations.
    pub grid_order: usize,
    /// Same-refdes (multi-unit) bounding-box spread (cohesion).
    pub sib_spread: f64,
    /// Unsatisfied [`crate::ir::Relation`] statements (the LLM's relational intent).
    pub relation: usize,
    /// Bounding-box half-perimeter of every `Relation::Group` (group cohesion).
    pub group_spread: f64,
}

impl RawMetrics {
    /// The measurement of a candidate that could not be realized at all: every term
    /// saturated, so any objective built from it loses to every buildable candidate.
    pub fn unbuildable() -> Self {
        Self {
            fallbacks: usize::MAX,
            junctions: usize::MAX,
            length: f64::INFINITY,
            crossings: usize::MAX,
            corners: usize::MAX,
            merges: usize::MAX,
            overlaps: usize::MAX,
            congestion: usize::MAX,
            body_cross: usize::MAX,
            stray: f64::INFINITY,
            orient_viol: usize::MAX,
            leg_viol: usize::MAX,
            spine_viol: usize::MAX,
            spread: f64::INFINITY,
            grid_order: usize::MAX,
            sib_spread: f64::INFINITY,
            relation: usize::MAX,
            group_spread: f64::INFINITY,
        }
    }
}

/// What a candidate placement would cost if it SHIPPED — the engine's oracle.
///
/// Realizing a sheet (build + route + text-solve) is heavy, non-algorithmic
/// infrastructure; an engine that chooses a measurement-based method calls this and
/// applies its OWN weights to the answer. Implemented by `sch-floorplan` over the real
/// realiser, and by a stub in an engine's own tests.
///
/// Every method is a pure function of `items`: the same slice must always measure the
/// same, so an engine's search is reproducible.
pub trait CandidateEvaluator: Sync {
    /// The 18 weight-free terms of the fast per-move objective build.
    fn measure(&self, items: &[Item]) -> RawMetrics;

    /// Readability warnings the shipped sheet would carry.
    fn warnings(&self, items: &[Item]) -> usize;

    /// The shipped sheet's body / IC / wire crossing triple.
    fn crossings(&self, items: &[Item]) -> Crossings;

    /// Net merges + shorts + foreign taps of the shipped sheet. Any value > 0 mis-wires,
    /// so this is the gate every engine must respect before claiming an improvement.
    fn truthfulness_breaks(&self, items: &[Item]) -> usize;

    /// `(warnings, content extent)` of the sheet AS IT SHIPS — realized with the emit's
    /// orphan label columns, text-solved and reframed. The only faithful measure of a
    /// placement's final sprawl. `None` when the sheet cannot be realized or is empty.
    fn rendered(&self, items: &[Item]) -> Option<(usize, Rect)>;

    /// `(crossings, warnings, content extent)` of the shipped sheet in ONE realize pass,
    /// for the whole-placement gate that needs all three.
    fn shipped(&self, items: &[Item]) -> Option<(Crossings, usize, Rect)>;

    /// The readability warnings themselves, for an engine that reports them.
    fn warning_messages(&self, items: &[Item]) -> Vec<String>;

    /// Where each satellite should slide, read off ONE realization of `items`: the anchor
    /// pins a 2-pin part wires to, and the axis its own body runs along. Only the realiser
    /// knows live pin world positions, and realizing is expensive, so the whole pass is one
    /// query rather than a per-pin oracle.
    fn cohesion_plans(&self, items: &[Item]) -> Vec<CohesionPlan>;

    /// The same evaluator measured against a different sheet intent, for an engine that
    /// trials an IR variant (a forced power rail, say) before adopting it.
    fn with_ir<'a>(&'a self, ir: &'a LayoutIr) -> Box<dyn CandidateEvaluator + 'a>;
}

/// Where one satellite belongs, per [`CandidateEvaluator::cohesion_plans`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CohesionPlan {
    /// Index into the measured `items`.
    pub item: usize,
    /// Whether the part's own pin-to-pin axis runs vertically as placed.
    pub vertical: bool,
    /// The anchor-pin position it should align onto.
    pub target: [f64; 2],
}

/// A schematic placement ENGINE: the searchable leaf.
///
/// ## Contract
/// - **Deterministic given the problem.** No clock beyond the deadline poll; a fixed
///   `seed` reproduces the same placement.
/// - **Deadline-honouring.** Poll [`SchematicPlaceProblem::out_of_time`] at loop heads
///   and ship the best-so-far; honouring a deadline costs convergence, never correctness.
/// - **Never panics.** A unit it cannot place reports through the result's counts.
/// - The returned [`PlacementOutput`] describes the FINAL `problem.items`.
pub trait PlacementEngine: Send + Sync {
    /// Open provenance: the engine's stable name (e.g. `"anneal"`, `"spine"`).
    fn name(&self) -> &'static str;

    /// Search placement and write the final geometry into `problem.items`.
    fn place(
        &self,
        problem: &mut SchematicPlaceProblem,
        eval: &dyn CandidateEvaluator,
    ) -> PlacementOutput;
}

/// Result of an engine's search: the final item geometry lives in `problem.items`;
/// `ir` is the (possibly engine-refined) sheet intent the realiser should ship with.
pub struct PlacementOutput {
    pub result: PlaceResult,
    pub ir: LayoutIr,
}

/// A [`SchematicPlaceProblem`] frozen to JSON: the whole input to a placement leaf, with
/// no KiCAD installation and no symbol library behind it.
///
/// This is what makes a leaf independently workable — an engine author checks a golden
/// problem into their crate's fixtures and iterates on it with `cargo test` alone.
///
/// Two things do not survive the freeze. The deadline is a wall-clock instant the caller
/// sets per run. And each symbol's `raw_definition` — its `(symbol …)` drawing, which only
/// the writer reads and which is 90% of the bytes — is dropped, so a frozen problem is for
/// SEARCHING over, never for emitting a sheet from.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct ProblemFixture {
    pub items: Vec<Item>,
    pub inc: Incidence,
    pub ir: LayoutIr,
    #[serde(default)]
    pub pin_flow: Vec<((usize, String), PinFlow)>,
    pub seed: u64,
    #[serde(default)]
    pub options: PlaceOptions,
}

impl From<&SchematicPlaceProblem> for ProblemFixture {
    fn from(p: &SchematicPlaceProblem) -> Self {
        let mut items = p.items.clone();
        for it in &mut items {
            it.geom.raw_definition = String::new();
        }
        Self {
            items,
            inc: p.inc.clone(),
            ir: p.ir.clone(),
            pin_flow: p.pin_flow.iter().map(|(k, v)| (k.clone(), *v)).collect(),
            seed: p.seed,
            options: p.options,
        }
    }
}

impl From<ProblemFixture> for SchematicPlaceProblem {
    fn from(f: ProblemFixture) -> Self {
        Self {
            items: f.items,
            inc: f.inc,
            ir: f.ir,
            pin_flow: f.pin_flow.into_iter().collect(),
            seed: f.seed,
            options: f.options,
            deadline: None,
        }
    }
}
