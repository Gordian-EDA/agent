//! Symbol/footprint electrical-pad compatibility checks shared by schematic
//! authoring and PCB regeneration.

use std::collections::BTreeSet;

use anyhow::Result;
use circuit_lang::model::Design;
use kicad_footprint::FootprintId;
use serde::Serialize;

use crate::AgentRuntime;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FootprintPinMismatch {
    pub reference: String,
    pub symbol: String,
    pub footprint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub polarity_mismatch: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub footprint_pads_absent_from_symbol: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub symbol_pins_absent_from_footprint: Vec<String>,
}

struct Assignment<'a> {
    reference: &'a str,
    symbol: &'a str,
    footprint: &'a str,
}

/// Validate explicit footprint assignments in a compiled circuit design.
pub fn design_pin_mismatches(
    ctx: &AgentRuntime,
    design: &Design,
) -> Result<Vec<FootprintPinMismatch>> {
    assignment_mismatches(
        ctx,
        design.blocks.values().flat_map(|block| {
            block
                .components
                .iter()
                .filter_map(|(reference, component)| {
                    component.footprint.as_deref().map(|footprint| Assignment {
                        reference,
                        symbol: &component.part,
                        footprint,
                    })
                })
        }),
    )
}

/// Validate footprint assignments exported by KiCAD before board creation.
pub fn netlist_pin_mismatches(
    ctx: &AgentRuntime,
    netlist: &kicad::Netlist,
) -> Result<Vec<FootprintPinMismatch>> {
    assignment_mismatches(
        ctx,
        netlist.components.iter().filter_map(|component| {
            component
                .properties
                .get("Footprint")
                .map(|footprint| Assignment {
                    reference: &component.reference,
                    symbol: &component.lib_id,
                    footprint,
                })
        }),
    )
}

fn assignment_mismatches<'a>(
    ctx: &AgentRuntime,
    assignments: impl IntoIterator<Item = Assignment<'a>>,
) -> Result<Vec<FootprintPinMismatch>> {
    let assignments: Vec<_> = assignments.into_iter().collect();
    if assignments.is_empty() {
        return Ok(Vec::new());
    }

    let catalog = ctx.footprint_catalog()?;
    let mut mismatches = Vec::new();
    for assignment in assignments {
        let Some(symbol) = ctx.provider().symbol(assignment.symbol) else {
            continue; // circuit compilation reports unknown symbols
        };
        let Ok(footprint_id) = FootprintId::parse(assignment.footprint) else {
            continue; // footprint discovery reports malformed ids more specifically
        };
        let Ok(footprint) = catalog.footprint(&footprint_id) else {
            continue; // footprint discovery/regeneration reports lookup failures
        };
        let (extra_pads, missing_pins) = pad_number_differences(
            symbol.pins.iter().map(|pin| pin.number.as_str()),
            footprint.pads.iter().map(|pad| pad.number.as_str()),
        );
        let polarity_mismatch =
            capacitor_polarity_mismatch(assignment.symbol, &footprint_id).map(str::to_owned);
        if !extra_pads.is_empty() || !missing_pins.is_empty() || polarity_mismatch.is_some() {
            mismatches.push(FootprintPinMismatch {
                reference: assignment.reference.to_owned(),
                symbol: assignment.symbol.to_owned(),
                footprint: footprint_id.to_string(),
                polarity_mismatch,
                footprint_pads_absent_from_symbol: extra_pads,
                symbol_pins_absent_from_footprint: missing_pins,
            });
        }
    }
    Ok(mismatches)
}

fn pad_number_differences<'a>(
    symbol_pin_numbers: impl IntoIterator<Item = &'a str>,
    footprint_pad_numbers: impl IntoIterator<Item = &'a str>,
) -> (Vec<String>, Vec<String>) {
    let symbol_pins: BTreeSet<&str> = symbol_pin_numbers
        .into_iter()
        .filter(|pin| !pin.is_empty())
        .collect();
    let footprint_pads: BTreeSet<&str> = footprint_pad_numbers
        .into_iter()
        .filter(|pad| !pad.is_empty())
        .collect();

    let extra_pads = footprint_pads
        .difference(&symbol_pins)
        .map(|pad| (*pad).to_owned())
        .collect();
    let missing_pins = symbol_pins
        .difference(&footprint_pads)
        .map(|pin| (*pin).to_owned())
        .collect();
    (extra_pads, missing_pins)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CapacitorPolarity {
    Polarized,
    Unpolarized,
}

fn capacitor_polarity_mismatch(
    symbol_id: &str,
    footprint_id: &FootprintId,
) -> Option<&'static str> {
    match (
        symbol_capacitor_polarity(symbol_id),
        footprint_capacitor_polarity(footprint_id),
    ) {
        (Some(CapacitorPolarity::Unpolarized), Some(CapacitorPolarity::Polarized)) => Some(
            "unpolarized capacitor symbol cannot express the footprint's positive pad; use a Device:C_Polarized variant (pin 1 positive) or a non-polarized capacitor footprint",
        ),
        (Some(CapacitorPolarity::Polarized), Some(CapacitorPolarity::Unpolarized)) => Some(
            "polarized capacitor symbol is paired with an ordinary non-polarized capacitor footprint; use a Device:C variant or select a polarized CP/C_Elec footprint",
        ),
        _ => None,
    }
}

fn symbol_capacitor_polarity(symbol_id: &str) -> Option<CapacitorPolarity> {
    let (library, name) = symbol_id.split_once(':')?;
    if library != "Device" {
        return None;
    }
    if name.starts_with("C_Polarized") {
        return Some(CapacitorPolarity::Polarized);
    }
    if matches!(name, "C" | "C_Small" | "C_US" | "C_Small_US" | "C_45deg") {
        return Some(CapacitorPolarity::Unpolarized);
    }
    None
}

fn footprint_capacitor_polarity(footprint_id: &FootprintId) -> Option<CapacitorPolarity> {
    let library = footprint_id.library().as_str();
    let name = footprint_id.name();
    let polarized = match library {
        "Capacitor_SMD" => name.starts_with("CP_") || name.starts_with("C_Elec_"),
        "Capacitor_THT" | "Capacitor_Tantalum_SMD" => name.starts_with("CP_"),
        _ => false,
    };
    if polarized {
        return Some(CapacitorPolarity::Polarized);
    }

    let unpolarized = match library {
        // Standard chip-capacitor names start with a numeric package size.
        // Keeping this narrow avoids guessing about trimmers or vendor parts.
        "Capacitor_SMD" => name
            .strip_prefix("C_")
            .and_then(|suffix| suffix.as_bytes().first())
            .is_some_and(u8::is_ascii_digit),
        "Capacitor_THT" => ["C_Axial_", "C_Disc_", "C_Radial_", "C_Rect_"]
            .iter()
            .any(|prefix| name.starts_with(prefix)),
        _ => false,
    };
    unpolarized.then_some(CapacitorPolarity::Unpolarized)
}

#[cfg(test)]
mod tests {
    use super::{capacitor_polarity_mismatch, pad_number_differences};
    use kicad_footprint::FootprintId;

    fn footprint(id: &str) -> FootprintId {
        FootprintId::parse(id).expect("valid test footprint id")
    }

    #[test]
    fn detects_both_directions_of_numbered_pad_mismatch() {
        let symbol_pins: Vec<_> = (1..=60).map(|n| n.to_string()).collect();
        let footprint_pads: Vec<_> = (1..=4).map(|n| n.to_string()).collect();
        let (extra, missing) = pad_number_differences(
            symbol_pins.iter().map(String::as_str),
            footprint_pads.iter().map(String::as_str),
        );

        assert!(extra.is_empty());
        assert_eq!(missing.len(), 56);
        assert_eq!(missing.first().map(String::as_str), Some("10"));
        assert!(missing.contains(&"60".to_owned()));
    }

    #[test]
    fn allows_mechanical_and_repeated_shield_pads() {
        let (extra, missing) =
            pad_number_differences(["1", "2", "S1"], ["1", "2", "S1", "S1", "", ""]);

        assert!(extra.is_empty());
        assert!(missing.is_empty());
    }

    #[test]
    fn reports_numbered_footprint_pads_missing_from_symbol() {
        let (extra, missing) = pad_number_differences(["1", "2"], ["1", "2", "3", "3", ""]);

        assert_eq!(extra, ["3"]);
        assert!(missing.is_empty());
    }

    #[test]
    fn rejects_unpolarized_symbol_with_polarized_footprints() {
        for footprint_id in [
            "Capacitor_SMD:CP_Elec_8x10.5",
            "Capacitor_SMD:C_Elec_10x10.2",
            "Capacitor_THT:CP_Radial_D8.0mm_P3.50mm",
            "Capacitor_Tantalum_SMD:CP_EIA-3216-18_Kemet-A",
        ] {
            let reason = capacitor_polarity_mismatch("Device:C", &footprint(footprint_id));
            assert!(
                reason.is_some_and(|reason| reason.contains("Device:C_Polarized")),
                "expected actionable mismatch for {footprint_id}, got {reason:?}"
            );
        }
    }

    #[test]
    fn rejects_polarized_symbol_with_ordinary_capacitor_footprints() {
        for footprint_id in [
            "Capacitor_SMD:C_0603_1608Metric",
            "Capacitor_THT:C_Disc_D5.0mm_W2.5mm_P5.00mm",
        ] {
            let reason = capacitor_polarity_mismatch(
                "Device:C_Polarized_Small_US",
                &footprint(footprint_id),
            );
            assert!(
                reason.is_some_and(|reason| reason.contains("non-polarized")),
                "expected actionable mismatch for {footprint_id}, got {reason:?}"
            );
        }
    }

    #[test]
    fn accepts_matching_capacitor_polarity_and_avoids_unknown_guesses() {
        for symbol_id in [
            "Device:C_Polarized",
            "Device:C_Polarized_Small",
            "Device:C_Polarized_US",
            "Device:C_Polarized_Small_US",
        ] {
            assert_eq!(
                capacitor_polarity_mismatch(symbol_id, &footprint("Capacitor_SMD:CP_Elec_8x10.5")),
                None,
                "known polarized alias {symbol_id} must be accepted"
            );
        }
        for symbol_id in ["Device:C", "Device:C_Small", "Device:C_US"] {
            assert_eq!(
                capacitor_polarity_mismatch(
                    symbol_id,
                    &footprint("Capacitor_SMD:C_0603_1608Metric")
                ),
                None,
                "ordinary symbol {symbol_id} must accept a ceramic footprint"
            );
        }
        assert_eq!(
            capacitor_polarity_mismatch(
                "Device:C_Trim",
                &footprint("Capacitor_SMD:CP_Elec_8x10.5")
            ),
            None,
            "special capacitor symbols stay outside the narrow rule"
        );
        assert_eq!(
            capacitor_polarity_mismatch(
                "Device:C_Polarized",
                &footprint("Vendor:Unknown_Capacitor_0603")
            ),
            None,
            "unknown footprint naming must not be guessed"
        );
    }
}
