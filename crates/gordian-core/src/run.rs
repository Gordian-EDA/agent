//! One end-to-end run: schematic loop, then the board, with the composition
//! polish pass overlapped on the board build and both bounded by one wall clock.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{Context, Result};
use gordian_llm::Provider;
use kicad::KicadInstallation;
use serde_json::{Value, json};

use crate::agent::{Agent, Budget, event};
use crate::board::{self, BoardOutcome};
use crate::critic;
use crate::{compose, render, skills};

/// What one `gordian agent` invocation was asked for.
pub struct RunOptions {
    pub project_dir: PathBuf,
    pub schematic_filename: String,
    pub prompt: String,
    pub budget: Budget,
    /// Run the composition polish pass after the loop (`--no-review` clears it).
    pub polish: bool,
    /// Build the board after the schematic is accepted.
    pub board: bool,
    /// Composition rounds when there is budget for them.
    pub compose_rounds: usize,
}

/// The delivered run, mirrored into `report.json` beside the design.
pub struct Report {
    pub value: Value,
    pub schematic: Option<PathBuf>,
    pub erc_errors: usize,
}

/// Design, polish, build the board, and write `report.json` into the project.
pub async fn run(
    client: &dyn Provider,
    kicad: &KicadInstallation,
    options: RunOptions,
) -> Result<Report> {
    let started = Instant::now();
    let deadline = started + options.budget.total;
    let mut agent = Agent::new(
        client,
        kicad.symbol_dir(),
        kicad.cli_path(),
        &options.project_dir,
        &options.schematic_filename,
        options.budget.clone(),
    )?;
    agent.open_existing()?;
    let skill_block = skills::prompt_block(&options.prompt);
    if !skill_block.is_empty() {
        event("skills: a matching starter design was attached to the prompt");
    }

    // The board is deterministic and needs only the netlist, so it starts on the
    // first clean build and routes while the model is still running ERC, the
    // review and the polish pass on the same circuit.
    let early = EarlyBoard {
        enabled: options.board,
        kicad: kicad.clone(),
        work_dir: agent.work_dir().to_path_buf(),
        deadline,
        started: Mutex::new(None),
    };
    let hook = |sheet: &Path| early.start(sheet);

    let loop_started = Instant::now();
    agent.on_first_clean_build(&hook);
    let outcome = agent.run(&options.prompt, &skill_block).await?;
    let loop_seconds = loop_started.elapsed().as_secs_f64();
    event(format!(
        "schematic: {} build(s) in {loop_seconds:.0}s, review {}",
        outcome.builds,
        outcome
            .review
            .as_ref()
            .map_or("none".to_string(), |r| format!("{:.2}", r.mean))
    ));

    let board_task = early.claim(outcome.source.as_deref());
    let board_stem = agent.out_sch().with_extension("");

    let mut schematic = outcome.sheet.clone();
    let mut design = outcome.design.clone();
    let mut review = outcome.review;
    let mut compose_seconds = 0.0;
    if options.polish
        && !agent.edit_mode()
        && let Some(current) = design.clone()
        && current.get("layout").is_some_and(Value::is_array)
    {
        let baseline = agent.best_score().unwrap_or(0.0);
        let defects = review
            .as_ref()
            .map(critic::defect_lines)
            .unwrap_or_default();
        let out_dir = agent.work_dir().join("compose");
        // The pass budgets its own rounds, but a single round can overrun its
        // estimate; the wall clock is the promise, so it is also enforced here.
        let left = deadline.saturating_duration_since(Instant::now());
        let pass = compose::compose(
            client,
            compose::Pass {
                lib: agent.library(),
                kicad_cli: kicad.cli_path(),
                out_dir: &out_dir,
                design: &current,
                defects: &defects,
                baseline,
                rounds: options.compose_rounds,
                deadline,
            },
        );
        let composed = match tokio::time::timeout(left, pass).await {
            Ok(composed) => composed?,
            Err(_) => {
                event("compose: abandoned at the wall clock");
                compose::Composed::default()
            }
        };
        compose_seconds = composed.seconds;
        if let Some(better) = composed.sheet {
            std::fs::copy(&better, agent.out_sch())
                .with_context(|| format!("delivering {}", agent.out_sch().display()))?;
            schematic = Some(agent.out_sch().to_path_buf());
            design = composed.design;
            review = composed.review;
            event("compose: the polished layout was delivered");
        }
    }

    let mut board_outcome = match board_task {
        Some(task) => task.await.unwrap_or_default(),
        None => BoardOutcome::default(),
    };
    publish_board(&mut board_outcome, &board_stem, &options.project_dir);

    let renders = final_renders(kicad, agent.out_sch(), &options.project_dir);
    if let Some(design) = design.as_ref() {
        let _ = std::fs::write(
            options.project_dir.join("design.json"),
            serde_json::to_string_pretty(design)?,
        );
    }

    let erc_errors = outcome
        .erc
        .iter()
        .filter(|line| line.starts_with("[error]"))
        .count();
    let value = json!({
        "prompt": options.prompt,
        "schematic": {
            "path": schematic.as_ref().map(|p| p.display().to_string()),
            "renders": renders,
            "builds": outcome.builds,
            "issues": outcome.issues,
            "erc_errors": erc_errors,
            "erc": outcome.erc,
            "review": review.as_ref().map(critic::as_json),
        },
        "pcb": board_outcome.as_json(),
        "timings": {
            "total_s": round1(started.elapsed().as_secs_f64()),
            "schematic_loop_s": round1(loop_seconds),
            "compose_s": round1(compose_seconds),
            "pcb_s": round1(board_outcome.seconds),
            "budget_s": options.budget.total.as_secs(),
        },
        "usage": {
            "provider_requests": outcome.usage.requests,
            "input_tokens": outcome.usage.input,
            "output_tokens": outcome.usage.output,
            "cache_write_tokens": outcome.usage.cache_write,
            "cache_read_tokens": outcome.usage.cache_read,
        },
        "summary": outcome.summary,
    });
    let report_path = options.project_dir.join("report.json");
    std::fs::write(&report_path, serde_json::to_string_pretty(&value)? + "\n")
        .with_context(|| format!("writing {}", report_path.display()))?;
    event(format!("report: {}", report_path.display()));

    Ok(Report {
        value,
        schematic,
        erc_errors,
    })
}

/// The two PNGs delivered beside the design: the plain sheet and the gridded one.
fn final_renders(
    kicad: &KicadInstallation,
    sch: &std::path::Path,
    project_dir: &std::path::Path,
) -> Value {
    if !sch.is_file() {
        return Value::Null;
    }
    let clean = project_dir.join("schematic.png");
    let grid = project_dir.join("schematic-grid.png");
    match render::sheet(kicad.cli_path(), sch, &clean, &grid) {
        Ok(_) => json!({"clean": clean.display().to_string(), "grid": grid.display().to_string()}),
        Err(error) => json!({"error": format!("{error:#}")}),
    }
}

fn round1(seconds: f64) -> f64 {
    (seconds * 10.0).round() / 10.0
}

/// Move a finished board out of its attempt directory and next to the design.
///
/// Each attempt routes into its own directory, so an abandoned run that is still
/// writing cannot corrupt the one being delivered.
fn publish_board(outcome: &mut BoardOutcome, stem: &Path, project_dir: &Path) {
    if let Some(pcb) = outcome.pcb.take() {
        let delivered = stem.with_extension("kicad_pcb");
        if std::fs::copy(&pcb, &delivered).is_ok() {
            let project = pcb.with_extension("kicad_pro");
            let _ = std::fs::copy(&project, stem.with_extension("kicad_pro"));
            outcome.pcb = Some(delivered);
        }
    }
    if let Some(png) = outcome.front_png.take() {
        let delivered = project_dir.join("board-front.png");
        if std::fs::copy(&png, &delivered).is_ok() {
            outcome.front_png = Some(delivered);
        }
    }
}

/// The board stage, started from whichever build first came out clean.
///
/// A later build, or the polish pass, may change the drawing; only a change of
/// the *source build* changes the netlist, so that is the one condition under
/// which the early board is thrown away and rebuilt from what was delivered.
struct EarlyBoard {
    enabled: bool,
    kicad: KicadInstallation,
    work_dir: PathBuf,
    deadline: Instant,
    started: Mutex<Option<(PathBuf, tokio::task::JoinHandle<BoardOutcome>)>>,
}

impl EarlyBoard {
    /// Start routing `sheet`. Called once, from the design loop.
    fn start(&self, sheet: &Path) {
        if !self.enabled {
            return;
        }
        if let Some(task) = self.attempt(sheet, "board-early") {
            *self.started.lock().expect("board task lock") = Some((sheet.to_path_buf(), task));
        }
    }

    /// The task to await: the running one when `delivered` is the build it was
    /// started from, otherwise a fresh one over the delivered netlist.
    fn claim(&self, delivered: Option<&Path>) -> Option<tokio::task::JoinHandle<BoardOutcome>> {
        let running = self.started.lock().expect("board task lock").take();
        match (running, delivered) {
            (Some((source, task)), Some(delivered)) if source == delivered => Some(task),
            (running, delivered) => {
                if let Some((_, task)) = running {
                    // `spawn_blocking` cannot be interrupted, so the abandoned
                    // run keeps writing — into its own directory, which nothing
                    // downstream reads.
                    event("board: the delivered sheet is not the one being routed, restarting");
                    task.abort();
                }
                self.attempt(delivered?, "board-final")
            }
        }
    }

    /// Route one snapshot of `sheet` inside its own directory under the work dir.
    fn attempt(&self, sheet: &Path, name: &str) -> Option<tokio::task::JoinHandle<BoardOutcome>> {
        let dir = self.work_dir.join(name);
        let snapshot = dir.join("design.kicad_sch");
        if let Err(error) = std::fs::create_dir_all(&dir).and_then(|()| {
            std::fs::copy(sheet, &snapshot)?;
            Ok(())
        }) {
            event(format!("board: could not stage the sheet: {error}"));
            return None;
        }
        let job = board::Job {
            sch: snapshot,
            pcb: dir.join("design.kicad_pcb"),
            render_png: dir.join("board-front.png"),
            timeout_s: self
                .deadline
                .saturating_duration_since(Instant::now())
                .as_secs()
                .max(20),
        };
        let kicad = self.kicad.clone();
        Some(tokio::task::spawn_blocking(move || {
            board::build(&kicad, &job)
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A finished board is copied out of its attempt directory next to the
    /// design — the `.kicad_pro` with it — and the outcome names where it landed.
    #[test]
    fn publishing_moves_the_board_beside_the_design() {
        let dir = tempfile::tempdir().unwrap();
        let attempt = dir.path().join("board-early");
        std::fs::create_dir_all(&attempt).unwrap();
        std::fs::write(attempt.join("design.kicad_pcb"), "(kicad_pcb)").unwrap();
        std::fs::write(attempt.join("design.kicad_pro"), "{}").unwrap();
        std::fs::write(attempt.join("board-front.png"), b"png").unwrap();
        let mut outcome = BoardOutcome {
            pcb: Some(attempt.join("design.kicad_pcb")),
            front_png: Some(attempt.join("board-front.png")),
            ..Default::default()
        };

        publish_board(&mut outcome, &dir.path().join("design"), dir.path());

        assert_eq!(outcome.pcb, Some(dir.path().join("design.kicad_pcb")));
        assert_eq!(outcome.front_png, Some(dir.path().join("board-front.png")));
        assert!(dir.path().join("design.kicad_pro").is_file());
    }

    /// A board stage that produced nothing leaves nothing behind and says so.
    #[test]
    fn publishing_a_board_that_was_never_built_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let mut outcome = BoardOutcome {
            pcb: Some(dir.path().join("missing/design.kicad_pcb")),
            ..Default::default()
        };

        publish_board(&mut outcome, &dir.path().join("design"), dir.path());

        assert_eq!(outcome.pcb, None);
        assert!(!dir.path().join("design.kicad_pcb").exists());
    }
}
