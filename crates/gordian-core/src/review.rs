//! Independent diverse-lens review MECHANICS — domain-agnostic.
//!
//! [`review`] runs a FRESH, history-free [`Provider::complete`] ensemble over a
//! subject (a netlist, a layout, anything the caller can describe in text),
//! parses each pass's verdict JSON, and returns `(lowest score, union of
//! high-confidence critical/major defect lines)`. The DOMAIN supplies the review
//! system prompt and the diverse `lenses`; this module owns only the ensemble
//! loop, the verdict parsing, and the defect-dedup.
//!
//! A single LLM pass has run-to-run variance and a narrow attention span;
//! diverse lenses each catch different fault classes, which beats repeating one
//! prompt. Each lens retries its own JSON parse once; a total parse failure
//! degrades to `(0.0, [])` (conservative — nothing actionable).

use anyhow::Result;
use llm_client::{Message, Provider};
use serde_json::Value;

/// One independent review pass, generalized: the DOMAIN passes the review
/// `system` prompt and the diverse `lenses` (an empty-string lens is the general
/// pass; a non-empty lens appends an "extra emphasis this pass" rider). Returns
/// `(lowest score across lenses, union of high-confidence critical/major defect
/// lines)` — ready to feed back as a fix turn. A total parse failure degrades to
/// `(0.0, [])`.
pub async fn review(
    client: &dyn Provider,
    system: &str,
    lenses: &[&str],
    intent: &str,
    subject: &str,
) -> Result<(f64, Vec<String>)> {
    let msgs = [Message::user(format!("Intended circuit: {intent}\n\nNetlist:\n{subject}"))];
    let mut union: Vec<String> = Vec::new();
    let mut min_score = f64::INFINITY;
    let mut any = false;
    for lens in lenses {
        let lens_system = if lens.is_empty() {
            system.to_string()
        } else {
            format!(
                "{system}\n\nEXTRA EMPHASIS THIS PASS — scrutinise especially: {lens}. \
                 (Still report any other clear electrical fault you notice.)"
            )
        };
        let mut parsed = None;
        for _ in 0..2 {
            let completion = client.complete(&lens_system, &msgs, &[]).await?;
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

/// Two defect lines are "the same" if they target the same refdes (the `- U2:`
/// prefix) — so a union (across lenses, or with a deterministic check layer)
/// never feeds two phrasings of one fault.
pub fn same_defect(a: &str, b: &str) -> bool {
    let refdes =
        |s: &str| s.trim_start_matches("- ").split(':').next().unwrap_or("").trim().to_string();
    let (ra, rb) = (refdes(a), refdes(b));
    !ra.is_empty() && ra == rb
}

/// Pull the verdict JSON from a reasoning+JSON response (prefer the block after
/// `FINAL_JSON:`, else the last balanced `{...}` object).
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

    #[test]
    fn same_defect_matches_by_refdes() {
        assert!(same_defect("- U1: a (b)", "- U1: c (d)"));
        assert!(!same_defect("- U1: a (b)", "- R2: c (d)"));
        assert!(!same_defect("no refdes", "also none"));
    }
}
