//! `pcb-drc` — PCB design-rule checking.
//!
//! This crate owns both the extensible rule API and Gordian's standard PCB DRC
//! implementation. It checks route geometry and connectivity, and provides
//! cleanup helpers that remove copper which cannot be shipped honestly.
//!
//! [`StandardDrc`] is the in-house rule set behind the [`Drc`] contract, so a
//! routing leaf can take it as `&dyn Drc` without depending on this crate.
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
//! - [`Finding`](pcb_model::Finding) — a single design-rule violation (a
//!   self-contained serde value, defined in `pcb-model` so the contract is
//!   expressible without this crate).
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

pub mod connectivity;
mod ctx;
#[cfg(test)]
mod goldens;
pub mod rules;

pub use ctx::{CopperGeom, CopperItem, DrcCtx, collect_copper};

use pcb_model::{Drc, Finding, Findings, RouteSolution, RoutingView};

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
    /// pairwise clearance, hole-to-hole/copper, via-diameter, dangling ends,
    /// then connectivity
    /// (folded in last). This order is part of the contract: a report is
    /// comparable across runs only because it is fixed.
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
    pub fn run(&self, problem: &RoutingView, solution: &RouteSolution) -> Vec<Finding> {
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

/// Gordian's standard PCB design-rule oracle: [`DrcSuite::standard`] behind the
/// [`Drc`] contract, so routing leaves can take it as `&dyn Drc` without naming
/// this crate.
///
/// Findings come back in the suite's canonical order — geometry rules first (in
/// copper-collection order), then the connectivity oracle folded in last.
#[derive(Debug, Clone, Copy, Default)]
pub struct StandardDrc;

impl Drc for StandardDrc {
    fn name(&self) -> &'static str {
        "standard"
    }

    fn check(&self, view: &RoutingView, solution: &RouteSolution) -> Findings {
        DrcSuite::standard().run(view, solution)
    }
}
