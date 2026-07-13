//! Symbol/footprint electrical-pad compatibility checks shared by schematic
//! authoring and PCB regeneration.

use std::collections::BTreeSet;

use anyhow::Result;
use circuit_lang::model::Design;
use kicad_footprint::FootprintId;
use serde::Serialize;

use crate::AgentRuntime;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct FootprintPinMismatch {
    pub(crate) reference: String,
    pub(crate) symbol: String,
    pub(crate) footprint: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) footprint_pads_absent_from_symbol: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) symbol_pins_absent_from_footprint: Vec<String>,
}

struct Assignment<'a> {
    reference: &'a str,
    symbol: &'a str,
    footprint: &'a str,
}

/// Validate explicit footprint assignments in a compiled circuit design.
pub(crate) fn design_pin_mismatches(
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
pub(crate) fn netlist_pin_mismatches(
    ctx: &AgentRuntime,
    netlist: &kicad_cli::Netlist,
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
        if !extra_pads.is_empty() || !missing_pins.is_empty() {
            mismatches.push(FootprintPinMismatch {
                reference: assignment.reference.to_owned(),
                symbol: assignment.symbol.to_owned(),
                footprint: footprint_id.to_string(),
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

#[cfg(test)]
mod tests {
    use super::pad_number_differences;

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
}
