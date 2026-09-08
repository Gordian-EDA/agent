//! Skill starters: the verified part list, pin map and ready-to-build design
//! JSON of a circuit close to what was asked for.
//!
//! A matching starter is what turns a Blue Pill into a one-shot. When the
//! deterministic TRIGGER pass claims the prompt the run does not even ask the
//! model to re-emit that design: [`starter`] hands it to the build tool as v1
//! before the first request, and the model reviews a sheet that already exists.
//! Otherwise the block still rides with the opening message as a starting point.

use serde_json::Value;

/// How many starters ride with a prompt.
const TOP_K: usize = 2;

/// The instruction that frames the starters.
const FRAMING: &str = "If one of these skills covers the request, `build` ITS DESIGN JSON AS IT STANDS first - \
    that layout already scores 9 with the reviewer - and change only where the request differs from it. Every lib id, \
    pin key, footprint and net in a skill is verified against the installed libraries, so do not look those parts up \
    again with `search_symbols` or `symbol_info`, do not add parts the skill does not have, and copy its pin keys \
    character for character. Then fix only what the checks and the review report. Do not start from a blank sheet \
    when a skill covers the circuit.";

/// A skill whose design the run builds itself, before the model is asked anything.
pub struct Starter {
    pub name: String,
    pub design: Value,
}

/// The skill to build unattended for `prompt`, if any.
///
/// Only a TRIGGER match qualifies: a fuzzy hit says "close to this family",
/// which is a good starting point for the model but not a design to build
/// without being asked. A skill whose `## Layout` JSON does not parse falls back
/// to the ordinary flow with a warning.
pub fn starter(prompt: &str) -> Option<Starter> {
    let skill = gordian_skills::select(prompt, 1)
        .into_iter()
        .next()
        .filter(|s| s.matches_trigger(prompt))?;
    match skill.design() {
        Some(design) => Some(Starter {
            name: skill.name,
            design,
        }),
        None => {
            tracing::warn!(
                skill = %skill.name,
                "skill has no buildable design; falling back to the model-driven flow"
            );
            None
        }
    }
}

/// The context block appended to the opening user message, or empty when nothing
/// in the library matches.
pub fn prompt_block(prompt: &str) -> String {
    let block = gordian_skills::prompt_block(&gordian_skills::select(prompt, TOP_K));
    if block.is_empty() {
        String::new()
    } else {
        format!("{block}\n{FRAMING}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blue_pill_request_finds_its_starter() {
        let block = prompt_block("Design an STM32F103C8T6 'Blue Pill' development board");
        if block.is_empty() {
            return; // no skill library installed on this machine
        }
        assert!(block.contains("# Relevant design skills"));
        assert!(block.contains(FRAMING));
    }

    #[test]
    fn a_trigger_match_yields_a_design_to_build_unattended() {
        let Some(found) = starter("Design an STM32F103C8T6 'Blue Pill' development board") else {
            return; // no skill library installed on this machine
        };
        assert_eq!(found.name, "stm32f103-blue-pill");
        assert_eq!(found.design["paper"], "A3");
        assert!(found.design["parts"].as_array().is_some_and(|p| p.len() > 20));
    }

    /// Skill-first hands a starter to the engine unattended, so every starter
    /// must at least lay out. The residual defects each one still carries are
    /// listed, because a starter with a real (non-cosmetic) defect costs the run
    /// the extra build that skill-first exists to save.
    #[test]
    fn every_starter_lays_out_and_its_residue_is_named() {
        let Ok(config) = crate::platform::load_config() else {
            return; // no KiCad installation on this machine
        };
        let Some(symbols) = config.kicad.symbol_dir.filter(|d| d.is_dir()) else {
            return;
        };
        let lib = crate::engines::sch::Library::load(&symbols).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut stale = Vec::new();
        for skill in gordian_skills::all() {
            let design = skill.design().expect("a skill without a design");
            let out = dir.path().join(format!("{}.kicad_sch", skill.name));
            let report = crate::engines::sch::build(&lib, &design, &out)
                .unwrap_or_else(|e| panic!("{} does not lay out: {e:#}", skill.name));
            let real: Vec<&String> = report
                .issues
                .iter()
                .filter(|i| !i.ends_with("texts must not overlap"))
                .collect();
            if !real.is_empty() || report.notes.iter().any(|n| n.contains("did not fit")) {
                stale.push(format!("{}: {real:?}", skill.name));
            }
        }
        // The Blue Pill is the starter skill-first is measured on: its residue
        // must stay cosmetic, or a seeded run spends its budget repairing it.
        assert!(
            !stale.iter().any(|s| s.starts_with("stm32f103-blue-pill")),
            "the Blue Pill starter has drifted from the engine: {stale:?}"
        );
    }

    /// A request that names no skill's trigger reaches the library only through
    /// the fuzzy pass, which is not specific enough to build without asking.
    #[test]
    fn a_fuzzy_only_match_is_not_built_unattended() {
        assert!(starter("a low-noise instrumentation front end for a strain gauge").is_none());
        assert!(starter("summarise the quarterly sales report").is_none());
    }
}
