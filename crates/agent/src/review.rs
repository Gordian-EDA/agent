//! Independent electrical-CORRECTNESS review of a committed netlist — the netlist analog of the
//! layout critic (`tools/schematic_critic.py`). Run as a FRESH [`LlmClient::complete`] call (no
//! conversation history → unbiased; the generating model rationalises its own slips). Catches
//! faults that pass ERC and look clean: pin-function mis-wires, voltage-domain part-selection
//! errors, missing-essential-part and topology errors. Used by [`crate::Agent::run_turn_reviewed`].

use crate::llm::{LlmClient, Message};
use anyhow::Result;
use serde_json::Value;

pub const REVIEW_SYSTEM: &str = r#"You are a senior electronics design engineer performing a NETLIST review (NOT a layout review).

You are given a circuit's intended function and its netlist in circuit-YAML (components with a refdes, a `part:` KiCAD lib_id, an optional `value:`, and pin->net maps; `between:[A,B]` = a 2-pin part across nets A,B; `positive/negative` = a polarized part; `power:NET` = a rail symbol; `label:global` = an exposed port).

Review ONLY for ELECTRICAL-DESIGN CORRECTNESS — faults a netlist can have while still passing ERC (connectivity) and looking clean:
1. PIN-FUNCTION mis-wires: a net wired to the WRONG pin for its function. Use your knowledge of the SPECIFIC part's pinout (e.g. SPI/ISP MISO/MOSI/SCK on the wrong MCU pin; a regulator FB pin not seeing the feedback divider; enable/boot/reset tied wrong).
2. WRONG VALUES: a resistor/cap value wrong by ~an order of magnitude for its role (1M I2C pull-up; 10nF "bulk" cap; a feedback divider whose ratio gives the wrong output voltage).
3. MISSING ESSENTIAL parts (cannot function without): crystal with no load caps; regulator with no output cap; an IC powered with NO decoupling at all.
4. VOLTAGE-DOMAIN / part-selection: a part operated outside its supply range (e.g. a 5V-only transceiver on a 3.3V rail).
5. TOPOLOGY errors: feedback/bias/reference wired wrong; reversed polarity; a missing return path.

Do NOT report layout, naming/style, nice-to-have protection, or anything you are not confident is a real electrical fault. A correct design SHOULD score 9-10 with few/no defects; do NOT invent defects.

REASON step by step FIRST (per IC, state its key pins from your knowledge of that exact part, then trace the critical nets), THEN emit, after a line `FINAL_JSON:`, a JSON object:
{"score": 0-10, "summary": "one line", "defects": [{"severity": "critical|major|minor", "confidence": "high|medium|low", "refdes": "U1", "issue": "short", "why": "the electrical reason"}]}"#;

/// One independent review pass over a netlist. Returns `(score, high-confidence
/// critical/major defect lines)` — the lines are ready to feed back to the agent as a fix turn.
/// A parse failure degrades to `(0.0, [])` (treated as "nothing actionable") rather than erroring.
/// Diverse review LENSES, unioned. A ground-truth recall sweep (tools/recall_harness.py, 25 injected
/// defects) showed repeated SAME-prompt sampling is flat (it can't recover a *consistent* miss), while
/// DIVERSE lenses each catch different fault classes and lift recall (80%→84%, and the clear-defect
/// rate to ~95%). Empty string = the general pass.
const LENSES: &[&str] = &[
    "",
    "power, regulation and analog faults: for EVERY resistor divider feeding a regulator feedback or \
     reference pin, COMPUTE the resulting output voltage from the resistor values and verify it matches \
     the intended rail; also bias/reference networks, voltage-domain part supply ranges, and \
     current-limit / gain resistor values",
    "digital interfaces and clocking: SPI/I2C/UART/ISP bus signals on the correct device pins, \
     crystal/oscillator pin placement, reset/boot/enable/chip-select straps, direction and address pins",
];

/// Review a netlist with a DIVERSE-LENS ENSEMBLE and return `(lowest score, union of high-confidence
/// critical/major defect lines)` — ready to feed back as a fix turn. A single LLM pass has run-to-run
/// variance and a narrow attention span (validated: a 1-sample review rated a 5V-MCU-on-3.3V slip
/// "clean"); diverse lenses (general + power/analog + digital/interface) catch different fault classes,
/// which beats repeating one prompt. Each lens retries its own JSON parse once; a total parse failure
/// degrades to `(0.0, [])` (conservative — nothing actionable).
pub async fn review_netlist(
    client: &dyn LlmClient,
    intent: &str,
    netlist: &str,
) -> Result<(f64, Vec<String>)> {
    let msgs = [Message::user(format!("Intended circuit: {intent}\n\nNetlist:\n{netlist}"))];
    let mut union: Vec<String> = Vec::new();
    let mut min_score = f64::INFINITY;
    let mut any = false;
    for lens in LENSES {
        let system = if lens.is_empty() {
            REVIEW_SYSTEM.to_string()
        } else {
            format!("{REVIEW_SYSTEM}\n\nEXTRA EMPHASIS THIS PASS — scrutinise especially: {lens}. \
                     (Still report any other clear electrical fault you notice.)")
        };
        let mut parsed = None;
        for _ in 0..2 {
            let completion = client.complete(&system, &msgs, &[]).await?;
            if let Some(v) = extract_json(&completion.text) {
                parsed = Some(v);
                break;
            }
        }
        if let Some(v) = parsed {
            any = true;
            let (score, defects) = parse_review(&v);
            min_score = min_score.min(score);
            for d in defects {
                if !union.iter().any(|e| same_defect(e, &d)) {
                    union.push(d);
                }
            }
        }
    }
    if !any {
        return Ok((0.0, Vec::new())); // never parsed ⇒ conservative: nothing actionable
    }
    Ok((min_score, union))
}

/// Two defect lines are "the same" if they target the same refdes (the `- U2:` prefix) — so the
/// union across samples doesn't feed the agent two phrasings of one fault.
fn same_defect(a: &str, b: &str) -> bool {
    let refdes = |s: &str| s.trim_start_matches("- ").split(':').next().unwrap_or("").trim().to_string();
    let (ra, rb) = (refdes(a), refdes(b));
    !ra.is_empty() && ra == rb
}

/// Pull the verdict JSON from a reasoning+JSON response (prefer the block after `FINAL_JSON:`,
/// else the last balanced `{...}` object).
fn extract_json(text: &str) -> Option<Value> {
    let tail = text.rsplit_once("FINAL_JSON:").map(|(_, b)| b).unwrap_or(text);
    let cleaned = tail
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    if let Ok(v) = serde_json::from_str::<Value>(cleaned) {
        return Some(v);
    }
    let (mut depth, mut start, mut last) = (0i32, None, None);
    for (i, b) in text.bytes().enumerate() {
        if b == b'{' {
            if depth == 0 {
                start = Some(i);
            }
            depth += 1;
        } else if b == b'}' {
            depth -= 1;
            if depth == 0 && let Some(s) = start {
                last = Some(&text[s..=i]);
            }
        }
    }
    last.and_then(|s| serde_json::from_str(s).ok())
}

/// `(score, high-confidence critical/major defect lines)` from a verdict object.
fn parse_review(v: &Value) -> (f64, Vec<String>) {
    let score = v.get("score").and_then(Value::as_f64).unwrap_or(0.0);
    let mut defects = Vec::new();
    for d in v.get("defects").and_then(Value::as_array).into_iter().flatten() {
        let sev = d.get("severity").and_then(Value::as_str).unwrap_or("");
        let conf = d.get("confidence").and_then(Value::as_str).unwrap_or("");
        if matches!(sev, "critical" | "major") && conf == "high" {
            let rd = d.get("refdes").and_then(Value::as_str).unwrap_or("");
            let issue = d.get("issue").and_then(Value::as_str).unwrap_or("");
            let why = d.get("why").and_then(Value::as_str).unwrap_or("");
            defects.push(format!("- {rd}: {issue} ({why})"));
        }
    }
    (score, defects)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_final_json_and_filters_to_high_conf_major() {
        let text = r#"reasoning here...
FINAL_JSON:
{"score": 6, "summary": "x", "defects": [
  {"severity":"critical","confidence":"high","refdes":"U1","issue":"a","why":"b"},
  {"severity":"minor","confidence":"high","refdes":"R1","issue":"c","why":"d"},
  {"severity":"major","confidence":"low","refdes":"C1","issue":"e","why":"f"}
]}"#;
        let v = extract_json(text).unwrap();
        let (score, defects) = parse_review(&v);
        assert_eq!(score, 6.0);
        assert_eq!(defects.len(), 1); // only the critical/high one
        assert!(defects[0].contains("U1"));
    }

    #[test]
    fn last_balanced_object_when_no_marker() {
        let text = r#"{"score": 9, "defects": []}"#;
        let (score, defects) = parse_review(&extract_json(text).unwrap());
        assert_eq!(score, 9.0);
        assert!(defects.is_empty());
    }
}
