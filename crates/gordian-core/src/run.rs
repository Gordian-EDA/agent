//! One end-to-end run: schematic loop, then the board, with the composition
//! polish pass overlapped on the board build and both bounded by one wall clock.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use gordian_llm::Provider;
use kicad::KicadInstallation;
use serde_json::{Value, json};

use crate::agent::{Agent, Budget, event};
use crate::board::{self, BoardOutcome};
use crate::critic;
use crate::engines::pcb;
use crate::{compose, inputs, render, skills};

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
    let skill_block =
        skills::prompt_block(&options.prompt) + &inputs::prompt_block(&options.project_dir);
    if !skill_block.is_empty() {
        event("skills: a matching starter design was attached to the prompt");
    }

    // The board is deterministic and needs only the netlist, so it starts on each
    // clean build and routes while the model is still running ERC, the review and
    // the polish pass on the same circuit.
    let early = EarlyBoard {
        enabled: options.board,
        kicad: kicad.clone(),
        work_dir: agent.work_dir().to_path_buf(),
        deadline,
        started: Mutex::new(None),
        attempts: Mutex::new(0),
    };
    let hook = |sheet: &Path| early.start(sheet);

    let loop_started = Instant::now();
    agent.on_clean_build(&hook);
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
        if skip_compose(review.as_ref()) {
            event("compose skipped: review already 9");
        } else {
            let baseline = agent.best_score().unwrap_or(0.0);
            let defects = review
                .as_ref()
                .map(critic::defect_lines)
                .unwrap_or_default();
            let out_dir = agent.work_dir().join("compose");
            let remaining = deadline.saturating_duration_since(Instant::now());
            let compose_deadline = Instant::now() + compose_cap(remaining);
            let composed = compose::compose(
                client,
                compose::Pass {
                    lib: agent.library(),
                    kicad_cli: kicad.cli_path(),
                    out_dir: &out_dir,
                    design: &current,
                    defects: &defects,
                    baseline,
                    rounds: options.compose_rounds,
                    deadline: compose_deadline,
                },
            )
            .await?;
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
    }

    let mut board_outcome = match board_task {
        Some(task) => task.await.unwrap_or_default(),
        None => BoardOutcome::default(),
    };
    publish_board(&mut board_outcome, &board_stem, &options.project_dir);
    publish_fab(
        kicad,
        &mut board_outcome,
        schematic.as_deref(),
        &options.project_dir,
        deadline,
    );

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

/// A review already this close to the critic's ceiling has nothing worth
/// polishing; the compose pass is skipped outright rather than spend the wall
/// clock chasing a fraction of a point.
const COMPOSE_SKIP_REVIEW: f64 = 8.9;

/// The compose pass's own wall-clock cap: never worth more than a minute, and
/// never so much that it eats into the reserve the PCB stage needs to finish
/// publishing after both stages join.
const COMPOSE_MAX: Duration = Duration::from_secs(60);

/// Whether the accepted build is already good enough that polishing it is not
/// worth the wall clock.
fn skip_compose(review: Option<&critic::Review>) -> bool {
    review.is_some_and(|r| r.mean >= COMPOSE_SKIP_REVIEW)
}

/// The compose pass's wall-clock budget: at most [`COMPOSE_MAX`], and never
/// more than what is left once the PCB stage's own delivery reserve is set
/// aside, so a long compose pass cannot itself blow the run's deadline.
fn compose_cap(remaining: Duration) -> Duration {
    COMPOSE_MAX.min(remaining.saturating_sub(DELIVERY_RESERVE))
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

/// Gerbers, drill, placement and BOM take a few seconds; below this much runway
/// left in the budget, fab export is skipped rather than risk overrunning delivery.
const FAB_MIN_REMAINING: Duration = Duration::from_secs(8);

/// Export the fabrication bundle for a published board into `<project>/fab/`, plus
/// a back-side render beside the front one. A no-op when there is no board, or too
/// little budget left to spend a few more seconds on it.
fn publish_fab(
    kicad: &KicadInstallation,
    outcome: &mut BoardOutcome,
    schematic: Option<&Path>,
    project_dir: &Path,
    deadline: Instant,
) {
    let Some(pcb) = outcome.pcb.as_deref().filter(|p| p.is_file()) else {
        return;
    };
    if deadline.saturating_duration_since(Instant::now()) < FAB_MIN_REMAINING {
        outcome
            .notes
            .push("fab: skipped, too little budget remaining".to_string());
        return;
    }
    match pcb::export_fab(kicad, pcb, schematic, &project_dir.join("fab")) {
        Ok(files) => outcome.fab_files = files,
        Err(error) => {
            let note = format!("fab: export failed: {error:#}");
            event(format!("board: {note}"));
            outcome.notes.push(note);
        }
    }
    let back_png = project_dir.join("board-back.png");
    if pcb::render(kicad, pcb, &back_png, pcb::Side::Back).is_ok() {
        outcome.back_png = Some(back_png);
    }
}

/// What the run keeps back from the router so the finished board, the renders and
/// `report.json` all land inside the promised wall clock.
const DELIVERY_RESERVE: std::time::Duration = std::time::Duration::from_secs(10);

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
    attempts: Mutex<usize>,
}

impl EarlyBoard {
    /// Start routing `sheet`. Called for every clean build: a later build usually
    /// re-draws the same netlist, but when it does not, the board has to start
    /// over — and starting over here, while the model is still reviewing, is what
    /// keeps the router off the last seconds of the run.
    fn start(&self, sheet: &Path) {
        if !self.enabled {
            return;
        }
        let mut slot = self.started.lock().expect("board task lock");
        // A rebuild is usually a re-drawing, not a new board: an aborted router
        // leaves its solver running, so it is restarted only when the netlist or
        // the footprints actually moved.
        if let Some((source, _)) = slot.as_mut()
            && self.same_board(source, sheet)
        {
            *source = sheet.to_path_buf();
            return;
        }
        let mut attempts = self.attempts.lock().expect("board attempt lock");
        *attempts += 1;
        let name = format!("board-{attempts}");
        if let Some(task) = self.attempt(sheet, &name) {
            if let Some((_, previous)) = slot.take() {
                event("board: a newer clean build changed the netlist, restarting the router on it");
                previous.abort();
            }
            *slot = Some((sheet.to_path_buf(), task));
        }
    }

    /// The task to await: the running one when it is routing the netlist that was
    /// delivered, otherwise a fresh one over the delivered sheet.
    ///
    /// The delivered sheet is often a LATER build than the one being routed — the
    /// model re-balanced a block, or the polish pass re-composed it — and a change
    /// of drawing is not a change of board. So the two sheets are compared by
    /// [`board::identity`], not by path: a routed board is thrown away only when
    /// the nets or the footprints actually moved.
    fn claim(&self, delivered: Option<&Path>) -> Option<tokio::task::JoinHandle<BoardOutcome>> {
        let running = self.started.lock().expect("board task lock").take();
        match (running, delivered) {
            (Some((source, task)), Some(delivered))
                if source == delivered || self.same_board(&source, delivered) =>
            {
                Some(task)
            }
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

    /// Whether two sheets route to the same board.
    fn same_board(&self, source: &Path, delivered: &Path) -> bool {
        let same = match (
            board::identity(&self.kicad, source),
            board::identity(&self.kicad, delivered),
        ) {
            (Some(source), Some(delivered)) => source == delivered,
            _ => false,
        };
        if same {
            event("board: that sheet re-draws the same netlist, keeping the routed board");
        }
        same
    }

    /// Route one snapshot of `sheet` inside its own directory under the work dir.
    fn attempt(&self, sheet: &Path, name: &str) -> Option<tokio::task::JoinHandle<BoardOutcome>> {
        if !self.enabled {
            return None;
        }
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
            // The last seconds of the budget belong to delivery: publishing the
            // board, re-rendering the sheet and writing the report.
            timeout_s: self
                .deadline
                .saturating_duration_since(Instant::now())
                .saturating_sub(DELIVERY_RESERVE)
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

    fn review(mean: f64) -> critic::Review {
        critic::Review {
            score: mean,
            mean,
            samples: vec![mean],
            verdict: Value::Null,
        }
    }

    /// A build already at or past the skip threshold has nothing worth
    /// polishing.
    #[test]
    fn compose_is_skipped_once_the_review_is_already_near_perfect() {
        assert!(skip_compose(Some(&review(8.9))));
        assert!(skip_compose(Some(&review(9.0))));
    }

    /// Below the threshold, or with no review at all, the pass still runs.
    #[test]
    fn compose_runs_below_the_threshold_or_with_no_review() {
        assert!(!skip_compose(Some(&review(8.89))));
        assert!(!skip_compose(Some(&review(6.0))));
        assert!(!skip_compose(None));
    }

    /// A generous remaining budget still caps the pass at a minute.
    #[test]
    fn compose_cap_never_exceeds_a_minute() {
        assert_eq!(compose_cap(Duration::from_secs(600)), COMPOSE_MAX);
    }

    /// A tight remaining budget shrinks the cap by the PCB delivery reserve,
    /// rather than let the pass eat into it.
    #[test]
    fn compose_cap_yields_to_the_delivery_reserve() {
        let remaining = Duration::from_secs(40);
        assert_eq!(compose_cap(remaining), remaining - DELIVERY_RESERVE);
    }

    /// Once there is less budget left than the delivery reserve needs, the
    /// cap collapses to zero rather than go negative — the pass is not
    /// started at all.
    #[test]
    fn compose_cap_collapses_to_zero_when_the_reserve_does_not_fit() {
        assert_eq!(
            compose_cap(Duration::from_secs(5)),
            Duration::ZERO
        );
    }

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
