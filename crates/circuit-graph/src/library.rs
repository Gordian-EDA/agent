//! The built-in idiom library — pure data.
//!
//! Each idiom is one `const Pattern`. To add an idiom you write a pattern here and
//! list it in [`active_library`] (the set the host freezes & co-places) or
//! [`extended_library`] (defined and tested, awaiting a placement rule). The
//! matcher in `matcher.rs` never changes.

use crate::graph::NetKind;
use crate::pattern::{Edge, NetMatch, NodePred, Pattern, PlacementHint, Role};

const CRYSTAL_LIBS: &[&str] = &["Crystal", "Resonator", "Oscillator"];
const CAP_LIBS: &[&str] = &["Device:C"];
const RES_LIBS: &[&str] = &["Device:R"];
const LED_LIBS: &[&str] = &["LED", "Device:D"];

/// **Crystal oscillator**: an MCU/anchor whose two oscillator pins drive a crystal,
/// each oscillator net loaded to ground by a small capacitor. The defining shape
/// is the crystal *between two distinct signal nets that both tap the same anchor*,
/// with one ground-referenced load cap on each net.
static CRYSTAL_ROLES: &[Role] = &[
    Role::one("anchor", NodePred::PinsAtLeast(3)),
    Role::one("crystal", NodePred::LibAny(CRYSTAL_LIBS)),
    Role::one("cap_a", NodePred::And(&[NodePred::LibAny(CAP_LIBS), NodePred::Pins(2)])),
    Role::one("cap_b", NodePred::And(&[NodePred::LibAny(CAP_LIBS), NodePred::Pins(2)])),
];
static CRYSTAL_EDGES: &[Edge] = &[
    // The crystal shares an oscillator (signal) net with the anchor.
    Edge::shared("crystal", "anchor", NetMatch::Kind(NetKind::Signal)),
    // Each load cap shares a signal net with the crystal …
    Edge::shared("cap_a", "crystal", NetMatch::Kind(NetKind::Signal)),
    Edge::shared("cap_b", "crystal", NetMatch::Kind(NetKind::Signal)),
    // … and the two caps must sit on *different* osc nets, not the same one
    // (they share GND — that's fine — but must not share a signal net).
    Edge::distinct("cap_a", "cap_b", NetMatch::Kind(NetKind::Signal)),
    // Each load cap returns to ground.
    Edge::rail("cap_a", NetKind::Ground),
    Edge::rail("cap_b", NetKind::Ground),
];
pub static CRYSTAL: Pattern = Pattern {
    name: "crystal",
    anchor_role: "anchor",
    roles: CRYSTAL_ROLES,
    edges: CRYSTAL_EDGES,
    hint: PlacementHint::BesideAnchorPins,
    min_score: 1.0,
};

/// **Decoupling bank**: three or more rail-to-rail bypass capacitors on a supply
/// that reaches the anchor IC. Each cap bridges a power rail and ground and is not
/// itself an anchor pin's series element.
static DECOUPLE_ROLES: &[Role] = &[
    Role::one("anchor", NodePred::PinsAtLeast(3)),
    Role::many("cap", NodePred::And(&[NodePred::LibAny(CAP_LIBS), NodePred::Pins(2)]), 3, 64),
];
static DECOUPLE_EDGES: &[Edge] = &[
    // Every bank cap bridges power and ground …
    Edge::rail("cap", NetKind::Power),
    Edge::rail("cap", NetKind::Ground),
    // … on a power net that also reaches the anchor (it decouples *this* IC).
    Edge::shared("cap", "anchor", NetMatch::Kind(NetKind::Power)),
];
pub static DECOUPLING: Pattern = Pattern {
    name: "decoupling",
    anchor_role: "anchor",
    roles: DECOUPLE_ROLES,
    edges: DECOUPLE_EDGES,
    hint: PlacementHint::BankNearAnchor,
    min_score: 1.0,
};

/// **RC low-pass / snubber**: a series resistor into a node that is shunted to
/// ground by a capacitor. Defined here and unit-tested; not yet in the active set
/// (awaiting a series placement rule). Demonstrates extensibility.
static RC_ROLES: &[Role] = &[
    Role::one("res", NodePred::And(&[NodePred::LibAny(RES_LIBS), NodePred::Pins(2)])),
    Role::one("cap", NodePred::And(&[NodePred::LibAny(CAP_LIBS), NodePred::Pins(2)])),
];
static RC_EDGES: &[Edge] = &[
    // Resistor and cap share the filtered node …
    Edge::shared("res", "cap", NetMatch::Kind(NetKind::Signal)),
    // … and the cap shunts it to ground.
    Edge::rail("cap", NetKind::Ground),
];
pub static RC_LOWPASS: Pattern = Pattern {
    name: "rc_lowpass",
    anchor_role: "res",
    roles: RC_ROLES,
    edges: RC_EDGES,
    hint: PlacementHint::SeriesFromPin,
    min_score: 1.0,
};

/// **LED indicator**: an LED in series with a current-limiting resistor. Defined &
/// tested; not yet active.
static LED_ROLES: &[Role] = &[
    Role::one("led", NodePred::LibAny(LED_LIBS)),
    Role::one("res", NodePred::And(&[NodePred::LibAny(RES_LIBS), NodePred::Pins(2)])),
];
static LED_EDGES: &[Edge] = &[Edge::shared("led", "res", NetMatch::Any)];
pub static LED_INDICATOR: Pattern = Pattern {
    name: "led_indicator",
    anchor_role: "led",
    roles: LED_ROLES,
    edges: LED_EDGES,
    hint: PlacementHint::SeriesFromPin,
    min_score: 1.0,
};

/// The idioms the host **acts on** — freezes and co-places. Order matters only for
/// cross-pattern claim tie-breaks (earlier wins).
pub fn active_library() -> Vec<Pattern> {
    vec![CRYSTAL.clone(), DECOUPLING.clone()]
}

/// Idioms defined and tested but not yet wired into placement. Adding one to the
/// active set once it has a placement rule is a one-line change.
pub fn extended_library() -> Vec<Pattern> {
    vec![RC_LOWPASS.clone(), LED_INDICATOR.clone()]
}
