//! The independent visual critic: the rendered sheet graded against a human sheet
//! rated exactly 9.
//!
//! The rubric is not written here. [`CRITIC_SYSTEM`] and [`ANCHOR_CALIBRATION`]
//! are the very files `tools/schematic_critic.py` reads, so the critic the agent
//! calls and the fact the quality harness scores it by grade against one text.
//!
//! One vision pass has ±1-2 run-to-run variance, so the sheet is graded
//! [`SAMPLES`] times concurrently and scored by the MEAN — the estimator whose
//! error falls as the root of the sample count, where a modal or median read
//! barely moves. The narrative cannot be averaged, so it is taken whole from the
//! run whose own score sits nearest that mean.

use anyhow::Result;
use gordian_llm::{Binary, ChatMessage, ContentPart, MessageContent, Provider, completed_text};
use serde_json::{Value, json};

/// The rubric, shared verbatim with `tools/schematic_critic.py`.
const CRITIC_SYSTEM: &str = include_str!("../../../tools/schematic_critic_system.txt");

/// The anchor framing, shared verbatim with the same script.
const ANCHOR_CALIBRATION: &str = include_str!("../../../tools/schematic_critic_anchor.txt");

/// What suppresses the two false-positive-prone defect classes when the engine's
/// own geometry says they cannot be there. Shared verbatim with the same script.
const ENGINE_CLEAN: &str = include_str!("../../../tools/schematic_critic_engine_clean.txt");

/// The human sheet every score is calibrated against: equal to it is a 9.
const ANCHOR_PNG: &[u8] = include_bytes!("../assets/reference_9.png");

/// How the reference is described back to the model.
const REFERENCE: &str = "human sheet rated 9";

/// Grades per review. Seven reads bring the spread on one unchanged sheet to
/// ~0.3 of a point; three left it at 1-3, which cannot resolve a sheet.
pub const SAMPLES: usize = 7;

/// One review: the rounded mean, the mean, every sample and one coherent verdict.
pub struct Review {
    pub score: f64,
    pub mean: f64,
    pub samples: Vec<f64>,
    pub verdict: Value,
}

impl Review {
    /// The tool text the model reads: score, one-line summary, then the defects.
    pub fn text(&self) -> String {
        let summary = self
            .verdict
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or("");
        let mut lines = vec![format!(
            "score {:.1}/10 (mean of {} reads {:?}) vs the {REFERENCE}. {summary}",
            self.mean,
            self.samples.len(),
            self.samples
        )];
        for defect in self.defects().iter().take(10) {
            let text = |key: &str| {
                defect
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            };
            lines.push(format!(
                "  - [{}] {} at {}: {} -> {}",
                text("severity"),
                text("category"),
                text("location"),
                text("description"),
                text("fix"),
            ));
        }
        lines.join("\n")
    }

    /// The critic's defect list, worst first.
    pub fn defects(&self) -> Vec<Value> {
        self.verdict
            .get("defects")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    }

    /// The one-line event the transcript shows, in the shape the quality harness
    /// parses (`review <score>/10 ...`).
    pub fn event(&self) -> String {
        format!(
            "review {:.0}/10 mean {:.2} samples {:?} — {} defect(s)",
            self.score,
            self.mean,
            self.samples,
            self.defects().len()
        )
    }
}

/// Grade `clean_png` [`SAMPLES`] times against the embedded anchor.
///
/// `engine_clean` states the engine's own geometry verdict: when it found no wire
/// through a body and no bare wire end, the two false-positive-prone classes are
/// ruled out for the grader instead of being left to its eyes.
pub async fn review(
    client: &dyn Provider,
    clean_png: &[u8],
    parts: &str,
    engine_clean: bool,
) -> Result<Option<Review>> {
    let messages = [ChatMessage::user(MessageContent::from_parts(vec![
        ContentPart::from_text(user_prompt(parts, engine_clean)),
        ContentPart::Binary(png(clean_png.to_vec())),
        ContentPart::Binary(png(ANCHOR_PNG.to_vec())),
    ]))];
    let graded = futures::future::join_all((0..SAMPLES).map(|_| {
        let messages = messages.clone();
        async move {
            match client.complete(CRITIC_SYSTEM, &messages, &[]).await {
                Ok(end) => verdict(&completed_text(&end)),
                Err(error) => {
                    tracing::warn!("critic sample failed: {error:#}");
                    None
                }
            }
        }
    }))
    .await;

    let mut runs: Vec<(f64, Value)> = graded.into_iter().flatten().collect();
    if runs.is_empty() {
        return Ok(None);
    }
    runs.sort_by(|a, b| a.0.total_cmp(&b.0));
    let samples: Vec<f64> = runs.iter().map(|(score, _)| *score).collect();
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    let mut verdict = runs
        .into_iter()
        .reduce(|best, run| {
            if (run.0 - mean).abs() < (best.0 - mean).abs() {
                run
            } else {
                best
            }
        })
        .expect("at least one run")
        .1;
    if engine_clean && let Some(defects) = verdict.get_mut("defects").and_then(Value::as_array_mut)
    {
        defects.retain(|d| {
            !matches!(
                d.get("category").and_then(Value::as_str),
                Some("wire-through-body" | "dangling-pin")
            )
        });
    }
    Ok(Some(Review {
        score: mean.round(),
        mean: (mean * 100.0).round() / 100.0,
        samples,
        verdict,
    }))
}

/// The framing that rides with the two images, in the order the rubric names:
/// the sheet under review first, the reference second.
fn user_prompt(parts: &str, engine_clean: bool) -> String {
    let calibration = ANCHOR_CALIBRATION.trim();
    let ground_truth = if engine_clean {
        format!("\n\n{}", ENGINE_CLEAN.trim())
    } else {
        String::new()
    };
    let parts = parts.chars().take(1500).collect::<String>();
    format!(
        "Audit this rendered schematic for layout quality. Reason first (trace every \
         wire-through-body and dangling-pin candidate to its endpoints), then emit the \
         FINAL_JSON verdict. Parts on the sheet: {parts}.\n\n{calibration}{ground_truth}\n\n\
         Add a \"fix\" field to every defect: the one concrete re-layout that removes it."
    )
}

fn png(bytes: Vec<u8>) -> Binary {
    use base64::Engine;
    Binary::from_base64(
        "image/png",
        base64::engine::general_purpose::STANDARD.encode(bytes),
        None,
    )
}

/// `(score, verdict)` from one grader response, or `None` if it emitted no JSON.
fn verdict(text: &str) -> Option<(f64, Value)> {
    let value = gordian_llm::verdict_json(text)?;
    let score = value.get("score").and_then(Value::as_f64)?;
    Some((score, value))
}

/// The critic's defect list rendered for the composer, which sees no image of its
/// own beyond the current render.
pub fn defect_lines(review: &Review) -> String {
    review
        .defects()
        .iter()
        .map(|d| {
            let text = |key: &str| d.get(key).and_then(Value::as_str).unwrap_or("");
            format!(
                "- [{}] {} at {}: {} -> {}",
                text("severity"),
                text("category"),
                text("location"),
                text("description"),
                text("fix")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The verdict JSON kept in `report.json`.
pub fn as_json(review: &Review) -> Value {
    json!({
        "score": review.score,
        "mean": review.mean,
        "samples": review.samples,
        "reference": REFERENCE,
        "summary": review.verdict.get("summary").cloned().unwrap_or(Value::Null),
        "dimension_scores": review.verdict.get("dimension_scores").cloned().unwrap_or(Value::Null),
        "defects": review.defects(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shared_rubric_and_anchor_are_the_critic_script_s() {
        assert!(CRITIC_SYSTEM.contains("wire-through-body"));
        assert!(ANCHOR_CALIBRATION.contains("rated exactly 9/10"));
        assert!(ANCHOR_PNG.starts_with(&[0x89, b'P', b'N', b'G']));
    }

    #[test]
    fn the_engine_ground_truth_only_rides_along_when_the_geometry_is_clean() {
        assert!(user_prompt("R1=1k", true).contains("AUTHORITATIVE ENGINE GROUND TRUTH"));
        assert!(!user_prompt("R1=1k", false).contains("AUTHORITATIVE ENGINE GROUND TRUTH"));
    }
}
