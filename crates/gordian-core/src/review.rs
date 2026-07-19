//! Independent diverse-lens review MECHANICS — domain-agnostic.
//!
//! [`review`] runs a FRESH, history-free [`Provider::complete`] ensemble over a
//! subject (a netlist, a layout, anything the caller can describe in text),
//! parses each pass's verdict JSON, and returns `(actionable score, union of
//! high-confidence critical/major defect lines)`. The DOMAIN supplies the review
//! system prompt and the diverse `lenses`; this module owns only the ensemble
//! loop, the verdict parsing, and the defect-dedup.
//!
//! A single LLM pass has run-to-run variance and a narrow attention span;
//! diverse lenses each catch different fault classes, which beats repeating one
//! prompt. JSON retries are caller-controlled; a total parse failure degrades to
//! `(0.0, [])` (conservative — nothing actionable).

use gordian_llm::{Binary, ChatMessage, ContentPart, MessageContent, Provider, completed_text};
use anyhow::Result;
use futures::future::join_all;
use serde_json::Value;

/// One independent NETLIST review pass, generalized: the DOMAIN passes the review
/// `system` prompt and the diverse `lenses` (an empty-string lens is the general
/// pass; a non-empty lens appends an "extra emphasis this pass" rider). Returns
/// `(actionable score across lenses, union of high-confidence critical/major defect
/// lines)` — ready to feed back as a fix turn. A total parse failure degrades to
/// `(0.0, [])`.
pub async fn review(
    client: &dyn Provider,
    system: &str,
    lenses: &[&str],
    intent: &str,
    subject: &str,
) -> Result<(f64, Vec<String>)> {
    let prompt = format!("Intended circuit: {intent}\n\nNetlist:\n{subject}");
    review_ensemble(client, system, lenses, &prompt, None, false).await
}

/// One independent LAYOUT (vision) review pass: the same diverse-lens ensemble,
/// but the subject is a rendered image the critic LOOKS at, not text. The DOMAIN
/// passes the vision-critic `system` prompt (e.g. the ported schematic/PCB critic)
/// and the `lenses`; `prompt` is the textual framing that rides alongside the
/// image (intended circuit + "reason first, then FINAL_JSON"). The `image` is
/// attached as a genai [`Binary`] content part so the backend sends it. Returns
/// the same `(actionable score, union of high-confidence defects)` shape, degrading
/// to `(0.0, [])` on a total parse failure — so a flaky vision call never poisons
/// the union with phantom defects.
pub async fn review_image(
    client: &dyn Provider,
    system: &str,
    lenses: &[&str],
    prompt: &str,
    image: Binary,
) -> Result<(f64, Vec<String>)> {
    review_ensemble(client, system, lenses, prompt, Some(image), false).await
}

/// Same as [`review`], but with explicit JSON retry control.
pub async fn review_with_retry(
    client: &dyn Provider,
    system: &str,
    lenses: &[&str],
    intent: &str,
    subject: &str,
    retry_json: bool,
) -> Result<(f64, Vec<String>)> {
    let prompt = format!("Intended circuit: {intent}\n\nNetlist:\n{subject}");
    review_ensemble(client, system, lenses, &prompt, None, retry_json).await
}

/// Same as [`review_image`], but with explicit JSON retry control.
pub async fn review_image_with_retry(
    client: &dyn Provider,
    system: &str,
    lenses: &[&str],
    prompt: &str,
    image: Binary,
    retry_json: bool,
) -> Result<(f64, Vec<String>)> {
    review_ensemble(client, system, lenses, prompt, Some(image), retry_json).await
}

/// The shared lens loop behind [`review`] (text) and [`review_image`] (vision):
/// for each lens, run a FRESH history-free completion over `prompt` (plus the
/// optional `image`), parse its verdict, and union the high-confidence defects.
/// By default each lens runs once; callers may enable one retry on malformed
/// JSON. Degrades to `(0.0, [])` if NO lens ever parsed.
async fn review_ensemble(
    client: &dyn Provider,
    system: &str,
    lenses: &[&str],
    prompt: &str,
    image: Option<Binary>,
    retry_json: bool,
) -> Result<(f64, Vec<String>)> {
    let msgs = [match &image {
        Some(img) => ChatMessage::user(MessageContent::from_parts(vec![
            ContentPart::from_text(prompt),
            ContentPart::Binary(img.clone()),
        ])),
        None => ChatMessage::user(prompt),
    }];
    let passes = lenses.iter().map(|lens| {
        let lens_system = if lens.is_empty() {
            system.to_string()
        } else {
            format!(
                "{system}\n\nEXTRA EMPHASIS THIS PASS — scrutinise especially: {lens}. \
                 (Still report any other clear fault you notice.)"
            )
        };

        let msgs = msgs.clone();
        async move {
            let mut parsed = None;
            let attempts = if retry_json { 2 } else { 1 };
            for _ in 0..attempts {
                let end = client.complete(&lens_system, &msgs, &[]).await?;
                if let Some(v) = extract_json(&completed_text(&end)) {
                    parsed = Some(v);
                    break;
                }
            }
            Ok::<_, anyhow::Error>(parsed)
        }
    });

    let mut union: Vec<String> = Vec::new();
    let mut min_score = f64::INFINITY;
    let mut any = false;
    for parsed in join_all(passes).await {
        if let Some(v) = parsed? {
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
    let refdes = |s: &str| {
        s.trim_start_matches("- ")
            .split(':')
            .next()
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let (ra, rb) = (refdes(a), refdes(b));
    !ra.is_empty() && ra == rb
}

/// Pull the verdict JSON from a reasoning+JSON response (prefer the block after
/// `FINAL_JSON:`, else the last balanced `{...}` object).
fn extract_json(text: &str) -> Option<Value> {
    let tail = text
        .rsplit_once("FINAL_JSON:")
        .map(|(_, b)| b)
        .unwrap_or(text);
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
            if depth == 0
                && let Some(s) = start
            {
                last = Some(&text[s..=i]);
            }
        }
    }
    last.and_then(|s| serde_json::from_str(s).ok())
}

const NO_ACTIONABLE_DEFECT_FLOOR: f64 = 8.0;

/// `(score, high-confidence critical/major defect lines)` from a verdict object.
///
/// Handles BOTH verdict shapes the critics emit, since the same dedup/feedback
/// machinery serves them: the netlist critic's `{refdes, issue, why, evidence}`
/// and the vision layout critic's `{location, category, description}`. Each defect
/// renders to the same `- <target>: <issue> (<why>)` line so [`same_defect`] can
/// dedup a layout defect against a netlist one on the shared `<target>` prefix.
///
/// Important: only grounded, high-confidence major/critical defects are actionable
/// in the agent loop. If a reviewer returns `score: 3` but its defects are all
/// minor, medium/low confidence, ungrounded, or empty, that score is noise for
/// gating purposes. In that case clamp it to the non-gating band.
fn parse_review(v: &Value) -> (f64, Vec<String>) {
    let mut score = v.get("score").and_then(Value::as_f64).unwrap_or(0.0);
    let mut defects = Vec::new();
    for d in v
        .get("defects")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let sev = d.get("severity").and_then(Value::as_str).unwrap_or("");
        let conf = d.get("confidence").and_then(Value::as_str).unwrap_or("");
        if matches!(sev, "critical" | "major") && conf == "high" && defect_is_grounded(d) {
            let str_of = |k: &str| d.get(k).and_then(Value::as_str).unwrap_or("");
            // refdes/location = the target prefix; issue/description = the fault;
            // why/category = the reason. Prefer the netlist keys, fall back to the
            // layout keys.
            let target = pick(str_of("refdes"), str_of("location"));
            let issue = pick(str_of("issue"), str_of("description"));
            let why = pick(str_of("why"), str_of("category"));
            defects.push(format!("- {target}: {issue} ({why})"));
        }
    }
    if defects.is_empty() && score > 0.0 {
        score = score.max(NO_ACTIONABLE_DEFECT_FLOOR);
    }
    (score, defects)
}

fn defect_is_grounded(d: &Value) -> bool {
    if d.get("refdes").is_some() && evidence_text(d).trim().is_empty() {
        return false;
    }
    if d.get("category").and_then(Value::as_str) == Some("text-overlap") {
        return describes_actual_text_collision(d);
    }
    true
}

fn evidence_text(d: &Value) -> String {
    match d.get("evidence") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn describes_actual_text_collision(d: &Value) -> bool {
    let str_of = |k: &str| d.get(k).and_then(Value::as_str).unwrap_or("");
    let text =
        format!("{}\n{}", str_of("description"), str_of("verification")).to_ascii_lowercase();
    [
        "overlap",
        "collid",
        "merge",
        "touch",
        "abut",
        "cover",
        "obscur",
        "on top of",
        "intersect",
        "superimpos",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

/// The first non-empty of two field values (the netlist key, then the layout key).
fn pick<'a>(primary: &'a str, fallback: &'a str) -> &'a str {
    if primary.is_empty() {
        fallback
    } else {
        primary
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_final_json_and_filters_to_high_conf_major() {
        let text = r#"reasoning here...
FINAL_JSON:
{"score": 6, "summary": "x", "defects": [
  {"severity":"critical","confidence":"high","refdes":"U1","issue":"a","why":"b","evidence":"U1.pins.VDD = 12V"},
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

    #[test]
    fn parses_the_layout_critic_verdict_shape() {
        // The vision critic uses location/category/description, not refdes/issue/why.
        let text = r#"reasoning...
FINAL_JSON:
{"score": 5, "dimension_scores": {"readability": 5}, "defects": [
  {"severity":"major","confidence":"high","category":"spacing","location":"C1","description":"decoupling cap stranded far from U1's power pin"},
  {"severity":"minor","confidence":"high","category":"orientation","location":"R3","description":"vertical series part"}
]}"#;
        let (score, defects) = parse_review(&extract_json(text).unwrap());
        assert_eq!(score, 5.0);
        assert_eq!(defects.len(), 1, "only the high-confidence major");
        assert!(
            defects[0].starts_with("- C1:"),
            "location → target prefix: {defects:?}"
        );
        assert!(defects[0].contains("decoupling cap"));
    }

    #[test]
    fn low_score_without_actionable_defects_is_clamped() {
        let text = r#"FINAL_JSON:
{"score": 3, "defects": [
  {"severity":"minor","confidence":"high","refdes":"R1","issue":"cosmetic","why":"style"},
  {"severity":"major","confidence":"low","refdes":"C1","issue":"guess","why":"uncertain"}
]}"#;
        let (score, defects) = parse_review(&extract_json(text).unwrap());
        assert!(
            defects.is_empty(),
            "minor/low-confidence issues are not actionable: {defects:?}"
        );
        assert_eq!(
            score, 8.0,
            "raw 3/10 with no actionable defect must not gate a fix loop"
        );
    }

    #[test]
    fn low_score_with_actionable_defect_is_preserved() {
        let text = r#"FINAL_JSON:
{"score": 3, "defects": [
  {"severity":"critical","confidence":"high","refdes":"U1","issue":"wrong rail","why":"VDD on 12V","evidence":"U1.pins.VDD = 12V"}
]}"#;
        let (score, defects) = parse_review(&extract_json(text).unwrap());
        assert_eq!(score, 3.0);
        assert_eq!(defects.len(), 1);
    }

    #[test]
    fn ungrounded_netlist_defect_is_not_actionable() {
        let text = r#"FINAL_JSON:
{"score": 2, "defects": [
  {"severity":"major","confidence":"high","refdes":"U1","issue":"wrong/missing power pin wiring","why":"Only VDD pins appear partially assigned; pin 7 is NRST, but other required VDD/GND pins are not shown as tied consistently."}
]}"#;
        let (score, defects) = parse_review(&extract_json(text).unwrap());
        assert!(
            defects.is_empty(),
            "netlist defects must cite exact netlist evidence, not visual inference: {defects:?}"
        );
        assert_eq!(score, 8.0);
    }

    #[test]
    fn cramped_text_without_collision_is_not_actionable_overlap() {
        let text = r#"FINAL_JSON:
{"score": 5, "defects": [
  {"severity":"major","confidence":"high","category":"text-overlap","location":"J1/upper-right input block","description":"Input block has cramped net/signal text packed tightly together, making associations harder to read at a glance.","verification":"5V,GND, VDD, GND, and J1 label are close together."}
]}"#;
        let (score, defects) = parse_review(&extract_json(text).unwrap());
        assert!(
            defects.is_empty(),
            "cramped but non-colliding text is not an actionable overlap: {defects:?}"
        );
        assert_eq!(score, 8.0);
    }

    #[test]
    fn actual_text_collision_remains_actionable() {
        let text = r#"FINAL_JSON:
{"score": 5, "defects": [
  {"severity":"major","confidence":"high","category":"text-overlap","location":"R1/C1","description":"The GND label overlaps the R1 value text, merging the strings visually.","verification":"The GND characters collide with the 10k value text."}
]}"#;
        let (score, defects) = parse_review(&extract_json(text).unwrap());
        assert_eq!(score, 5.0);
        assert_eq!(defects.len(), 1);
        assert!(defects[0].starts_with("- R1/C1:"));
    }

    #[tokio::test]
    async fn review_lenses_are_dispatched_concurrently() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::time::{Duration, sleep};

        struct SlowReviewer {
            in_flight: Arc<AtomicUsize>,
            max_in_flight: Arc<AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl Provider for SlowReviewer {
            async fn complete(
                &self,
                _system: &str,
                _messages: &[ChatMessage],
                _tools: &[crate::Tool],
            ) -> anyhow::Result<crate::StreamEnd> {
                let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                self.max_in_flight.fetch_max(now, Ordering::SeqCst);
                sleep(Duration::from_millis(20)).await;
                self.in_flight.fetch_sub(1, Ordering::SeqCst);
                Ok(crate::StreamEnd {
                    captured_content: Some(MessageContent::from_text(
                        r#"FINAL_JSON: {"score": 9, "defects": []}"#,
                    )),
                    ..Default::default()
                })
            }
        }

        let max_in_flight = Arc::new(AtomicUsize::new(0));
        let reviewer = SlowReviewer {
            in_flight: Arc::new(AtomicUsize::new(0)),
            max_in_flight: Arc::clone(&max_in_flight),
        };

        let (_score, defects) = review_ensemble(
            &reviewer,
            "sys",
            &["", "power", "digital"],
            "prompt",
            None,
            false,
        )
        .await
        .unwrap();

        assert!(defects.is_empty());
        assert!(
            max_in_flight.load(Ordering::SeqCst) > 1,
            "independent lenses should overlap instead of running serially"
        );
    }
}
