//! Reference-design skills: `skills/<name>/SKILL.md` data files, selected against a
//! user prompt and injected into the agent's system prompt.
//!
//! A skill is markdown with YAML-ish frontmatter (`name`, `description`, `triggers`)
//! and a body holding the verified parts list, pin map, layout JSON and checklists for
//! one circuit family. Selection is two-stage: a deterministic trigger-keyword pass,
//! then [`fuzzy_matcher`]'s `SkimMatcherV2` ranking of the prompt against each skill's
//! triggers and name, so a paraphrase still finds the right design.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;

/// One `SKILL.md` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub triggers: Vec<String>,
    /// Markdown after the frontmatter.
    pub body: String,
}

/// A fuzzy match must reach this fraction of a pattern's self-match score to count.
const FUZZY_MIN: f64 = 0.80;

/// Directory holding `<skill>/SKILL.md`: `$GORDIAN_SKILLS_DIR`, else `<repo>/skills`.
pub fn skills_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("GORDIAN_SKILLS_DIR") {
        return PathBuf::from(dir);
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../skills")
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from("skills"))
}

/// All skills in [`skills_dir`], sorted by name. Parse failures are skipped with a warning.
pub fn load_dir(dir: &Path) -> Vec<Skill> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        tracing::warn!(dir = %dir.display(), "skills directory not readable");
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let file = entry.path().join("SKILL.md");
        if !file.is_file() {
            continue;
        }
        match std::fs::read_to_string(&file)
            .map_err(anyhow::Error::from)
            .and_then(|t| parse(&t))
        {
            Ok(skill) => out.push(skill),
            Err(err) => tracing::warn!(file = %file.display(), %err, "skipping malformed skill"),
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Process-wide cache of [`skills_dir`].
pub fn all() -> &'static [Skill] {
    static CACHE: OnceLock<Vec<Skill>> = OnceLock::new();
    CACHE.get_or_init(|| load_dir(&skills_dir()))
}

/// The `k` skills most relevant to `prompt`, best first; empty when nothing matches.
pub fn select(prompt: &str, k: usize) -> Vec<Skill> {
    select_from(all(), prompt, k)
}

/// [`select`] against an explicit candidate set.
pub fn select_from(skills: &[Skill], prompt: &str, k: usize) -> Vec<Skill> {
    let haystack = prompt.to_lowercase();
    let matcher = SkimMatcherV2::default().ignore_case();

    let mut scored: Vec<(f64, &Skill)> = skills
        .iter()
        .filter_map(|s| {
            let trigger = trigger_score(s, &haystack);
            let score = if trigger > 0.0 {
                1000.0 + trigger
            } else {
                let fuzzy = fuzzy_score(&matcher, &haystack, s);
                if fuzzy < FUZZY_MIN {
                    return None;
                }
                fuzzy * 100.0
            };
            Some((score, s))
        })
        .collect();

    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
    scored.into_iter().take(k).map(|(_, s)| s.clone()).collect()
}

/// Deterministic pass: sum of the lengths of the triggers occurring as whole words in `haystack`.
fn trigger_score(skill: &Skill, haystack: &str) -> f64 {
    skill
        .triggers
        .iter()
        .map(|t| t.to_lowercase())
        .filter(|t| !t.is_empty() && contains_word(haystack, t))
        .map(|t| t.len() as f64)
        .sum()
}

/// Substring match that does not start or end mid-word (so "can" misses "candidate").
fn contains_word(haystack: &str, needle: &str) -> bool {
    let boundary = |c: Option<char>| c.is_none_or(|c| !c.is_alphanumeric());
    haystack.match_indices(needle).any(|(i, _)| {
        let before = haystack[..i].chars().next_back();
        let after = haystack[i + needle.len()..].chars().next();
        // A part-number trigger may carry a package suffix in the prompt ("bq76930" / "BQ76930DBT").
        let part_number = needle.len() >= 5 && needle.chars().any(|c| c.is_ascii_digit());
        boundary(before) && (boundary(after) || part_number)
    })
}

/// Best subsequence match of any trigger or of the name, normalised by its self-match score.
fn fuzzy_score(matcher: &SkimMatcherV2, haystack: &str, skill: &Skill) -> f64 {
    // Only the curated patterns: single description words are far too generic to rank on.
    let patterns = skill
        .triggers
        .iter()
        .map(String::as_str)
        .chain([skill.name.as_str()]);

    patterns
        .filter(|p| p.chars().count() >= 4)
        .filter_map(|p| {
            let ideal = matcher.fuzzy_match(p, p)? as f64;
            let got = matcher.fuzzy_match(haystack, p)? as f64;
            Some((got / ideal).clamp(0.0, 1.0))
        })
        .fold(0.0, f64::max)
}

/// Render selected skills for injection into the system prompt.
pub fn prompt_block(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "# Relevant design skills\n\
         Verified reference designs for this request. Their parts, pin maps and layout JSON were\n\
         checked against the installed KiCad libraries and build with zero ERC violations: reuse\n\
         them, adapting values and extra circuitry to the user's wording. Confirm with\n\
         `search_symbols` / `symbol_info` any lib id, pin name or footprint you add yourself.\n\
         Rules that hold for every design, skill or not:\n\
         - Ground symbols point down, positive supplies point up.\n\
         - One decoupling capacitor per supply pin, drawn beside the pin it serves.\n\
         - Instantiate every mutually exclusive strap or option and leave the inactive one open,\n\
           rather than omitting it.\n\
         - Every pin is connected, `nc`, or deliberately `float`; a net with one pin is a bug.\n",
    );
    for s in skills {
        out.push_str(&format!(
            "\n## Skill: {}\n{}\n\n{}\n",
            s.name,
            s.description,
            s.body.trim()
        ));
    }
    out
}

/// Split `---` frontmatter from the body and read `name`, `description`, `triggers`.
pub fn parse(text: &str) -> anyhow::Result<Skill> {
    let rest = text
        .strip_prefix("---\n")
        .ok_or_else(|| anyhow::anyhow!("missing leading `---` frontmatter fence"))?;
    let (front, body) = rest
        .split_once("\n---")
        .ok_or_else(|| anyhow::anyhow!("unterminated frontmatter"))?;

    let mut name = String::new();
    let mut description = String::new();
    let mut triggers = Vec::new();
    let mut in_triggers = false;
    for line in front.lines() {
        if let Some(item) = line.trim().strip_prefix("- ").filter(|_| in_triggers) {
            triggers.push(unquote(item));
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        in_triggers = false;
        let value = value.trim();
        match key.trim() {
            "name" => name = unquote(value),
            "description" => description = unquote(value),
            "triggers" => {
                in_triggers = true;
                triggers = parse_inline_list(value);
            }
            _ => {}
        }
    }
    anyhow::ensure!(!name.is_empty(), "frontmatter has no `name`");
    Ok(Skill {
        name,
        description,
        triggers,
        body: body.trim_start_matches(['-', '\n']).trim().to_string(),
    })
}

/// `[a, "b, c", 'd']` -> three entries; commas inside quotes do not split.
fn parse_inline_list(value: &str) -> Vec<String> {
    let Some(inner) = value
        .trim()
        .strip_prefix('[')
        .and_then(|v| v.strip_suffix(']'))
    else {
        return Vec::new();
    };
    let mut items = Vec::new();
    let mut item = String::new();
    let mut quote = None;
    for c in inner.chars() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, ',') => items.push(std::mem::take(&mut item)),
            _ => item.push(c),
        }
    }
    items.push(item);
    items
        .into_iter()
        .map(|s| unquote(&s))
        .filter(|s| !s.is_empty())
        .collect()
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    s.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| s.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(s)
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_skills() -> Vec<Skill> {
        let skills = load_dir(&skills_dir());
        assert!(
            skills.len() >= 10,
            "expected the skill library to be present, got {}",
            skills.len()
        );
        skills
    }

    #[test]
    fn frontmatter_and_body_round_trip() {
        let s = parse(
            "---\nname: demo\ndescription: A demo skill.\ntriggers: [foo, \"bar baz\"]\n---\n\n# Parts\nbody\n",
        )
        .unwrap();
        assert_eq!(s.name, "demo");
        assert_eq!(s.triggers, ["foo", "bar baz"]);
        assert!(s.body.starts_with("# Parts"));
    }

    #[test]
    fn block_list_triggers() {
        let s = parse("---\nname: d\ntriggers:\n  - one\n  - \"two three\"\n---\nbody\n").unwrap();
        assert_eq!(s.triggers, ["one", "two three"]);
    }

    #[test]
    fn every_skill_parses_and_is_named_after_its_directory() {
        for entry in std::fs::read_dir(skills_dir()).unwrap().flatten() {
            let file = entry.path().join("SKILL.md");
            if !file.is_file() {
                continue;
            }
            let skill = parse(&std::fs::read_to_string(&file).unwrap()).unwrap();
            assert_eq!(skill.name, entry.file_name().to_string_lossy());
            assert!(
                !skill.description.is_empty(),
                "{} has no description",
                skill.name
            );
            assert!(!skill.triggers.is_empty(), "{} has no triggers", skill.name);
            assert!(
                skill.body.contains("## Layout"),
                "{} has no Layout section",
                skill.name
            );
        }
    }

    fn stub(name: &str, description: &str, triggers: &[&str]) -> Skill {
        Skill {
            name: name.into(),
            description: description.into(),
            triggers: triggers.iter().map(|s| s.to_string()).collect(),
            body: "## Layout\n{}".into(),
        }
    }

    #[test]
    fn fuzzy_pass_catches_paraphrases_but_not_noise() {
        let skills = [
            stub(
                "ne555-blinker-ldo",
                "NE555 astable blinker with an LDO",
                &["ne555", "555 timer"],
            ),
            stub(
                "hbridge",
                "Discrete MOSFET H-bridge for a brushed DC motor",
                &["h-bridge", "motor driver"],
            ),
        ];
        // Paraphrase: no trigger is present verbatim, the fuzzy pass still finds it.
        let hits = select_from(&skills, "a discrete hbridge for a 12V brushed motor", 1);
        assert_eq!(hits[0].name, "hbridge");
        // Noise must stay below the threshold.
        assert!(select_from(&skills, "book a flight to Lisbon next Tuesday", 2).is_empty());
        assert!(select_from(&skills, "", 2).is_empty());
    }

    #[test]
    fn triggers_match_whole_words_only() {
        let skills = [stub("can-node", "CAN transceiver node", &["can", "canh"])];
        assert!(select_from(&skills, "a candidate scanner", 1).is_empty());
        assert_eq!(
            select_from(&skills, "add a CAN bus node", 1)[0].name,
            "can-node"
        );
    }

    #[test]
    fn blue_pill_prompt_selects_the_blue_pill_skill() {
        let hits = select_from(
            &repo_skills(),
            "Design an STM32F103C8T6 'Blue Pill' development board",
            3,
        );
        assert_eq!(hits[0].name, "stm32f103-blue-pill");
    }

    #[test]
    fn ne555_prompt_selects_the_blinker_skill() {
        let hits = select_from(&repo_skills(), "NE555 astable LED blinker with a 5V LDO", 3);
        assert_eq!(hits[0].name, "ne555-blinker-ldo");
    }

    /// One representative prompt per skill, phrased as the quality cases phrase them.
    #[test]
    fn every_skill_is_reachable_from_a_realistic_prompt() {
        let skills = repo_skills();
        let cases = [
            (
                "Design an ATmega328P Arduino-style board with a CH340C bridge",
                "atmega328p-arduino",
            ),
            (
                "A single-supply dual-op-amp audio preamplifier with an MCP6002 and a TLE2426 midrail",
                "audio-preamp",
            ),
            (
                "Common-emitter 2N3904 preamp with voltage-divider bias",
                "bjt-preamp",
            ),
            (
                "10-series Li-ion battery management around a BQ76930DBT",
                "bms-10s",
            ),
            (
                "A 12 V to 5 V 1 A buck converter with a real integrated regulator",
                "buck-converter",
            ),
            (
                "A 3.3 V CAN transceiver interface with switchable 120 ohm termination",
                "can-node",
            ),
            (
                "USB-C powered ESP32-WROOM environmental sensor node with a BME280",
                "esp32-wroom-sensor-node",
            ),
            ("Discrete H-bridge for a 12 V brushed DC motor", "hbridge"),
            (
                "A 3.3 V I2C temperature sensor breakout with an address strap",
                "i2c-sensor-breakout",
            ),
            (
                "A compact 5 V low-side status LED driver with an NPN transistor",
                "led-driver",
            ),
            (
                "NE555 astable LED blinker with a 5 V LDO",
                "ne555-blinker-ldo",
            ),
            (
                "A second-order active low-pass filter on one half of a dual op-amp",
                "sallen-key-filter",
            ),
            (
                "STM32F103C8T6 Blue Pill development board",
                "stm32f103-blue-pill",
            ),
            (
                "STM32F405RGTx controller with an onboard 3.3 V buck supply",
                "stm32f4-buck",
            ),
            (
                "A USB-C sink power entry with CC pull-downs and ESD protection",
                "usb-c-power-input",
            ),
        ];
        assert_eq!(
            cases.len(),
            skills.len(),
            "a skill has no prompt in this table"
        );
        for (prompt, want) in cases {
            let hits = select_from(&skills, prompt, 1);
            assert_eq!(
                hits.first().map(|s| s.name.as_str()),
                Some(want),
                "prompt: {prompt}"
            );
        }
    }

    #[test]
    fn unrelated_prompt_selects_nothing() {
        let hits = select_from(
            &repo_skills(),
            "Please summarise the quarterly sales report for me",
            3,
        );
        assert!(
            hits.is_empty(),
            "unexpected matches: {:?}",
            hits.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn top_k_is_respected_and_ordered() {
        let prompt = "an ESP32-WROOM sensor node board and, on the same sheet, an NE555 blinker";
        let hits = select_from(&repo_skills(), prompt, 2);
        assert_eq!(
            hits.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["esp32-wroom-sensor-node", "ne555-blinker-ldo"]
        );
        assert_eq!(select_from(&repo_skills(), prompt, 1).len(), 1);
    }

    #[test]
    fn prompt_block_is_empty_without_skills() {
        assert!(prompt_block(&[]).is_empty());
        let block = prompt_block(&select_from(&repo_skills(), "blue pill", 1));
        assert!(block.starts_with("# Relevant design skills"));
        assert!(block.contains("## Skill: stm32f103-blue-pill"));
    }
}
