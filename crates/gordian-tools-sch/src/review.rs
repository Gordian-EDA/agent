//! `review_schematic` — an independent, anchored visual critic of the live sheet.
//!
//! The rubric is not written here: [`CRITIC_SYSTEM`] and [`ANCHOR_CALIBRATION`]
//! are the very files `tools/schematic_critic.py` reads, so the tool the agent
//! calls and the harness fact it is scored by grade against one text. The anchor
//! is a human-drawn sheet rated exactly 9 — equal to it is a 9, better a 10 —
//! which is what makes a score comparable across circuits.
//!
//! A single vision pass has ±1-2 run-to-run variance, so the sheet is graded
//! [`SAMPLES`] times concurrently and scored by the MEAN of the samples — the
//! estimator whose error falls as the root of the sample count, where a modal or
//! median read barely moves. The narrative cannot be averaged, so it is taken
//! whole from the run whose own score sits nearest that mean. This is exactly
//! what `tools/schematic_critic.py` does, so the tool the agent calls and the
//! harness fact it is scored by agree on the number.
//!
//! The grader is shown the same two pictures the harness shows it — the clean
//! sheet and the reference — and nothing else. It used to get a third image, the
//! millimetre-overlay render, purely so defects could carry `at_mm`; that made
//! the two critics grade different pictures for a coordinate the model never
//! needed, since `arrange` takes blocks and refdes, not millimetres.

use anyhow::Result;
use gordian_llm::{Binary, ChatMessage, ContentPart, MessageContent, Provider, completed_text};
use gordian_runtime::AgentRuntime;
use gordian_runtime::tool::IMAGE_PATH_KEY;
use serde_json::{Value, json};

use crate::render::{SheetPngs, sheet_pngs};

/// The critic rubric, shared verbatim with `tools/schematic_critic.py`.
const CRITIC_SYSTEM: &str = include_str!("../../../tools/schematic_critic_system.txt");

/// The anchor framing, shared verbatim with `tools/schematic_critic.py`.
const ANCHOR_CALIBRATION: &str = include_str!("../../../tools/schematic_critic_anchor.txt");

/// What suppresses the two false-positive-prone defect classes when the engine's
/// own geometry says they cannot be there. Shared verbatim with the same script.
const ENGINE_CLEAN: &str = include_str!("../../../tools/schematic_critic_engine_clean.txt");

/// The default anchor: the human sheet the harness calibrates every schematic
/// score against. Embedded so the tool is anchored wherever the agent runs.
const DEFAULT_ANCHOR: &[u8] = include_bytes!("../../../quality/anchor/schematic-9.png");

/// How the reference is described back to the model.
const REFERENCE: &str = "human sheet rated 9";

/// Grades per review. Seven reads bring the spread on one unchanged sheet to
/// ~0.3 of a point; three left it at 1-3, which cannot resolve a sheet.
pub const SAMPLES: usize = 7;

/// Everything the critic looks at, prepared off the async loop.
pub struct Subject {
    sheet: SheetPngs,
    anchor: Binary,
}

impl Subject {
    /// The pair of renders plus the reference PNG the critic grades against.
    pub fn new(sheet: SheetPngs, anchor_png: Vec<u8>) -> Self {
        Self {
            sheet,
            anchor: png(anchor_png),
        }
    }

    /// Where the annotated render was saved.
    pub fn annotated_path(&self) -> String {
        self.sheet.annotated_path.display().to_string()
    }
}

/// Render the sheet and load the anchor. Blocking: KiCAD exports the SVG.
pub fn prepare(ctx: &AgentRuntime) -> Result<Result<Subject, Value>> {
    if !ctx.sch_path().is_file() {
        return Ok(Err(json!({
            "error": format!(
                "no schematic at {} yet — create one before reviewing it",
                ctx.sch_path().display()
            ),
        })));
    }
    Ok(Ok(Subject::new(sheet_pngs(ctx)?, DEFAULT_ANCHOR.to_vec())))
}

fn png(bytes: Vec<u8>) -> Binary {
    use base64::Engine;
    Binary::from_base64(
        "image/png",
        base64::engine::general_purpose::STANDARD.encode(bytes),
        None,
    )
}

/// Grade the sheet [`SAMPLES`] times and report the mean score with the
/// narrative of the run nearest that mean.
pub async fn review(client: &dyn Provider, subject: &Subject) -> Result<Value> {
    let messages = [ChatMessage::user(MessageContent::from_parts(vec![
        ContentPart::from_text(user_prompt(&subject.sheet)),
        ContentPart::Binary(png(subject.sheet.clean.clone())),
        ContentPart::Binary(subject.anchor.clone()),
    ]))];
    let graded = futures::future::join_all((0..SAMPLES).map(|_| {
        let messages = messages.clone();
        async move {
            let end = client.complete(CRITIC_SYSTEM, &messages, &[]).await?;
            Ok::<_, anyhow::Error>(verdict(&completed_text(&end)))
        }
    }))
    .await;

    let mut runs = Vec::new();
    for run in graded {
        if let Some(run) = run? {
            runs.push(run);
        }
    }
    if runs.is_empty() {
        return Ok(json!({
            "error": "the visual critic returned no parsable verdict; try review_schematic again",
        }));
    }
    let Graded {
        score,
        mean,
        samples,
        verdict,
    } = grade(runs);
    let mut result = json!({
        "ok": true,
        "score": score,
        "mean": mean,
        "samples": samples,
        "reference": REFERENCE,
        "summary": verdict.get("summary").cloned().unwrap_or(Value::Null),
        "defects": defects(&verdict),
        "note": format!(
            "An independent critic graded the rendered sheet {SAMPLES} times against a {REFERENCE}; \
             `mean` is the mean of the `samples`, `score` is it rounded, and the defects come from \
             the sample nearest the mean; each names the `refs` to re-lay-out. \
             A single read of one unchanged sheet swings 1-3 points, so judge only by the MEAN. \
             A first review of 5.5 or better is the best this sheet will read — finish and report \
             it. Under 5.5 the composition is wrong: fix what the defects name and review again, \
             but a review worse than the previous one means the last edit hurt, so do not arrange \
             again, finish, and report the best mean this sheet reached."
        ),
    });
    result[IMAGE_PATH_KEY] = json!(subject.annotated_path());
    Ok(result)
}

/// The framing that rides with the images. The rubric's own anchor sentence is
/// reused verbatim, so the image order it names — sheet first, reference second —
/// is the order they are attached in, and it is the whole attachment: the harness
/// critic sends exactly these two, and a grader given a different picture returns
/// a different number.
///
/// The engine has already measured this exact geometry, so the answer to the two
/// classes the rubric spends half its length warning about is stated as ground
/// truth rather than left to the grader's eyes.
fn user_prompt(sheet: &SheetPngs) -> String {
    let parts = &sheet.parts;
    let calibration = ANCHOR_CALIBRATION.trim();
    let ground_truth = engine_ground_truth(&sheet.visual);
    format!(
        "Audit this rendered schematic for layout quality. Reason first (trace every \
         wire-through-body and dangling-pin candidate to its endpoints), then emit the \
         FINAL_JSON verdict. Parts on the sheet: {parts}.\n\n{calibration}\n\n{ground_truth}\n\n\
         Add two fields to every defect: \"refs\" — the reference designators involved, \
         which is how the sheet is edited — and \"fix\": the one concrete re-layout that \
         removes it."
    )
}

/// The engine's verdict on the two false-positive-prone classes: either they are
/// impossible on this sheet, or these are exactly the ones that are real.
fn engine_ground_truth(visual: &sch_floorplan::visual::VisualFacts) -> String {
    if visual.wires_through_bodies.is_empty() && visual.dangling_wire_ends.is_empty() {
        return ENGINE_CLEAN.trim().to_string();
    }
    let crossings = visual
        .wires_through_bodies
        .iter()
        .map(|c| c.reference.clone())
        .collect::<Vec<_>>();
    let dangling = visual
        .dangling_wire_ends
        .iter()
        .map(|[x, y]| format!("({x}, {y})"))
        .collect::<Vec<_>>();
    format!(
        "AUTHORITATIVE ENGINE GROUND TRUTH (exact geometric + netlist analysis of the real \
         coordinates): the ONLY wires through a body are on {}; the ONLY bare wire ends are \
         at {} mm. Report no other wire-through-body or dangling-pin defect — any such claim \
         is a confirmed false positive.",
        if crossings.is_empty() {
            "no part".to_string()
        } else {
            crossings.join(", ")
        },
        if dangling.is_empty() {
            "no point".to_string()
        } else {
            dangling.join(", ")
        },
    )
}

/// `(score, verdict)` from one grader response, or `None` if it emitted no JSON.
fn verdict(text: &str) -> Option<(f64, Value)> {
    let value = gordian_llm::verdict_json(text)?;
    let score = value.get("score").and_then(Value::as_f64)?;
    Some((score, value))
}

/// One review's verdict: the rounded mean, the mean itself, every sample, and
/// the narrative of the run nearest the mean.
struct Graded {
    score: f64,
    mean: f64,
    samples: Vec<f64>,
    verdict: Value,
}

/// Score by the mean of the samples and keep one coherent narrative: the run
/// whose own score sits nearest the mean, lowest such run on a tie. Mirrors
/// `tools/schematic_critic.py`.
fn grade(mut runs: Vec<(f64, Value)>) -> Graded {
    runs.sort_by(|a, b| a.0.total_cmp(&b.0));
    let samples: Vec<f64> = runs.iter().map(|(score, _)| *score).collect();
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    let verdict = runs
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
    Graded {
        score: mean.round(),
        mean: (mean * 100.0).round() / 100.0,
        samples,
        verdict,
    }
}

/// The critic's defects in the tool's shape: severity, kind, where, what, fix.
fn defects(verdict: &Value) -> Vec<Value> {
    verdict
        .get("defects")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|d| {
            let text = |key: &str| d.get(key).and_then(Value::as_str).unwrap_or("").to_string();
            json!({
                "severity": text("severity"),
                "kind": text("category"),
                "refs": refs(d),
                "what": text("description"),
                "fix": text("fix"),
                "confidence": text("confidence"),
            })
        })
        .collect()
}

/// The refdes list a defect names, from `refs` when the grader supplied it and
/// from the rubric's own `location` field otherwise.
fn refs(defect: &Value) -> Vec<String> {
    match defect.get("refs") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        Some(Value::String(one)) => vec![one.clone()],
        _ => defect
            .get("location")
            .and_then(Value::as_str)
            .into_iter()
            .map(str::to_string)
            .collect(),
    }
}

/// A score without its trailing `.0`, so `7` reads as `7`.
fn trim_zero(value: f64) -> String {
    let text = format!("{value}");
    text.strip_suffix(".0").unwrap_or(&text).to_string()
}

/// The one-line summary the agent loop shows for a finished review.
pub fn summary(result: &Value) -> String {
    let Some(score) = result.get("score").and_then(Value::as_f64) else {
        return result
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("review unavailable")
            .to_string();
    };
    let samples = result
        .get("samples")
        .and_then(Value::as_array)
        .map(|s| {
            s.iter()
                .filter_map(Value::as_f64)
                .map(trim_zero)
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default();
    let defects = result
        .get("defects")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let mean = result
        .get("mean")
        .and_then(Value::as_f64)
        .unwrap_or(score);
    format!(
        "review mean {mean:.2} (score {}/10) vs the {REFERENCE} (samples {samples}); \
         {defects} defect(s)",
        trim_zero(score)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(score: f64, summary: &str) -> (f64, Value) {
        (score, json!({ "score": score, "summary": summary }))
    }

    #[test]
    fn the_shared_rubric_and_anchor_are_the_critic_script_s() {
        assert!(CRITIC_SYSTEM.contains("wire-through-body"));
        assert!(ANCHOR_CALIBRATION.contains("rated exactly 9/10"));
    }

    #[test]
    fn the_score_is_the_rounded_mean_of_every_sample() {
        let graded = grade(vec![run(4.0, "a"), run(8.0, "b"), run(8.0, "c"), run(5.0, "d")]);
        assert_eq!(graded.samples, vec![4.0, 5.0, 8.0, 8.0]);
        assert_eq!(graded.mean, 6.25);
        assert_eq!(graded.score, 6.0);
    }

    #[test]
    fn the_narrative_comes_from_the_run_nearest_the_mean() {
        let graded = grade(vec![run(4.0, "low"), run(7.0, "middle"), run(10.0, "high")]);
        assert_eq!(graded.mean, 7.0);
        assert_eq!(graded.verdict["summary"], "middle");
    }

    #[test]
    fn a_bimodal_sheet_scores_between_its_modes_not_at_one() {
        let graded = grade(vec![
            run(4.0, "a"),
            run(4.0, "b"),
            run(4.0, "c"),
            run(4.0, "d"),
            run(8.0, "e"),
            run(8.0, "f"),
            run(8.0, "g"),
        ]);
        assert_eq!(graded.mean, 5.71);
        assert_eq!(graded.score, 6.0);
    }

    #[test]
    fn defects_carry_the_refs_and_the_fix() {
        let verdict = json!({"defects": [{
            "severity": "major", "confidence": "high", "category": "spacing",
            "location": "C1/U1", "description": "C1 is stranded", "fix": "move C1 beside U1.VDD",
            "refs": ["C1", "U1"]
        }]});
        let mapped = defects(&verdict);
        assert_eq!(mapped[0]["kind"], "spacing");
        assert_eq!(mapped[0]["refs"], json!(["C1", "U1"]));
        assert_eq!(mapped[0]["what"], "C1 is stranded");
        assert_eq!(mapped[0]["fix"], "move C1 beside U1.VDD");
    }

    #[test]
    fn refs_fall_back_to_the_rubric_s_location() {
        let verdict = json!({"defects": [{"severity": "minor", "location": "R3"}]});
        assert_eq!(defects(&verdict)[0]["refs"], json!(["R3"]));
    }
}
