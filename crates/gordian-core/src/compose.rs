//! The composition polish pass: revise only the layout trees against the
//! critic's defects, score every version, keep the best.
//!
//! Port of `schagent/compose.py`. The netlist is frozen — the composer may only
//! re-arrange what is already there — so a round can improve the drawing and
//! never the circuit. It is budget-aware: a round only starts while the deadline
//! is far enough away for the round to finish.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use gordian_llm::{Binary, ChatMessage, ContentPart, MessageContent, Provider, completed_text};
use serde_json::Value;

use crate::agent::{event, part_summary};
use crate::critic::{self, Review};
use crate::engines::sch;
use crate::render;

const COMPOSER: &str = r#"You are a senior electronics engineer re-composing a KiCad schematic. You get the netlist (parts and pin nets),
the current LAYOUT (each block is a CSS-flexbox-like tree of "row"/"col" containers with "gap"/"align"; leaves are
{"part": id} with optional "rot"/"mirror"; parts of multi-unit symbols use {"part": id, "unit": n}), the current
render with a coordinate grid, and a reviewer's defect list. Output ONLY a JSON object {"layout": [...]} - a complete
new layout for ALL parts (every part exactly once), fixing the defects. Rules: a row is one signal path (consecutive
parts share a net); shunts hang in a col under the part they attach to; ICs sit between a col of input-side parts and a
col of output-side parts; decoupling caps in a row next to the IC; symmetric halves are mirrored cols side by side;
blocks of 3-12 parts; compact gaps (4-6 passives, 6-8 around ICs); balanced blocks (wider than tall, never a long
column); unused units of an IC in a row beside that IC's power unit; titles for blocks. Do not change part ids or nets."#;

/// Layouts asked for per round. They are drawn and graded concurrently, so two
/// candidates cost one candidate's wall time and roughly double the chance that
/// a round beats the score it started from.
const CANDIDATES: usize = 2;

/// What a polish pass produced.
#[derive(Default)]
pub struct Composed {
    /// The winning sheet, when a round beat the score it started from.
    pub sheet: Option<PathBuf>,
    pub design: Option<Value>,
    pub review: Option<Review>,
    pub rounds: usize,
    pub seconds: f64,
}

/// What one polish pass works on.
pub struct Pass<'a> {
    pub lib: &'a sch::Library,
    pub kicad_cli: &'a Path,
    /// Where each round's sheet and renders are written.
    pub out_dir: &'a Path,
    /// The design whose layout trees are re-composed; its netlist is frozen.
    pub design: &'a Value,
    /// The critic's defects on `design`, which the first round works from.
    pub defects: &'a str,
    /// The score to beat; a round below it is discarded.
    pub baseline: f64,
    pub rounds: usize,
    pub deadline: Instant,
}

/// Re-compose the layout trees for at most `rounds` rounds, or until the
/// deadline runs out. Returns the best version that beat the baseline.
pub async fn compose(client: &dyn Provider, pass: Pass<'_>) -> Result<Composed> {
    let Pass {
        lib,
        kicad_cli,
        out_dir,
        design,
        defects,
        baseline,
        rounds,
        deadline,
    } = pass;
    let started = Instant::now();
    std::fs::create_dir_all(out_dir)?;
    let mut current = design.clone();
    let mut best: Option<(f64, PathBuf, Value, Review)> = None;
    let mut critique = defects.to_string();
    let mut done = 0usize;

    for round in 0..rounds {
        if Instant::now() + round_cost(started, done) > deadline {
            event(format!(
                "compose: stopping after {done} round(s) — not enough budget for another"
            ));
            break;
        }
        // The two model calls and every grade of the round run concurrently, so a
        // round explores CANDIDATES layouts for the wall time of one.
        let grid = latest_grid(out_dir, round);
        let layouts = futures::future::join_all(
            (0..CANDIDATES).map(|_| ask(client, &current, &critique, grid.clone())),
        )
        .await;
        done += 1;

        let mut built = Vec::new();
        for (k, layout) in layouts.into_iter().enumerate() {
            let Some(layout) = layout? else { continue };
            let mut candidate = current.clone();
            candidate["layout"] = layout;
            let sheet = out_dir.join(format!("c{round}_{k}.kicad_sch"));
            let report = match sch::build(lib, &candidate, &sheet) {
                Ok(report) => report,
                Err(error) => {
                    event(format!("compose {round}.{k}: build failed: {error:#}"));
                    continue;
                }
            };
            if !report.issues.is_empty() {
                event(format!(
                    "compose {round}.{k}: {} issue(s), discarded",
                    report.issues.len()
                ));
                continue;
            }
            let clean = sheet.with_extension("png");
            let grid = out_dir.join(format!("c{round}_{k}_grid.png"));
            let rendered = render::sheet(kicad_cli, &sheet, &clean, &grid)?;
            let summary = part_summary(&report.raw);
            built.push((k, sheet, candidate, rendered, summary, report.warnings.is_empty()));
        }
        if built.is_empty() {
            break;
        }
        let graded = futures::future::join_all(
            built
                .iter()
                .map(|(_, _, _, rendered, summary, clean)| {
                    critic::review(client, &rendered.clean, summary, *clean)
                }),
        )
        .await;

        let mut round_best: Option<(f64, Value, Review)> = None;
        for ((k, sheet, candidate, ..), review) in built.into_iter().zip(graded) {
            let Some(review) = review? else { continue };
            event(format!("compose {round}.{k}: {}", review.event()));
            let beaten = best.as_ref().map_or(baseline, |(score, _, _, _)| *score);
            if review.mean > beaten {
                best = Some((review.mean, sheet, candidate.clone(), review.clone()));
            }
            if round_best.as_ref().is_none_or(|(s, ..)| review.mean > *s) {
                round_best = Some((review.mean, candidate, review));
            }
        }
        // The next round works from the round's best layout and its defects.
        if let Some((_, candidate, review)) = round_best {
            current = candidate;
            critique = critic::defect_lines(&review);
        }
    }

    let (sheet, design, review) = match best {
        Some((_, sheet, design, review)) => (Some(sheet), Some(design), Some(review)),
        None => (None, None, None),
    };
    Ok(Composed {
        sheet,
        design,
        review,
        rounds: done,
        seconds: started.elapsed().as_secs_f64(),
    })
}

/// What one more round is expected to cost: the mean of the rounds already run,
/// and a conservative guess before the first one has finished.
fn round_cost(started: Instant, done: usize) -> Duration {
    if done == 0 {
        // A round is two model calls plus a seven-read grade of each candidate;
        // measured at 70-75 s, and a round that cannot finish is wasted entirely.
        Duration::from_secs(75)
    } else {
        started.elapsed() / done as u32
    }
}

/// The render the composer is shown: the newest grid image a round wrote.
fn latest_grid(out_dir: &Path, round: usize) -> Option<PathBuf> {
    (0..round)
        .rev()
        .flat_map(|r| (0..CANDIDATES).map(move |k| (r, k)))
        .map(|(r, k)| out_dir.join(format!("c{r}_{k}_grid.png")))
        .find(|p| p.is_file())
}

/// Ask the composer for a whole new `layout`, showing it the last render.
async fn ask(
    client: &dyn Provider,
    design: &Value,
    critique: &str,
    grid: Option<PathBuf>,
) -> Result<Option<Value>> {
    let netlist = serde_json::to_string(design.get("parts").unwrap_or(&Value::Null))?;
    let layout = serde_json::to_string(design.get("layout").unwrap_or(&Value::Null))?;
    let mut parts = vec![
        ContentPart::from_text(format!("NETLIST:\n{netlist}")),
        ContentPart::from_text(format!("CURRENT LAYOUT:\n{layout}")),
        ContentPart::from_text(format!("REVIEW:\n{critique}")),
    ];
    if let Some(path) = grid
        && let Ok(png) = std::fs::read(&path)
    {
        parts.push(ContentPart::from_text(
            "Current render (grid units on the axes):".to_string(),
        ));
        parts.push(ContentPart::Binary(binary(&png)));
    }
    parts.push(ContentPart::from_text(
        "Respond with the JSON layout only.".to_string(),
    ));
    let messages = [ChatMessage::user(MessageContent::from_parts(parts))];
    let end = client.complete(COMPOSER, &messages, &[]).await?;
    Ok(gordian_llm::verdict_json(&completed_text(&end))
        .and_then(|value| value.get("layout").cloned())
        .filter(Value::is_array))
}

fn binary(png: &[u8]) -> Binary {
    use base64::Engine;
    Binary::from_base64(
        "image/png",
        base64::engine::general_purpose::STANDARD.encode(png),
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_round_is_costed_conservatively_and_then_by_measurement() {
        let started = Instant::now() - Duration::from_secs(60);
        assert_eq!(round_cost(started, 0), Duration::from_secs(75));
        assert!(round_cost(started, 2) >= Duration::from_secs(29));
    }
}
