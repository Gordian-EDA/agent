//! Declarative idiom *patterns* — the library's data model.
//!
//! An idiom is a tiny attributed graph: a handful of **roles** (nodes to bind to
//! real components) joined by **edges** (net-sharing constraints). Adding a new
//! idiom is writing one [`Pattern`] value — no matcher code changes. That is the
//! whole point: the matching algorithm in `matcher.rs` is generic over patterns,
//! so the library in `library.rs` stays pure data and is trivially extensible.

use crate::graph::NetKind;

/// A constraint on a single component (node attributes only — connectivity lives
/// in [`Edge`]).
#[derive(Debug, Clone)]
pub enum NodePred {
    /// `lib_id` contains any of these substrings (KiCAD ids, case-sensitive).
    LibAny(&'static [&'static str]),
    /// Exactly `n` pins (2 → a passive two-terminal part).
    Pins(usize),
    /// At least `n` pins (an anchor IC).
    PinsAtLeast(usize),
    /// Parsed value within `[lo, hi]` in base SI units (see [`crate::value`]).
    /// Parts whose value does not parse simply fail this predicate.
    Value { lo: f64, hi: f64 },
    /// All of the sub-predicates hold.
    And(&'static [NodePred]),
    /// The sub-predicate does NOT hold (e.g. an anchor that is NOT a connector).
    Not(&'static NodePred),
    /// Matches anything (placeholder / wildcard role).
    Any,
}

/// The far endpoint of an [`Edge`]: another role, or an anonymous rail of a kind.
#[derive(Debug, Clone)]
pub enum Target {
    /// Share a net with the node bound to this role name.
    Role(&'static str),
    /// Connect to *some* net of this kind (a rail attachment; no role bound).
    Rail(NetKind),
}

/// Which nets satisfy an edge.
#[derive(Debug, Clone, Copy)]
pub enum NetMatch {
    /// Any shared net.
    Any,
    /// A shared net of exactly this kind.
    Kind(NetKind),
}

/// A connectivity constraint incident to role `a`.
#[derive(Debug, Clone)]
pub struct Edge {
    /// Left endpoint: a role name.
    pub a: &'static str,
    /// Right endpoint: a role or a rail.
    pub b: Target,
    /// What kind of net must join them.
    pub net: NetMatch,
    /// Invert the test: the endpoints must *not* share such a net. Lets a pattern
    /// force two same-typed roles apart (the two crystal load caps sit on
    /// *different* oscillator nets, not the same one).
    pub negate: bool,
    /// A soft edge: its absence lowers the similarity score but does not reject
    /// the match. Drives the approximate / graph-similarity behaviour.
    pub optional: bool,
}

impl Edge {
    pub const fn shared(a: &'static str, b: &'static str, net: NetMatch) -> Self {
        Edge { a, b: Target::Role(b), net, negate: false, optional: false }
    }
    pub const fn rail(a: &'static str, kind: NetKind) -> Self {
        Edge { a, b: Target::Rail(kind), net: NetMatch::Kind(kind), negate: false, optional: false }
    }
    pub const fn distinct(a: &'static str, b: &'static str, net: NetMatch) -> Self {
        Edge { a, b: Target::Role(b), net, negate: true, optional: false }
    }
    pub const fn opt(mut self) -> Self {
        self.optional = true;
        self
    }
}

/// How many components a role binds.
#[derive(Debug, Clone, Copy)]
pub enum Mult {
    /// Exactly one node.
    One,
    /// A bank of `min..=max` interchangeable nodes (decoupling caps).
    Many { min: usize, max: usize },
}

/// One node of the pattern.
#[derive(Debug, Clone)]
pub struct Role {
    pub name: &'static str,
    pub pred: NodePred,
    pub mult: Mult,
    /// A soft role: may stay unbound at a similarity cost (a crystal with only one
    /// visible load cap still matches, weaker).
    pub optional: bool,
}

impl Role {
    pub const fn one(name: &'static str, pred: NodePred) -> Self {
        Role { name, pred, mult: Mult::One, optional: false }
    }
    pub const fn many(name: &'static str, pred: NodePred, min: usize, max: usize) -> Self {
        Role { name, pred, mult: Mult::Many { min, max }, optional: false }
    }
}

/// An abstract placement intent the host realizes geometrically. The matcher is
/// pure (no mm / pins), so it only names *what* arrangement the idiom wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlacementHint {
    /// Hug the anchor's pins that carry the cluster's shared signal nets, members
    /// flanking (crystal + load caps).
    BesideAnchorPins,
    /// A compact bank parked beside the anchor (decoupling).
    BankNearAnchor,
    /// Members chained in series outward from the driving pin (LED + resistor).
    SeriesFromPin,
}

/// A complete idiom: roles + edges + how to place the result.
#[derive(Debug, Clone)]
pub struct Pattern {
    /// Stable kind tag, surfaced to the host and the agent (`"crystal"`, …).
    pub name: &'static str,
    /// Which role is the anchor IC (reported as `Match::anchor`).
    pub anchor_role: &'static str,
    pub roles: &'static [Role],
    pub edges: &'static [Edge],
    pub hint: PlacementHint,
    /// Reject matches scoring below this similarity (1.0 = every role+edge bound).
    pub min_score: f64,
}

impl Pattern {
    pub fn role(&self, name: &str) -> Option<&Role> {
        self.roles.iter().find(|r| r.name == name)
    }
}
