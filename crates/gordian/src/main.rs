//! The `gordian` CLI.
//!
//! `gordian agent --project <dir> "<prompt>"` designs a schematic, polishes its
//! layout, builds the board and leaves `report.json` in the project directory.
//! When the project already holds the schematic, the run is an edit: the model
//! returns a patch against it instead of a whole design.
//!
//! `gordian tui --project <dir>` is the same run, driven interactively: the
//! prompt is typed into a composer, the run streams into a live transcript and
//! its renders appear inline.

mod config;
mod tui;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use gordian_core::agent::Budget;
use gordian_core::run::{RunOptions, run};
use gordian_runtime::logging;

/// Project directory when `--project` is omitted.
const DEFAULT_PROJECT_DIR: &str = "gordian-project";

const USAGE: &str = "usage:
  gordian                              print version
  gordian agent [--project <dir>] [\"<prompt>\"]
                [--budget <seconds>] [--max-builds <n>] [--no-review] [--no-pcb]
                                       design a schematic (and its board) into <dir>
  gordian tui [--project <dir>]        design interactively in the terminal

options:
  --project, -p <dir>                  project directory (default: gordian-project)
  --budget <seconds>                   wall clock for the whole run
  --max-builds <n>                     ceiling on `build` calls
  --no-review                          skip the composition polish pass
  --no-pcb                             stop after the schematic
  -h, --help                           print help
  -V, --version                        print version";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("--version" | "-V" | "version") => {
            println!("gordian {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("--help" | "-h" | "help") => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some("agent") if matches!(&args[1..], [one] if one == "--help" || one == "-h") => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some("tui") if matches!(&args[1..], [one] if one == "--help" || one == "-h") => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some("tui") => match run_tui(&args[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                logging::init_stderr_only();
                tracing::error!("error: {error:#}");
                ExitCode::FAILURE
            }
        },
        Some("agent") => match run_agent(&args[1..]) {
            Ok(code) => code,
            Err(error) => {
                logging::init_stderr_only();
                tracing::error!("error: {error:#}");
                ExitCode::FAILURE
            }
        },
        Some(other) => {
            eprintln!("unknown command `{other}`\n{USAGE}");
            ExitCode::FAILURE
        }
    }
}

/// What one `agent` invocation asked for. The two bounds stay `None` unless the
/// command line set them, so the config's own values remain in charge.
struct Invocation {
    project_dir: PathBuf,
    prompt: String,
    budget_seconds: Option<u64>,
    max_builds: Option<usize>,
    polish: bool,
    board: bool,
}

fn parse(args: &[String]) -> Result<Invocation> {
    let mut project_dir: Option<PathBuf> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut budget_seconds = None;
    let mut max_builds = None;
    let mut polish = true;
    let mut board = true;
    let mut options = true;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        let value = |name: &str| -> Result<String> {
            args.get(index + 1)
                .cloned()
                .with_context(|| format!("{name} requires a value"))
        };
        match arg {
            "--" if options => options = false,
            "--project" | "-p" if options => {
                project_dir = Some(PathBuf::from(value("--project")?));
                index += 1;
            }
            "--budget" if options => {
                budget_seconds = Some(value("--budget")?.parse()?);
                index += 1;
            }
            "--max-builds" if options => {
                max_builds = Some(value("--max-builds")?.parse()?);
                index += 1;
            }
            "--no-review" if options => polish = false,
            "--no-pcb" if options => board = false,
            // Accepted and ignored: the quality harness passes it, and the loop
            // has no per-turn request cap of its own — the wall clock bounds it.
            "--max-requests" if options => index += 1,
            other if options && other.starts_with('-') => bail!("unknown agent option `{other}`"),
            other => positional.push(other.to_string()),
        }
        index += 1;
    }
    let (project_dir, prompt) = match (project_dir, positional.len()) {
        (Some(dir), 1) => (dir, positional.remove(0)),
        (Some(_), _) => bail!("expected exactly one prompt"),
        (None, 1) => (PathBuf::from(DEFAULT_PROJECT_DIR), positional.remove(0)),
        (None, 2) => (PathBuf::from(positional.remove(0)), positional.remove(0)),
        (None, _) => bail!("missing prompt"),
    };
    let board = board && wants_board(&prompt);
    Ok(Invocation {
        project_dir,
        prompt,
        budget_seconds,
        max_builds,
        polish,
        board,
    })
}

/// The project directory `tui` works in: `--project <dir>`, a bare path, or the
/// current directory.
fn parse_tui(args: &[String]) -> Result<PathBuf> {
    let mut dir: Option<PathBuf> = None;
    let mut options = true;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--" if options => options = false,
            "--project" | "-p" if options => {
                dir = Some(PathBuf::from(
                    args.get(index + 1).context("--project requires a value")?,
                ));
                index += 1;
            }
            other if options && other.starts_with('-') => bail!("unknown tui option `{other}`"),
            other if dir.is_none() => dir = Some(PathBuf::from(other)),
            other => bail!("unexpected argument `{other}`"),
        }
        index += 1;
    }
    dir.map_or_else(|| std::env::current_dir().context("resolving the working directory"), Ok)
}

/// Launch the cockpit. It runs on its own multi-threaded runtime so a design
/// run's blocking KiCad calls never stall the redraw loop.
fn run_tui(args: &[String]) -> Result<()> {
    let project_dir = parse_tui(args)?;
    let loaded = config::load_or_create()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the Tokio runtime")?;
    runtime.block_on(tui::run(project_dir, loaded))
}

/// A request that asks only for the schematic ("render the schematic") skips the board;
/// any mention of the board, layout, routing or fabrication — or no mention of the
/// schematic at all — gets both.
pub(crate) fn wants_board(prompt: &str) -> bool {
    let lower = prompt.to_ascii_lowercase();
    let board_words = ["pcb", "board", "layout", "rout", "fabricat", "gerber"];
    if board_words.iter().any(|w| lower.contains(w)) {
        return true;
    }
    !lower.contains("schematic")
}

fn run_agent(args: &[String]) -> Result<ExitCode> {
    let Invocation {
        project_dir,
        prompt,
        budget_seconds,
        max_builds,
        polish,
        board,
    } = parse(args)?;
    std::fs::create_dir_all(&project_dir)
        .with_context(|| format!("creating {}", project_dir.display()))?;

    let loaded = config::load_or_create()?;
    let kicad = config::detect_kicad(&loaded.config).context(
        "KiCad 10 is required; set kicad.cliPath, kicad.symbolDir and kicad.footprintDir in config.toml",
    )?;
    let mut client =
        gordian_core::GenaiProvider::from_config(&loaded.config.llm).with_context(|| {
            format!(
                "could not build the LLM client — set llm.adapter, llm.model and llm.apiKey in {}",
                loaded.path.display()
            )
        })?;
    if let Ok(thread) = std::env::var("GORDIAN_THREAD_ID") {
        client = client.with_thread_identifier(thread);
    }
    let _guard = logging::init(&project_dir, client.thread_identifier());

    let budget = Budget {
        total: Duration::from_secs(budget_seconds.unwrap_or(loaded.config.agent.budget_seconds)),
        max_builds: max_builds.unwrap_or(loaded.config.agent.max_builds),
        ..Budget::default()
    };
    tracing::info!(
        "kicad: {} ({})",
        kicad.version(),
        kicad.cli_path().display()
    );
    tracing::info!("config: {}", loaded.path.display());
    tracing::info!("project: {}", project_dir.display());
    tracing::info!(
        "budget: {}s, at most {} builds",
        budget.total.as_secs(),
        budget.max_builds
    );

    let options = RunOptions {
        project_dir,
        schematic_filename: loaded.config.project.schematic_filename.clone(),
        prompt: prompt.clone(),
        budget,
        polish,
        board,
        compose_rounds: 2,
    };
    tracing::info!(target: logging::EVENTS_TARGET, "--- agent events ---");
    tracing::info!(target: logging::EVENTS_TARGET, "turn 1: {prompt}");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the Tokio runtime")?;
    let report = runtime.block_on(run(&client, &kicad, options));
    runtime.shutdown_timeout(Duration::from_secs(1));
    let report = report?;

    let usage = &report.value["usage"];
    tracing::info!(
        target: logging::EVENTS_TARGET,
        "usage: provider_requests={}",
        usage["provider_requests"]
    );
    tracing::info!(
        target: logging::EVENTS_TARGET,
        "turn 1 done: requests={} elapsed={}s",
        usage["provider_requests"],
        report.value["timings"]["total_s"]
    );
    tracing::info!(
        "provider requests: {} (all model invocations, including review calls)",
        usage["provider_requests"]
    );
    tracing::info!("timings: {}", report.value["timings"]);
    tracing::info!("schematic review: {}", report.value["schematic"]["review"]);
    tracing::info!("pcb: {}", report.value["pcb"]);

    let Some(sch) = report.schematic else {
        bail!("the agent did not produce a schematic");
    };
    tracing::info!("schematic: {}", sch.display());
    if report.erc_errors > 0 {
        tracing::warn!("ERC reported {} error(s)", report.erc_errors);
        return Ok(ExitCode::FAILURE);
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    #[test]
    fn schematic_only_requests_skip_the_board() {
        assert!(!super::wants_board("Design a 555 blinker. Render the schematic."));
        assert!(super::wants_board("Design a Blue Pill schematic and PCB, route it"));
        assert!(super::wants_board("make a stm32 bluepill"));
    }

    use super::*;

    #[test]
    fn the_harness_invocation_parses() {
        let args = ["--project", "/tmp/p", "--no-review", "design a board"]
            .map(String::from)
            .to_vec();
        let inv = parse(&args).unwrap();
        assert_eq!(inv.project_dir, PathBuf::from("/tmp/p"));
        assert_eq!(inv.prompt, "design a board");
        assert!(!inv.polish);
        assert!(inv.board);
    }

    /// Unset bounds stay `None`, so the config's values are what run.
    #[test]
    fn the_wall_clock_and_build_ceiling_are_optional_overrides() {
        let bare = parse(&["p".to_string()]).unwrap();
        assert_eq!(bare.budget_seconds, None);
        assert_eq!(bare.max_builds, None);

        let args = ["--budget", "90", "--max-builds", "3", "p"]
            .map(String::from)
            .to_vec();
        let inv = parse(&args).unwrap();
        assert_eq!(inv.budget_seconds, Some(90));
        assert_eq!(inv.max_builds, Some(3));
    }

    #[test]
    fn a_bare_prompt_uses_the_default_project() {
        assert_eq!(
            parse(&["design a board".to_string()]).unwrap().project_dir,
            PathBuf::from(DEFAULT_PROJECT_DIR)
        );
    }

    #[test]
    fn the_cockpit_takes_its_project_from_a_flag_or_a_bare_path() {
        assert_eq!(
            parse_tui(&["--project".into(), "/tmp/p".into()]).unwrap(),
            PathBuf::from("/tmp/p")
        );
        assert_eq!(parse_tui(&["/tmp/p".into()]).unwrap(), PathBuf::from("/tmp/p"));
        assert!(parse_tui(&["/a".into(), "/b".into()]).is_err());
        assert!(parse_tui(&["--wat".into()]).is_err());
    }

    #[test]
    fn an_unknown_option_is_rejected() {
        assert!(parse(&["--wat".to_string(), "p".to_string()]).is_err());
        assert!(parse(&[]).is_err());
    }
}
