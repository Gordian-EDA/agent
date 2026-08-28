//! `pcb-drc` — PCB design-rule checking.
//!
//! This crate owns both the extensible rule API and Gordian's standard PCB DRC
//! implementation. It checks route geometry and connectivity, and provides
//! cleanup helpers that remove copper which cannot be shipped honestly.
//!
//! ## The engine-SDK shape
//!
//! - [`Rule`] — one design rule. `name()` gives open provenance; `check()`
//!   reads the shared [`DrcCtx`] and returns its [`Finding`]s.
//! - [`DrcSuite`] — an ordered list of `Box<dyn Rule>`. [`DrcSuite::standard`]
//!   is the in-house rule set in its canonical order; [`DrcSuite::with`] appends
//!   a custom rule; [`DrcSuite::run`] builds the context once and concatenates
//!   every rule's findings.
//! - [`DrcCtx`] — the context each rule reads: the problem, the solution, and
//!   the [`collect_copper`] pass shared by the geometry rules.
//! - [`Finding`] — a single design-rule violation (a self-contained serde
//!   value).
//!
//! ## Determinism contract
//!
//! Every [`Rule`] must be: **deterministic** given its input (identical
//! `DrcCtx` ⇒ identical findings, same order); **complete** — it reports every
//! finding it is responsible for, each with a human-readable reason in its
//! payload; and it must **never panic**. [`DrcSuite::run`] preserves rule order,
//! so the full report is deterministic when each rule is.
//!
//! ## Adding a third-party rule
//!
//! Depend on `pcb-drc` alone, `impl Rule for MyRule`, then:
//!
//! ```ignore
//! let findings = DrcSuite::standard()
//!     .with(Box::new(MyRule))
//!     .run(&problem, &solution);
//! ```

pub use pcb_model as problem;

pub mod connectivity;
mod ctx;
pub mod lint;
pub mod rules;

pub use connectivity::Violation;
pub use ctx::{CopperGeom, CopperItem, DrcCtx, collect_copper};
pub use lint::DrcViolation;

use problem::{Point2, RouteProblem, RouteSolution};
use serde::Serialize;

/// A single design-rule violation in a [`RouteSolution`] relative to its problem.
///
/// Carries enough payload to debug each case: the connection name(s), the layer
/// where relevant, the measured gap/width against what was required, and a
/// representative location. This is a self-contained serde value — the report a
/// [`Rule`] returns.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Finding {
    /// Two traces of different connections on the same layer are too close.
    ClearanceTraceTrace {
        /// First connection name.
        a: String,
        /// Second connection name.
        b: String,
        /// Layer the two traces share.
        layer: String,
        /// Measured edge-to-edge gap, mm.
        gap: f64,
        /// Required clearance, mm.
        required: f64,
        /// A point on the offending pair (closest-approach-ish; the first
        /// segment's nearest endpoint), for debugging.
        at: Point2,
    },
    /// A trace is too close to a foreign or unowned (keepout) obstacle.
    ClearanceTraceObstacle {
        /// The trace's connection name.
        connection: String,
        /// The obstacle's owners (empty for unowned/keepout copper).
        obstacle_owners: Vec<String>,
        /// Shared layer the conflict occurs on.
        layer: String,
        /// Measured edge-to-edge gap, mm.
        gap: f64,
        /// Required clearance, mm.
        required: f64,
        /// The obstacle centre, for debugging.
        at: Point2,
    },
    /// A via is too close to copper that is not its own connection.
    ClearanceViaAny {
        /// The via's connection name.
        connection: String,
        /// The other copper's owners (empty for unowned/keepout copper).
        other_owners: Vec<String>,
        /// Measured edge-to-edge gap, mm.
        gap: f64,
        /// Required clearance, mm.
        required: f64,
        /// The via position, for debugging.
        at: Point2,
    },
    /// A trace is narrower than the minimum trace width.
    TraceWidthBelowMin {
        /// The trace's connection name.
        connection: String,
        /// Layer the trace is on.
        layer: String,
        /// The trace's width, mm.
        width: f64,
        /// Required minimum width, mm.
        required: f64,
    },
    /// Copper (trace half-width or via radius included) leaves the board bounds.
    OutOfBounds {
        /// The owning connection name.
        connection: String,
        /// How far past the nearest board edge the copper extends, mm.
        overshoot: f64,
        /// The offending copper location, for debugging.
        at: Point2,
    },
    /// A trace or route point references a layer name that does not exist on
    /// this board (i.e. `layer.index(layer_count)` returns `None`). This is the
    /// slice-1 blind spot: the router silently fell back to layer 0 for unknown
    /// layer names; the lint catches it explicitly.
    InvalidLayer {
        /// The connection name that owns the offending copper.
        connection: String,
        /// The layer reference that could not be resolved (e.g. `"inner1"` on a
        /// 2-layer board, or a typo).
        layer: String,
        /// The board's layer count (provided for context when debugging).
        layer_count: u32,
    },
    /// A via's diameter is below KiCAD's minimum for its type. Through/blind/buried vias
    /// must meet the netclass via diameter (`problem.via_diameter`); only true micro vias
    /// get the relaxed microvia floor. kicad-cli flags this as `via_diameter`; the in-house
    /// lint must too, or the engine would ship a fault (it once shipped 56 — an HDI blind
    /// via emitted below the netclass min before this check existed).
    ViaDiameterBelowMin {
        /// The via's connection name.
        connection: String,
        /// The via's diameter, mm.
        diameter: f64,
        /// Required minimum diameter for this via type, mm.
        required: f64,
        /// The via position, for debugging.
        at: Point2,
    },
    /// A connectivity defect from the connectivity oracle, folded in.
    Connectivity {
        /// The wrapped connectivity violation.
        violation: Violation,
    },
}

/// One design rule: a named check over the shared [`DrcCtx`].
///
/// Implementations must honour the [crate-level determinism contract](crate):
/// deterministic given the context, reporting every finding (each with a human
/// reason in its payload), and never panicking. `name()` gives open provenance
/// so a report can attribute each finding to the rule that raised it.
pub trait Rule {
    /// A stable, human-readable identifier for this rule.
    fn name(&self) -> &'static str;
    /// Every [`Finding`] this rule raises for `ctx`, in deterministic order.
    fn check(&self, ctx: &DrcCtx) -> Vec<Finding>;
}

/// A composable, ordered suite of [`Rule`]s the agent (or a third party) builds.
///
/// [`DrcSuite::run`] builds the [`DrcCtx`] once and concatenates each rule's
/// findings in suite order, so the full report is deterministic whenever every
/// rule is.
pub struct DrcSuite(Vec<Box<dyn Rule>>);

impl DrcSuite {
    /// An empty suite. Compose with [`DrcSuite::with`].
    pub fn new() -> Self {
        DrcSuite(Vec::new())
    }

    /// The canonical in-house rule set, in its fixed reporting order:
    /// invalid-layer, trace-width, out-of-bounds, copper-to-board-edge,
    /// pairwise clearance, hole-to-hole/copper, via-diameter, then connectivity
    /// (folded in last). This is the order the former hardcoded `lint()`
    /// produced, so the findings are byte-identical.
    pub fn standard() -> Self {
        DrcSuite(rules::standard_rules())
    }

    /// Append a custom rule, returning the suite for chaining.
    pub fn with(mut self, rule: Box<dyn Rule>) -> Self {
        self.0.push(rule);
        self
    }

    /// Run every rule against `solution`/`problem` and concatenate the findings
    /// in suite order.
    pub fn run(&self, problem: &RouteProblem, solution: &RouteSolution) -> Vec<Finding> {
        let ctx = DrcCtx::build(problem, solution);
        let mut out = Vec::new();
        for rule in &self.0 {
            out.extend(rule.check(&ctx));
        }
        out
    }

    /// The names of the rules in this suite, in order — for open provenance.
    pub fn rule_names(&self) -> Vec<&'static str> {
        self.0.iter().map(|r| r.name()).collect()
    }
}

impl Default for DrcSuite {
    fn default() -> Self {
        Self::new()
    }
}
