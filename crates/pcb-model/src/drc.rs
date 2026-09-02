//! The design-rule-checking contract: what a rule reports and what an oracle is.
//!
//! The [`Drc`] trait is the injection point every routing leaf uses instead of
//! depending on a concrete rule crate. The rules themselves live in `pcb-drc`;
//! only the vocabulary they report in — [`Finding`] and [`Violation`] — and the
//! copper-cleanup passes derived from a report live here.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::Serialize;

use crate::{Point2, RouteSolution, RoutingView};

/// A connectivity defect in a [`RouteSolution`] relative to its problem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Violation {
    /// A connection's point #`point_index` is not joined to its point #0 by the
    /// emitted copper. (Connections with fewer than 2 points pass trivially.)
    Unconnected {
        /// Connection name.
        connection: String,
        /// Index into the connection's `points_to_connect` that is stranded.
        point_index: usize,
    },
    /// Copper from connections `a` and `b` is electrically shorted. Names are
    /// normalized so `a < b`; one violation is reported per unordered pair.
    CrossNetMerge {
        /// First connection name (the lexicographically smaller).
        a: String,
        /// Second connection name.
        b: String,
    },
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Violation::Unconnected {
                connection,
                point_index,
            } => write!(
                f,
                "connection \"{connection}\": point {point_index} is not connected to point 0 \
                 by the emitted copper",
            ),
            Violation::CrossNetMerge { a, b } => write!(
                f,
                "cross-net short: connections \"{a}\" and \"{b}\" are electrically merged",
            ),
        }
    }
}

/// A single design-rule violation in a [`RouteSolution`] relative to its problem.
///
/// Carries enough payload to debug each case: the connection name(s), the layer
/// where relevant, the measured gap/width against what was required, and a
/// representative location. This is a self-contained serde value — the report a
/// design rule returns.
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
    /// this board (i.e. `layer.index(layer_count)` returns `None`).
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
    /// get the relaxed microvia floor.
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

impl Finding {
    /// The net name(s) a GEOMETRY violation implicates. Empty for a connectivity
    /// finding. For a trace/trace clearance both nets are implicated; dropping
    /// the one in more violations resolves the most.
    pub fn nets(&self) -> Vec<String> {
        match self {
            Finding::ClearanceTraceTrace { a, b, .. } => vec![a.clone(), b.clone()],
            Finding::ClearanceTraceObstacle { connection, .. }
            | Finding::ClearanceViaAny { connection, .. }
            | Finding::TraceWidthBelowMin { connection, .. }
            | Finding::OutOfBounds { connection, .. }
            | Finding::ViaDiameterBelowMin { connection, .. }
            | Finding::InvalidLayer { connection, .. } => vec![connection.clone()],
            Finding::Connectivity { .. } => Vec::new(),
        }
    }

    /// Whether this is a geometry finding (clearance / width / via / bounds /
    /// invalid layer) as opposed to a connectivity defect.
    pub fn is_geometry(&self) -> bool {
        !matches!(self, Finding::Connectivity { .. })
    }
}

/// A design-rule report: every [`Finding`] an oracle raises, in its own
/// deterministic order.
pub type Findings = Vec<Finding>;

/// A design-rule oracle over an emitted [`RouteSolution`].
///
/// Routing leaves take this as `&dyn Drc` so they carry no dependency on a
/// concrete rule set; the composition root injects the production one.
///
/// An implementation must be **deterministic** (identical input ⇒ identical
/// findings in identical order) and must **never panic**.
pub trait Drc {
    /// A stable, human-readable identifier for this rule set.
    fn name(&self) -> &'static str;

    /// Every [`Finding`] this oracle raises for `solution` against `view`, in
    /// deterministic order.
    fn check(&self, view: &RoutingView, solution: &RouteSolution) -> Findings;

    /// Count the GEOMETRY violations of a solution (clearance / width / via /
    /// bounds / invalid layer) — excluding connectivity, which already
    /// correlates with the failed-net count.
    fn geometry_violations(&self, view: &RoutingView, solution: &RouteSolution) -> usize {
        self.check(view, solution)
            .iter()
            .filter(|f| f.is_geometry())
            .count()
    }

    /// Make `solution` connectivity-honest: drop the copper of every net the
    /// oracle reports as unconnected (a half-route a router miscounted as done)
    /// or cross-net-shorted, and return those net names (sorted, unique).
    ///
    /// The connectivity oracle — not a router's own bookkeeping — is the authority
    /// on what is actually joined. After this call the surviving copper carries no
    /// connectivity defect; callers should mark the returned names as failed nets
    /// so the reported result is faithful. Dropping a net's copper only removes
    /// obstacles, so it can never break another net or add a geometry violation.
    fn drop_unconnected_copper(
        &self,
        view: &RoutingView,
        solution: &mut RouteSolution,
    ) -> Vec<String> {
        let mut broken: BTreeSet<String> = BTreeSet::new();
        for finding in self.check(view, solution) {
            if let Finding::Connectivity { violation } = finding {
                match violation {
                    Violation::Unconnected { connection, .. } => {
                        broken.insert(connection);
                    }
                    Violation::CrossNetMerge { a, b } => {
                        broken.insert(a);
                        broken.insert(b);
                    }
                }
            }
        }
        if broken.is_empty() {
            return Vec::new();
        }
        solution.traces.retain(|t| !broken.contains(&t.connection));
        solution.vias.retain(|v| !broken.contains(&v.connection));
        broken.into_iter().collect()
    }

    /// Make `solution` GEOMETRY-clean: while any geometry violation remains, drop
    /// the copper of the net involved in the most violations and retry. Returns
    /// the dropped net names.
    ///
    /// An engine must never EMIT copper that fails DRC — on a board too dense to
    /// route a net cleanly, dropping it (and reporting it failed) is correct; a
    /// silent clearance violation that looks routed is not. Bounded by the net
    /// count so it always terminates. Connectivity is handled separately by
    /// [`Drc::drop_unconnected_copper`]; callers typically run both.
    fn drop_violating_copper(
        &self,
        view: &RoutingView,
        solution: &mut RouteSolution,
    ) -> Vec<String> {
        let mut dropped: BTreeSet<String> = BTreeSet::new();
        // One net can be dropped per pass; at most one pass per net plus a margin.
        let max_passes = view.connections.len() + 1;
        for _ in 0..max_passes {
            let mut tally: BTreeMap<String, usize> = BTreeMap::new();
            for finding in self.check(view, solution) {
                for net in finding.nets() {
                    *tally.entry(net).or_default() += 1;
                }
            }
            let Some(worst) = tally
                .iter()
                .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
                .map(|(n, _)| n.clone())
            else {
                break;
            };
            solution.traces.retain(|t| t.connection != worst);
            solution.vias.retain(|v| v.connection != worst);
            dropped.insert(worst);
        }
        dropped.into_iter().collect()
    }
}
