//! Symbol/footprint electrical-pad compatibility checks shared by schematic
//! authoring and PCB regeneration.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::{Context, Result, anyhow};
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use kicad_footprint::{FootprintId, PadTechnology, SearchQuery};
use sch_check::model::Design;
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
    pub missing_pads: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub extra_pins: Vec<String>,
    pub suggestion: Option<String>,
    #[serde(skip)]
    pub suggestion_compatible: bool,
    pub symbol_suggestion: Option<String>,
}

impl FootprintPinMismatch {
    /// Model-facing payload audit entry for this mismatch.
    pub fn payload(&self) -> sch_check::place_parts::FootprintMismatch {
        sch_check::place_parts::FootprintMismatch {
            refdes: self.reference.clone(),
            symbol: self.symbol.clone(),
            footprint: self.footprint.clone(),
            missing_pads: self.missing_pads.clone(),
            extra_pins: self.extra_pins.clone(),
            message: mismatch_message(&self.missing_pads, &self.extra_pins),
            suggestion: self.suggestion.clone(),
        }
    }
}

fn mismatch_message(missing_pads: &[String], extra_pins: &[String]) -> String {
    let mut clauses = Vec::new();
    if !missing_pads.is_empty() {
        clauses.push(format!(
            "symbol pin(s) {} have no footprint pad; use no_connect only when those pins are intentionally unused",
            missing_pads.join(", ")
        ));
    }
    if !extra_pins.is_empty() {
        clauses.push(format!(
            "footprint pad(s) {} have no symbol pin",
            extra_pins.join(", ")
        ));
    }
    clauses.join("; ")
}

/// Electrical compatibility of one installed symbol/footprint pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FootprintCompatibility {
    pub compatible: bool,
    pub pads: Vec<String>,
    pub missing_pads: Vec<String>,
    pub extra_pins: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub polarity_mismatch: Option<String>,
}

/// One compatibility-aware footprint search result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CompatibleFootprintHit {
    pub lib_id: String,
    pub compatible: bool,
    pub pads: Vec<String>,
    #[serde(skip)]
    score: i64,
}

struct Assignment<'a> {
    reference: &'a str,
    symbol: &'a str,
    footprint: &'a str,
    ignored_pins: BTreeSet<String>,
}

/// Decide one symbol/footprint pair using the shared electrical policy.
pub fn footprint_compatibility(
    ctx: &AgentRuntime,
    symbol_id: &str,
    footprint_id: &str,
) -> Result<FootprintCompatibility> {
    let symbol = ctx
        .provider()
        .symbol(symbol_id)
        .or_else(|| ctx.index().ok()?.symbol(symbol_id))
        .ok_or_else(|| anyhow!("unknown symbol `{symbol_id}`"))?;
    footprint_compatibility_for_pins(
        ctx,
        symbol_id,
        symbol.pins.iter().map(|pin| pin.number.as_str()),
        footprint_id,
    )
}

fn footprint_compatibility_ignoring(
    ctx: &AgentRuntime,
    symbol_id: &str,
    footprint_id: &str,
    ignored_pins: &BTreeSet<String>,
) -> Result<FootprintCompatibility> {
    let symbol = ctx
        .provider()
        .symbol(symbol_id)
        .or_else(|| ctx.index().ok()?.symbol(symbol_id))
        .ok_or_else(|| anyhow!("unknown symbol `{symbol_id}`"))?;
    let all_pins = symbol
        .pins
        .iter()
        .map(|pin| pin.number.as_str())
        .filter(|number| !number.is_empty())
        .collect::<BTreeSet<_>>();
    footprint_compatibility_for_required_pins(
        ctx,
        symbol_id,
        all_pins
            .iter()
            .copied()
            .filter(|number| !ignored_pins.contains(*number)),
        all_pins.iter().copied(),
        footprint_id,
    )
}

/// Decide a pair when the live schematic, rather than the provider, owns its pins.
pub fn footprint_compatibility_for_pins<'a>(
    ctx: &AgentRuntime,
    symbol_id: &str,
    symbol_pin_numbers: impl IntoIterator<Item = &'a str>,
    footprint_id: &str,
) -> Result<FootprintCompatibility> {
    let symbol_pins = symbol_pin_numbers
        .into_iter()
        .filter(|number| !number.is_empty())
        .collect::<BTreeSet<_>>();
    footprint_compatibility_for_required_pins(
        ctx,
        symbol_id,
        symbol_pins.iter().copied(),
        symbol_pins.iter().copied(),
        footprint_id,
    )
}

fn footprint_compatibility_for_required_pins<'a>(
    ctx: &AgentRuntime,
    symbol_id: &str,
    required_pin_numbers: impl IntoIterator<Item = &'a str>,
    all_symbol_pin_numbers: impl IntoIterator<Item = &'a str>,
    footprint_id: &str,
) -> Result<FootprintCompatibility> {
    let id = FootprintId::parse(footprint_id)
        .map_err(|_| anyhow!("malformed footprint id `{footprint_id}`"))?;
    let footprint = ctx
        .footprint_catalog()?
        .footprint(&id)
        .with_context(|| format!("loading footprint `{id}`"))?;

    let required_pins = required_pin_numbers
        .into_iter()
        .filter(|number| !number.is_empty())
        .collect::<BTreeSet<_>>();
    let symbol_pins = all_symbol_pin_numbers
        .into_iter()
        .filter(|number| !number.is_empty())
        .collect::<BTreeSet<_>>();
    let footprint_pads = footprint
        .pads
        .iter()
        .filter(|pad| pad.technology != PadTechnology::NpThruHole)
        .map(|pad| pad.number.as_str())
        .filter(|number| !number.is_empty())
        .collect::<BTreeSet<_>>();
    let missing_pads = required_pins
        .difference(&footprint_pads)
        .map(|number| (*number).to_owned())
        .collect::<Vec<_>>();
    let extra_pins = footprint
        .pads
        .iter()
        .filter(|pad| {
            pad.technology != PadTechnology::NpThruHole
                && !pad.number.is_empty()
                && !symbol_pins.contains(pad.number.as_str())
                && !mechanical_or_shield_pad(&pad.number)
        })
        .map(|pad| pad.number.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let pads = footprint_pads.into_iter().map(str::to_owned).collect();
    let polarity_mismatch = capacitor_polarity_mismatch(symbol_id, &id).map(str::to_owned);
    Ok(FootprintCompatibility {
        compatible: missing_pads.is_empty() && extra_pins.is_empty() && polarity_mismatch.is_none(),
        pads,
        missing_pads,
        extra_pins,
        polarity_mismatch,
    })
}

/// Rank catalog footprints by compatibility first and fuzzy query score second.
pub fn search_compatible_footprints(
    ctx: &AgentRuntime,
    symbol_id: &str,
    query: Option<&str>,
    limit: usize,
) -> Result<Vec<CompatibleFootprintHit>> {
    const TEXT_POOL: usize = 64;
    const FAMILY_POOL: usize = 2;

    if ctx
        .provider()
        .symbol(symbol_id)
        .or_else(|| ctx.index().ok()?.symbol(symbol_id))
        .is_none()
    {
        return Err(anyhow!("unknown symbol `{symbol_id}`"));
    }
    let catalog = ctx.footprint_catalog()?;
    let search_text = query
        .filter(|text| !text.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| footprint_query_for_symbol(symbol_id));
    let text_hits = catalog.search(SearchQuery::new(&search_text).limit(TEXT_POOL));
    let matcher = SkimMatcherV2::default().ignore_case();
    let mut libraries = Vec::new();
    let explicit_family = FootprintId::parse(&search_text).ok().filter(|preferred| {
        catalog
            .libraries()
            .any(|library| library.id() == preferred.library())
    });
    if let Some(preferred) = &explicit_family {
        libraries.push(preferred.library().clone());
    }
    if explicit_family.is_none() {
        for hit in text_hits.iter().take(FAMILY_POOL) {
            if !libraries.contains(hit.id.library()) {
                libraries.push(hit.id.library().clone());
            }
        }
    }
    let mut scored = HashMap::<FootprintId, i64>::new();
    for hit in &text_hits {
        if libraries.contains(hit.id.library()) {
            scored.insert(hit.id.clone(), hit.score);
        }
    }
    for library in &libraries {
        for entry in catalog.entries_in(library) {
            let id = entry.id().clone();
            let score = matcher
                .fuzzy_match(&id.to_string(), &search_text)
                .unwrap_or(0);
            scored.entry(id).or_insert(score);
        }
    }

    let mut hits = Vec::with_capacity(scored.len());
    for (id, score) in scored {
        let Ok(verdict) = footprint_compatibility(ctx, symbol_id, &id.to_string()) else {
            continue;
        };
        hits.push(CompatibleFootprintHit {
            lib_id: id.to_string(),
            compatible: verdict.compatible,
            pads: verdict.pads,
            score,
        });
    }
    hits.sort_by(|left, right| {
        right
            .compatible
            .cmp(&left.compatible)
            .then_with(|| right.score.cmp(&left.score))
            .then_with(|| left.lib_id.cmp(&right.lib_id))
    });
    hits.truncate(limit);
    Ok(hits)
}

/// Best installed compatible footprint for a symbol near `preferred`.
pub fn best_compatible_footprint(
    ctx: &AgentRuntime,
    symbol_id: &str,
    preferred: Option<&str>,
) -> Result<Option<String>> {
    let nearby = search_compatible_footprints(ctx, symbol_id, preferred, 1)?
        .into_iter()
        .find(|hit| hit.compatible)
        .map(|hit| hit.lib_id);
    if nearby.is_some() {
        return Ok(nearby);
    }
    Ok(None)
}

/// Best nearby symbol variant whose pins agree with an installed footprint.
pub fn best_compatible_symbol(
    ctx: &AgentRuntime,
    symbol_id: &str,
    footprint_id: &str,
) -> Result<Option<String>> {
    let mut candidates = ctx
        .index()?
        .search(symbol_id, 25)
        .into_iter()
        .map(|hit| hit.lib_id)
        .collect::<Vec<_>>();
    candidates.extend(ctx.provider().suggest(symbol_id));
    candidates
        .retain(|candidate| candidate != symbol_id && same_symbol_family(symbol_id, candidate));
    candidates.dedup();
    for candidate in candidates {
        if footprint_compatibility(ctx, &candidate, footprint_id).is_ok_and(|v| v.compatible) {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

/// Whether a suggested replacement is a named variant of the same symbol family.
fn same_symbol_family(current: &str, candidate: &str) -> bool {
    let Some((current_library, current_name)) = current.split_once(':') else {
        return false;
    };
    let Some((candidate_library, candidate_name)) = candidate.split_once(':') else {
        return false;
    };
    if current_library != candidate_library {
        return false;
    }
    current_name
        .strip_prefix(candidate_name)
        .or_else(|| candidate_name.strip_prefix(current_name))
        .is_some_and(|suffix| suffix.starts_with('_'))
}

/// Audit one resolvable assignment and attach its best catalog repair.
pub fn assignment_pin_mismatch(
    ctx: &AgentRuntime,
    reference: &str,
    symbol_id: &str,
    footprint_id: &str,
) -> Result<Option<FootprintPinMismatch>> {
    assignment_pin_mismatch_ignoring(ctx, reference, symbol_id, footprint_id, &BTreeSet::new())
}

/// Audit one assignment while allowing explicitly no-connected symbol pins to
/// be absent from the physical package.
pub fn assignment_pin_mismatch_ignoring(
    ctx: &AgentRuntime,
    reference: &str,
    symbol_id: &str,
    footprint_id: &str,
    ignored_pins: &BTreeSet<String>,
) -> Result<Option<FootprintPinMismatch>> {
    let verdict = footprint_compatibility_ignoring(ctx, symbol_id, footprint_id, ignored_pins)?;
    if verdict.compatible {
        return Ok(None);
    }
    let suggestion = best_same_library_footprint(
        ctx,
        symbol_id,
        footprint_id,
        ignored_pins,
        Some(footprint_id),
    )?;
    let suggestion_compatible = suggestion.as_deref().is_some_and(|candidate| {
        footprint_compatibility_ignoring(ctx, symbol_id, candidate, ignored_pins)
            .is_ok_and(|candidate| candidate.compatible)
    });
    let symbol_suggestion = if suggestion.is_none() {
        best_compatible_symbol(ctx, symbol_id, footprint_id)?
    } else {
        None
    };
    Ok(Some(FootprintPinMismatch {
        reference: reference.to_owned(),
        symbol: symbol_id.to_owned(),
        footprint: footprint_id.to_owned(),
        polarity_mismatch: verdict.polarity_mismatch,
        missing_pads: verdict.missing_pads,
        extra_pins: verdict.extra_pins,
        suggestion,
        suggestion_compatible,
        symbol_suggestion,
    }))
}

fn best_same_library_footprint(
    ctx: &AgentRuntime,
    symbol_id: &str,
    preferred: &str,
    ignored_pins: &BTreeSet<String>,
    exclude: Option<&str>,
) -> Result<Option<String>> {
    let Ok(preferred_id) = FootprintId::parse(preferred) else {
        return Ok(None);
    };
    let matcher = SkimMatcherV2::default().ignore_case();
    let preferred_tokens = preferred_id.name().split('_').collect::<Vec<_>>();
    let mut text_candidates = ctx
        .footprint_catalog()?
        .entries_in(preferred_id.library())
        .filter_map(|entry| {
            let id = entry.id().to_string();
            if exclude == Some(id.as_str()) {
                return None;
            }
            (1..=preferred_tokens.len()).rev().find_map(|token_count| {
                let query = preferred_tokens[..token_count].join("_");
                matcher
                    .fuzzy_match(entry.id().name(), &query)
                    .map(|score| (token_count, score, id.clone()))
            })
        })
        .collect::<Vec<_>>();
    text_candidates.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| right.1.cmp(&left.1))
            .then_with(|| left.2.cmp(&right.2))
    });
    text_candidates.truncate(32);

    let mut candidates = Vec::new();
    for (_, score, id) in text_candidates {
        let Ok(verdict) = footprint_compatibility_ignoring(ctx, symbol_id, &id, ignored_pins)
        else {
            continue;
        };
        let mismatch = verdict.missing_pads.len()
            + verdict.extra_pins.len()
            + usize::from(verdict.polarity_mismatch.is_some());
        candidates.push((mismatch, score, id));
    }
    candidates.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| right.1.cmp(&left.1))
            .then_with(|| left.2.cmp(&right.2))
    });
    Ok(candidates.into_iter().next().map(|(_, _, id)| id))
}

/// Diagnose a footprint ID that cannot be loaded and rank compatible repairs
/// from its own library before the caller mutates a schematic.
pub fn footprint_input_error(
    ctx: &AgentRuntime,
    reference: &str,
    symbol_id: &str,
    footprint: &str,
) -> Result<Option<String>> {
    let catalog = ctx.footprint_catalog()?;
    let id = match FootprintId::parse(footprint) {
        Ok(id) => id,
        Err(_) => {
            let suggestions = catalog.suggest(footprint);
            return Ok(Some(format!(
                "{reference}: {}",
                kicad_footprint::unknown_footprint_message(footprint, &suggestions)
            )));
        }
    };
    match catalog.footprint(&id) {
        Ok(_) => Ok(None),
        Err(error) if error.is_not_found() => {
            let suggestions =
                best_same_library_footprint(ctx, symbol_id, footprint, &BTreeSet::new(), None)?
                    .and_then(|candidate| FootprintId::parse(&candidate).ok())
                    .into_iter()
                    .collect::<Vec<_>>();
            Ok(Some(format!(
                "{reference}: {}",
                kicad_footprint::unknown_footprint_message(footprint, &suggestions)
            )))
        }
        Err(error) => Ok(Some(format!(
            "{reference}: footprint `{footprint}` could not be read: {error}"
        ))),
    }
}

/// Ranked catalog repairs when a requested footprint cannot be loaded.
pub fn unresolved_footprint_suggestions(
    ctx: &AgentRuntime,
    symbol_id: &str,
    footprint: &str,
) -> Result<Option<Vec<String>>> {
    let catalog = ctx.footprint_catalog()?;
    let id = match FootprintId::parse(footprint) {
        Ok(id) => id,
        Err(_) => {
            return Ok(Some(
                catalog
                    .suggest(footprint)
                    .into_iter()
                    .map(|candidate| candidate.to_string())
                    .collect(),
            ));
        }
    };
    match catalog.footprint(&id) {
        Ok(_) => Ok(None),
        Err(_) => {
            let same_library =
                best_same_library_footprint(ctx, symbol_id, footprint, &BTreeSet::new(), None)?;
            let mut suggestions = same_library.into_iter().collect::<Vec<_>>();
            suggestions.extend(
                catalog
                    .suggest(footprint)
                    .into_iter()
                    .map(|candidate| candidate.to_string()),
            );
            let mut seen = BTreeSet::new();
            suggestions.retain(|candidate| seen.insert(candidate.clone()));
            Ok(Some(suggestions))
        }
    }
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
                        ignored_pins: component
                            .pins
                            .iter()
                            .filter(|(_, target)| {
                                matches!(target, sch_check::model::PinTarget::NoConnect)
                            })
                            .map(|(pin, _)| pin.clone())
                            .collect(),
                    })
                })
        }),
    )
}

/// Validate footprint assignments exported by KiCAD before board creation.
pub fn netlist_pin_mismatches(
    ctx: &AgentRuntime,
    netlist: &kicad::Netlist,
    ignored_pins: &BTreeMap<String, BTreeSet<String>>,
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
                    ignored_pins: ignored_pins
                        .get(&component.reference)
                        .cloned()
                        .unwrap_or_default(),
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
        if ctx.provider().symbol(assignment.symbol).is_none() {
            continue;
        }
        let Ok(footprint_id) = FootprintId::parse(assignment.footprint) else {
            continue;
        };
        if catalog.footprint(&footprint_id).is_err() {
            continue; // footprint discovery/regeneration reports lookup failures
        }
        if let Some(mismatch) = assignment_pin_mismatch_ignoring(
            ctx,
            assignment.reference,
            assignment.symbol,
            &footprint_id.to_string(),
            &assignment.ignored_pins,
        )? {
            mismatches.push(mismatch);
        }
    }
    Ok(mismatches)
}

fn mechanical_or_shield_pad(number: &str) -> bool {
    let upper = number.to_ascii_uppercase();
    upper == "MP"
        || upper.starts_with("MP") && upper[2..].chars().all(|ch| ch.is_ascii_digit())
        || upper == "MH"
        || upper.starts_with("MH") && upper[2..].chars().all(|ch| ch.is_ascii_digit())
        || upper == "SH"
        || upper.starts_with("SHIELD")
}

fn footprint_query_for_symbol(symbol_id: &str) -> String {
    let name = symbol_id
        .split_once(':')
        .map_or(symbol_id, |(_, name)| name);
    if name.starts_with("C_Polarized") {
        return "Capacitor_SMD:CP_Elec".to_string();
    }
    if matches!(name, "C" | "C_Small" | "C_US" | "C_Small_US") {
        return "Capacitor_SMD:C_0603_1608Metric".to_string();
    }
    if matches!(name, "R" | "R_Small" | "R_US" | "R_Small_US") {
        return "Resistor_SMD:R_0603_1608Metric".to_string();
    }
    if name.starts_with("LED") {
        return "LED_SMD:LED_0603_1608Metric".to_string();
    }
    if name.starts_with("Barrel_Jack") {
        return "Connector_BarrelJack:BarrelJack".to_string();
    }
    if name.starts_with("AudioJack") {
        return "Connector_Audio:Jack".to_string();
    }
    name.replace('_', " ")
}

/// A footprint the catalog cannot produce, and whether the id itself is at fault.
pub struct UnresolvableFootprint {
    /// A `Lib:Name` the author got wrong, as opposed to one this install simply
    /// does not carry — a project-local library is absent, not mis-assigned.
    pub malformed: bool,
    pub message: String,
}

/// Footprints a design names that the catalog cannot produce, each with the ids
/// it was probably reaching for.
///
/// The board seed fails on all of them, so the schematic gate says so while the
/// schematic is still the thing being edited.
pub fn unresolvable_footprints(
    ctx: &AgentRuntime,
    design: &Design,
) -> Result<Vec<UnresolvableFootprint>> {
    let catalog = ctx.footprint_catalog()?;
    let mut out = Vec::new();
    for block in design.blocks.values() {
        for (reference, component) in &block.components {
            let Some(footprint) = component.footprint.as_deref().filter(|f| !f.is_empty()) else {
                continue;
            };
            let (malformed, problem) = match FootprintId::parse(footprint) {
                Err(_) => (true, catalog.suggest(footprint)),
                Ok(id) => match catalog.footprint(&id) {
                    Ok(_) => continue,
                    Err(e) if e.is_not_found() => (false, catalog.suggest(footprint)),
                    Err(e) => {
                        out.push(UnresolvableFootprint {
                            malformed: false,
                            message: format!(
                                "{reference}: footprint `{footprint}` could not be read: {e}"
                            ),
                        });
                        continue;
                    }
                },
            };
            out.push(UnresolvableFootprint {
                malformed,
                message: format!(
                    "{reference}: {} — use search_footprints{{symbol: \"{}\", query: \
                     \"{}\"}} for a real `Library:Name`, then assign_footprints",
                    kicad_footprint::unknown_footprint_message(footprint, &problem),
                    component.part,
                    footprint,
                ),
            });
        }
    }
    Ok(out)
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
    use super::{
        best_compatible_footprint, capacitor_polarity_mismatch, footprint_compatibility,
        same_symbol_family, search_compatible_footprints,
    };
    use crate::AgentRuntime;
    use kicad_footprint::FootprintId;

    fn footprint(id: &str) -> FootprintId {
        FootprintId::parse(id).expect("valid test footprint id")
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

    #[test]
    fn real_campaign_pairs_are_rejected_with_compatible_suggestions() {
        let Some(ctx) = AgentRuntime::detect_for_test() else {
            eprintln!("SKIP: no KiCad detected");
            return;
        };
        let cases = [
            (
                "Connector:Barrel_Jack",
                "Connector_BarrelJack:BarrelJack_Horizontal",
                Vec::<&str>::new(),
                vec!["3"],
            ),
            (
                "Device:C_Polarized",
                "Capacitor_SMD:C_1206_3216Metric",
                vec![],
                vec![],
            ),
            (
                "Connector_Audio:AudioJack2_Switch",
                "Connector_Audio:Jack_3.5mm_CUI_SJ1-3514N_Horizontal",
                vec!["SN"],
                vec!["R"],
            ),
        ];
        for (symbol, footprint, missing, extra) in cases {
            let verdict = footprint_compatibility(&ctx, symbol, footprint).unwrap();
            assert!(
                !verdict.compatible,
                "{symbol} unexpectedly accepted {footprint}"
            );
            assert_eq!(verdict.missing_pads, missing);
            assert_eq!(verdict.extra_pins, extra);
            if symbol == "Device:C_Polarized" {
                assert!(verdict.polarity_mismatch.is_some());
            }
            let suggestion = best_compatible_footprint(&ctx, symbol, Some(footprint))
                .unwrap()
                .unwrap_or_else(|| panic!("no compatible suggestion for {symbol}"));
            let suggested = footprint_compatibility(&ctx, symbol, &suggestion).unwrap();
            assert!(
                suggested.compatible,
                "suggested {suggestion} does not fit {symbol}: {suggested:?}"
            );
        }
    }

    #[test]
    fn compatibility_tier_precedes_a_closer_text_match() {
        let Some(ctx) = AgentRuntime::detect_for_test() else {
            eprintln!("SKIP: no KiCad detected");
            return;
        };
        let hits = search_compatible_footprints(
            &ctx,
            "Connector:Barrel_Jack",
            Some("Connector_BarrelJack:BarrelJack_Horizontal"),
            usize::MAX,
        )
        .unwrap();
        let incompatible = hits
            .iter()
            .position(|hit| hit.lib_id == "Connector_BarrelJack:BarrelJack_Horizontal")
            .expect("exact text match remains visible");
        assert!(incompatible > 0);
        assert!(hits[..incompatible].iter().all(|hit| hit.compatible));
        assert!(!hits[incompatible].compatible);
        assert_eq!(hits[incompatible].pads, ["1", "2", "3"]);
        assert!(
            hits.iter()
                .all(|hit| hit.lib_id.starts_with("Connector_BarrelJack:"))
        );
    }

    #[test]
    fn symbol_repairs_stay_within_a_named_variant_family() {
        assert!(same_symbol_family(
            "Connector:Barrel_Jack",
            "Connector:Barrel_Jack_Switch"
        ));
        assert!(same_symbol_family(
            "Connector_Audio:AudioJack2",
            "Connector_Audio:AudioJack2_Switch"
        ));
        assert!(!same_symbol_family(
            "Connector:Barrel_Jack",
            "Connector:Conn_01x02_Pin"
        ));
        assert!(!same_symbol_family(
            "Connector:Barrel_Jack",
            "Other:Barrel_Jack_Switch"
        ));
    }
}
