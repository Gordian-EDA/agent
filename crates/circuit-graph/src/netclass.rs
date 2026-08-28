//! Net- and part-name classification vocabulary: which names are grounds, power
//! rails, or negative supplies; which parts are connector-like. Pure string
//! heuristics — the single owner every stage (lint, infer, place, route, review)
//! classifies names through, so the vocabulary cannot drift between them.

/// Ground-like net or pin-function name heuristic.
pub fn is_ground(net: &str) -> bool {
    let u = net.trim_start_matches('/').to_ascii_uppercase();
    matches!(u.as_str(), "GNDD" | "GNDA" | "VSS" | "VSSA" | "VSSD")
        || u.starts_with("GND")
        || u.ends_with("GND")
}

/// A voltage-rail token: optional `+`/`-`, then a number with `V` as the decimal/unit
/// marker (`3V3`, `+5V`, `1V8`, `12V`, `3.3V`, `-5V`). Conservative — must start with
/// a digit and contain only digits / `.` / a single `V`, so signal names like
/// `5V_SENSE` or `VIN_FB` are NOT matched.
fn is_voltage_token(u: &str) -> bool {
    let s = u
        .strip_prefix('+')
        .or_else(|| u.strip_prefix('-'))
        .unwrap_or(u);
    if !s.starts_with(|c: char| c.is_ascii_digit()) {
        return false;
    }
    if s.chars().filter(|&c| c == 'V').count() != 1 {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_digit() || c == '.' || c == 'V')
}

/// Alternate voltage-rail spelling used by generated and legacy netlists:
/// `V5`, `V24`, or `V3V3`. Keep this as strict as [`is_voltage_token`] so names
/// such as `V5_SENSE` remain ordinary signals.
fn is_v_prefix_voltage_token(u: &str) -> bool {
    let Some(s) = u.strip_prefix('V') else {
        return false;
    };
    !s.is_empty()
        && s.starts_with(|c: char| c.is_ascii_digit())
        && s.chars().filter(|&c| c == 'V').count() <= 1
        && s.chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c == 'V')
}

/// Whether a net NAME is conventionally a power/ground rail. Used to infer rails on
/// agent-authored boards that name nets `GND`/`3V3`/`VBUS` but place no `power:`
/// symbols. Covers grounds, common named supplies (VCC/VDD/VBAT/VBUS/…), and voltage
/// tokens (3V3, +5V).
pub fn is_power_net(net: &str) -> bool {
    if is_ground(net) {
        return true;
    }
    let u = net.trim_start_matches('/').to_ascii_uppercase();
    if matches!(
        u.as_str(),
        "VCC"
            | "VDD"
            | "VDDA"
            | "VDDD"
            | "VCCA"
            | "VCCD"
            | "AVCC"
            | "AVDD"
            | "DVDD"
            | "PVDD"
            | "VBAT"
            | "VBUS"
            | "VIN"
            | "VOUT"
            | "VEE"
            | "VPP"
            | "VDDIO"
            | "VSYS"
            | "V+"
            | "V-"
            | "VS"
            | "VMOT"
    ) {
        return true;
    }
    if u.starts_with("VCC")
        || u.starts_with("VDD")
        || u.starts_with("VBUS")
        || u.starts_with("VBAT")
    {
        return true;
    }
    is_voltage_token(&u) || is_v_prefix_voltage_token(&u)
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

#[cfg(test)]
mod tests {
    use super::{is_ground, is_power_net};

    #[test]
    fn recognizes_strict_v_prefix_voltage_rails() {
        for net in ["V5", "V24", "V3V3"] {
            assert!(is_power_net(net), "{net}");
        }
        for net in ["V", "V5_SENSE", "VREF", "V24_ENABLE"] {
            assert!(!is_power_net(net), "{net}");
        }
    }

    #[test]
    fn recognizes_named_ground_domains_without_matching_signal_suffixes() {
        for net in [
            "FIELD_GND",
            "LOGIC_GND",
            "CHASSIS_GND",
            "PGND",
            "SGND",
            "VSSA",
            "GNDA",
        ] {
            assert!(is_ground(net), "{net}");
            assert!(is_power_net(net), "{net}");
        }
        for net in ["SIGNAL_GND_SENSE", "GROUND_FAULT", "NOT_GNDED", "VSS_DRIVE"] {
            assert!(!is_ground(net), "{net}");
        }
    }

    #[test]
    fn trims_sheet_path_prefix() {
        assert!(is_ground("/GND"));
        assert!(is_power_net("/3V3"));
    }
}
