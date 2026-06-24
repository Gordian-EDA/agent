//! The placement-engine boundary: the [`PlaceProblem`] an engine reads and the
//! [`PlacementEngine`] contract it implements. Lives here (not in `sch-layout`) so
//! the engine crates (`greedy-place`, `anneal-place`) depend only on the shared
//! vocabulary, never on the layout engine itself — breaking the dependency cycle.

use std::collections::BTreeSet;

use kicad_cli_rs::env::KicadEnv;

use crate::ir::LayoutIr;
use crate::item::Incidence;

/// The placement problem an engine works on: the scoring `env`, the connectivity
/// (`inc`), the intent (`ir` — rails/frozen/zones), the ERC `needs_flag` set, and a
/// `seed` for stochastic engines. It bundles what the placement primitives used to
/// thread by hand. A cost-based engine evaluates placements against it; a learned or
/// template engine may only read it.
pub struct PlaceProblem<'a> {
    pub env: &'a KicadEnv,
    pub inc: &'a Incidence,
    pub ir: &'a LayoutIr,
    pub needs_flag: &'a BTreeSet<String>,
    pub seed: u64,
}

/// A schematic placement ENGINE: given the [`PlaceProblem`], write final positions
/// into `items`. The only contract is "produce a placement" — *how* (cost-search,
/// learned, constraint, template, portfolio) is the engine's own business, so the
/// trait assumes nothing (no cost, no move-set). Greedy/Anneal happen to be
/// cost-based and keep their cost private; a future engine need not be.
pub trait PlacementEngine {
    fn name(&self) -> &'static str;
    fn place(&self, problem: &PlaceProblem, items: &mut [crate::item::Item]);
}
