//! The built-in idiom library — pure data.
//!
//! Each idiom is one `const Pattern`. To add an idiom you write a pattern here and
//! list it in [`active_library`] (the set the host freezes & co-places) or
//! [`extended_library`] (defined and tested, awaiting a placement rule). The
//! matcher in `matcher.rs` never changes.

use crate::graph::NetKind;
use crate::pattern::{Edge, NetMatch, NodePred, Pattern, Role};

const CRYSTAL_LIBS: &[&str] = &["Crystal", "Resonator", "Oscillator"];
const CAP_LIBS: &[&str] = &["Device:C"];
const RES_LIBS: &[&str] = &["Device:R"];
const LED_LIBS: &[&str] = &[":LED"];

/// **Crystal oscillator**: an MCU/anchor whose two oscillator pins drive a crystal,
/// each oscillator net loaded to ground by a small capacitor. The defining shape
/// is the crystal *between two distinct signal nets that both tap the same anchor*,
/// with one ground-referenced load cap on each net.
static CRYSTAL_ROLES: &[Role] = &[
    // The anchor is the IC the crystal hangs off — NOT the crystal itself. A 4-pin crystal (the
    // grounded-case Crystal_GND24 variant the agent actually uses) has ≥3 pins, so without this
    // exclusion it ALSO matches the anchor role; the resulting role ambiguity breaks the match and the
    // crystal idiom silently fails to fire (crystal then sprawls far from the OSC pins). Mirrors the
    // decoupling pattern's `Not(Connector)` anchor guard.
    Role::one(
        "anchor",
        NodePred::And(&[
            NodePred::PinsAtLeast(3),
            NodePred::Not(&NodePred::LibAny(CRYSTAL_LIBS)),
        ]),
    ),
    Role::one("crystal", NodePred::LibAny(CRYSTAL_LIBS)),
    Role::one(
        "cap_a",
        NodePred::And(&[NodePred::LibAny(CAP_LIBS), NodePred::Pins(2)]),
    ),
    Role::one(
        "cap_b",
        NodePred::And(&[NodePred::LibAny(CAP_LIBS), NodePred::Pins(2)]),
    ),
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
    min_score: 1.0,
};

/// **Decoupling bank**: three or more rail-to-rail bypass capacitors on a supply
/// that reaches the anchor IC. Each cap bridges a power rail and ground and is not
/// itself an anchor pin's series element.
static DECOUPLE_ROLES: &[Role] = &[
    // The anchor is the IC being decoupled — a multi-pin part that is NOT a connector
    // (a power/SWD header is a multi-pin part on the same rail+ground as the bypass caps,
    // but it is the supply ENTRY, not the decoupling target; without this exclusion the
    // matcher binds the bank to the connector and the placement then drops it).
    Role::one(
        "anchor",
        NodePred::And(&[
            NodePred::PinsAtLeast(3),
            NodePred::Not(&NodePred::LibAny(&["Connector"])),
        ]),
    ),
    Role::many(
        "cap",
        NodePred::And(&[NodePred::LibAny(CAP_LIBS), NodePred::Pins(2)]),
        3,
        64,
    ),
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
    min_score: 1.0,
};

/// **RC low-pass / snubber**: a series resistor into a node that is shunted to
/// ground by a capacitor. Defined here and unit-tested; not yet in the active set
/// (awaiting a series placement rule). Demonstrates extensibility.
static RC_ROLES: &[Role] = &[
    Role::one(
        "res",
        NodePred::And(&[NodePred::LibAny(RES_LIBS), NodePred::Pins(2)]),
    ),
    Role::one(
        "cap",
        NodePred::And(&[NodePred::LibAny(CAP_LIBS), NodePred::Pins(2)]),
    ),
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
    min_score: 1.0,
};

/// **LED indicator**: an LED in series with a current-limiting resistor. They join at
/// the LED's junction node — a SIGNAL net — NOT at a shared power rail; requiring a
/// signal-net join stops the LED from pairing with an unrelated resistor that merely
/// shares the same supply (a reset pull-up on the same V+).
static LED_ROLES: &[Role] = &[
    Role::one(
        "led",
        NodePred::And(&[NodePred::LibAny(LED_LIBS), NodePred::Pins(2)]),
    ),
    Role::one(
        "res",
        NodePred::And(&[NodePred::LibAny(RES_LIBS), NodePred::Pins(2)]),
    ),
];
static LED_EDGES: &[Edge] = &[Edge::shared("led", "res", NetMatch::Kind(NetKind::Signal))];
pub static LED_INDICATOR: Pattern = Pattern {
    name: "led_indicator",
    anchor_role: "led",
    roles: LED_ROLES,
    edges: LED_EDGES,
    min_score: 1.0,
};

const USB_LIBS: &[&str] = &["USB"];

/// **USB-C CC-pulldown pair**: a USB-C receptacle with two resistors, each from a distinct
/// connector signal pin (CC1, CC2) down to ground — the 5.1k configuration resistors. They
/// belong side-by-side beneath the connector; left to the generic search the second drifts to
/// a spare column (the power-entry sheet's R2-exiled defect). USB-scoped (the connector lib_id
/// must contain "USB"), so generic 2-pin header references never match ⇒ snapshots byte-stable.
static CC_PULLDOWN_ROLES: &[Role] = &[
    Role::one(
        "anchor",
        NodePred::And(&[NodePred::LibAny(USB_LIBS), NodePred::PinsAtLeast(6)]),
    ),
    Role::one(
        "res_a",
        NodePred::And(&[NodePred::LibAny(RES_LIBS), NodePred::Pins(2)]),
    ),
    Role::one(
        "res_b",
        NodePred::And(&[NodePred::LibAny(RES_LIBS), NodePred::Pins(2)]),
    ),
];
static CC_PULLDOWN_EDGES: &[Edge] = &[
    // Each resistor taps a (distinct) connector signal net …
    Edge::shared("res_a", "anchor", NetMatch::Kind(NetKind::Signal)),
    Edge::shared("res_b", "anchor", NetMatch::Kind(NetKind::Signal)),
    Edge::distinct("res_a", "res_b", NetMatch::Kind(NetKind::Signal)),
    // … and returns to ground (the pulldown).
    Edge::rail("res_a", NetKind::Ground),
    Edge::rail("res_b", NetKind::Ground),
];
pub static CC_PULLDOWN: Pattern = Pattern {
    name: "cc_pulldown",
    anchor_role: "anchor",
    roles: CC_PULLDOWN_ROLES,
    edges: CC_PULLDOWN_EDGES,
    min_score: 1.0,
};

/// **I2C pull-up pair**: an IC (the I2C device) with two resistors, each from a distinct
/// signal pin of the IC UP to a shared power rail — the classic SDA/SCL pull-ups. They belong
/// side-by-side beside the IC; left to the generic search they drift far below it (the
/// sensor-sheet "pull-ups placed far below the IC, long detours" defect). The mirror of
/// CC_PULLDOWN (resistors to a shared rail, distinct signals, freeze beside the anchor) but the
/// rail is POWER (pull-UP) and the anchor is an IC, NOT a connector.
static I2C_PULLUP_ROLES: &[Role] = &[
    Role::one(
        "anchor",
        NodePred::And(&[
            NodePred::PinsAtLeast(4),
            // Cap pin count so a small peripheral (BME280 = 8 pins) matches but a large MCU
            // (ESP32 = 30+) does NOT — its EN/BOOT control pull-ups are not an I2C bus pair and
            // freezing them as one cluster sprawls the MCU sheet (observed: controller 9→8).
            NodePred::PinsAtMost(16),
            NodePred::Not(&NodePred::LibAny(&["Connector", "Conn_", "USB"])),
        ]),
    ),
    Role::one(
        "res_a",
        NodePred::And(&[NodePred::LibAny(RES_LIBS), NodePred::Pins(2)]),
    ),
    Role::one(
        "res_b",
        NodePred::And(&[NodePred::LibAny(RES_LIBS), NodePred::Pins(2)]),
    ),
];
static I2C_PULLUP_EDGES: &[Edge] = &[
    // Each resistor taps a (distinct) IC signal net …
    Edge::shared("res_a", "anchor", NetMatch::Kind(NetKind::Signal)),
    Edge::shared("res_b", "anchor", NetMatch::Kind(NetKind::Signal)),
    Edge::distinct("res_a", "res_b", NetMatch::Kind(NetKind::Signal)),
    // … both pull UP to the SAME power rail (the I2C bus pull-up shape).
    Edge::shared("res_a", "res_b", NetMatch::Kind(NetKind::Power)),
    Edge::rail("res_a", NetKind::Power),
    Edge::rail("res_b", NetKind::Power),
];
pub static I2C_PULLUP: Pattern = Pattern {
    name: "i2c_pullup",
    anchor_role: "anchor",
    roles: I2C_PULLUP_ROLES,
    edges: I2C_PULLUP_EDGES,
    min_score: 1.0,
};

/// The idioms the host **acts on**. CRYSTAL/DECOUPLING/CC_PULLDOWN freeze and co-place beside
/// their anchor; LED_INDICATOR is report-only — its resistor is snapped below the LED by an mm
/// post-pass. Order matters only for cross-pattern claim tie-breaks (earlier wins).
pub fn active_library() -> Vec<Pattern> {
    vec![
        CRYSTAL.clone(),
        DECOUPLING.clone(),
        LED_INDICATOR.clone(),
        CC_PULLDOWN.clone(),
    ]
}

/// Idioms defined and tested but not yet wired into the engine. Adding one to the
/// active set is a one-line change.
pub fn extended_library() -> Vec<Pattern> {
    vec![RC_LOWPASS.clone()]
}
