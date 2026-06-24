//! Net- and part-name classification vocabulary: which names are grounds, power
//! rails, or negative supplies; which parts are connector-like; which side of a
//! symbol body a pin sits on. Pure string/geometry heuristics shared by the infer
//! and place stages — lifting them here breaks the infer↔place dependency.

/// Ground-like net name heuristic.
pub fn is_ground(net: &str) -> bool {
    let u = net.to_ascii_uppercase();
    u == "GND" || u == "GNDD" || u == "AGND" || u == "DGND" || u == "VSS" || u.starts_with("GND")
}

/// A voltage-rail token: optional `+`/`-`, then a number with `V` as the decimal/unit
/// marker (`3V3`, `+5V`, `1V8`, `12V`, `3.3V`, `-5V`). Conservative — must start with
/// a digit and contain only digits / `.` / a single `V`, so signal names like
/// `5V_SENSE` or `VIN_FB` are NOT matched.
fn is_voltage_token(u: &str) -> bool {
    let s = u.strip_prefix('+').or_else(|| u.strip_prefix('-')).unwrap_or(u);
    if !s.starts_with(|c: char| c.is_ascii_digit()) {
        return false;
    }
    if s.chars().filter(|&c| c == 'V').count() != 1 {
        return false;
    }
    s.chars().all(|c| c.is_ascii_digit() || c == '.' || c == 'V')
}

/// Whether a net NAME is conventionally a power/ground rail. Used to infer rails on
/// agent-authored boards that name nets `GND`/`3V3`/`VBUS` but place no `power:`
/// symbols. Covers grounds, common named supplies (VCC/VDD/VBAT/VBUS/…), and voltage
/// tokens (3V3, +5V).
pub fn is_power_net(net: &str) -> bool {
    if is_ground(net) {
        return true;
    }
    let u = net.to_ascii_uppercase();
    if matches!(
        u.as_str(),
        "VCC" | "VDD" | "VDDA" | "VCCA" | "VCCD" | "AVCC" | "AVDD" | "DVDD"
            | "VBAT" | "VBUS" | "VIN" | "VOUT" | "VEE" | "VPP" | "VDDIO" | "VSYS"
            | "V+" | "V-" | "VS" | "VMOT"
    ) {
        return true;
    }
    if u.starts_with("VCC") || u.starts_with("VDD") || u.starts_with("VBUS") || u.starts_with("VBAT")
    {
        return true;
    }
    is_voltage_token(&u)
}

/// A NEGATIVE supply rail (`VEE`, `V-`, `-12V`, `-5V`). A bulk/decoupling cap with one
/// pin on a negative supply is a VERTICAL rail tap, NOT a horizontal series element.
pub fn is_neg_supply(net: &str) -> bool {
    let u = net.to_ascii_uppercase();
    matches!(u.as_str(), "VEE" | "V-") || (u.starts_with('-') && is_voltage_token(&u))
}

/// A part that sits on power rails but is NOT the IC a decoupling bank serves — a
/// connector, jumper, mounting hole, or test point. These trip the graph matcher's
/// "anything bridging V+/GND" anchor pick, so the bank must skip them.
pub fn is_connector_like(part: &str) -> bool {
    part.contains("Connector")
        || part.contains("Conn_")
        || part.contains("Jumper")
        || part.contains("Mounting")
        || part.contains("TestPoint")
}

/// The side of the symbol body a pin sits on, from its local geometry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PinSide {
    East,
    West,
    North,
    South,
}

/// Classify a pin's local `(x, y)` offset into the body side it sits on.
pub fn pin_side(at: [f64; 2]) -> PinSide {
    if at[0].abs() >= at[1].abs() {
        if at[0] >= 0.0 {
            PinSide::East
        } else {
            PinSide::West
        }
    } else if at[1] >= 0.0 {
        PinSide::North // symbol-local +y is up; the pin points up = top side
    } else {
        PinSide::South
    }
}
