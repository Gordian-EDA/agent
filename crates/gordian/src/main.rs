//! The `gordian` CLI.
//!
//! - `gordian` (no args) prints the version.
//! - `gordian agent [--project <dir>] [--input <file>] ["<prompt>"]` runs one
//!   or more headless agent turns against real Bedrock + real KiCAD and prints
//!   the live transcript, turn outcomes, token totals, and final ERC result.
//! - `gordian tui [--project <dir>]` launches the ratatui copilot cockpit
//!   (spec §11): a chat transcript and input line, driving the same agent
//!   interactively.

mod config;
mod tui;

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use gordian_core::AgentRuntime;
use gordian_core::prompts::system_prompt;
use gordian_core::{Agent, AgentEvent, StopReason};
use gordian_runtime::logging;

/// Default project directory when `--project` is omitted.
const DEFAULT_PROJECT_DIR: &str = "gordian-project";

const USAGE: &str = "usage:
  gordian                              print version
  gordian agent [--project <dir>] [--no-review] [--input <file|->] [\"<prompt>\"]
                                       run one or more agent turns
  gordian tui [--project <dir>]        launch the copilot cockpit

options:
  -h, --help                           print help
  -V, --version                        print version";

fn print_version() {
    println!("gordian {}", env!("CARGO_PKG_VERSION"));
}

fn is_help_request(args: &[String]) -> bool {
    matches!(args, [arg] if arg == "--help" || arg == "-h")
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.first().map(String::as_str) {
        None => {
            logging::init_stderr_only();
            print_version();
            ExitCode::SUCCESS
        }
        Some("--help" | "-h" | "help") => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some("--version" | "-V" | "version") => {
            logging::init_stderr_only();
            print_version();
            ExitCode::SUCCESS
        }
        Some("agent") if is_help_request(&args[1..]) => {
            logging::init_stderr_only();
            tracing::info!(
                "usage: gordian agent [--project <dir>] [--no-review] [--input <file|->] [\"<prompt>\"]\n\nRun one or more headless agent turns."
            );
            ExitCode::SUCCESS
        }
        Some("agent") => match run_agent_command(&args[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                logging::init_stderr_only();
                tracing::error!("error: {e:#}");
                ExitCode::FAILURE
            }
        },
        Some("tui") if is_help_request(&args[1..]) => {
            logging::init_stderr_only();
            tracing::info!(
                "usage: gordian tui [--project <dir>]\n\nLaunch the interactive copilot cockpit."
            );
            ExitCode::SUCCESS
        }
        Some("tui") => match run_tui_command(&args[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                logging::init_stderr_only();
                tracing::error!("error: {e:#}");
                ExitCode::FAILURE
            }
        },
        Some(other) => {
            logging::init_stderr_only();
            tracing::error!("unknown command `{other}`");
            eprintln!("{USAGE}");
            ExitCode::FAILURE
        }
    }
}

/// Parse `tui` args into a project directory, defaulting to the current
/// working directory when `--project` is omitted — so `gordian tui` edits
/// `./design.kicad_sch` right where you launched it.
fn parse_tui_args(args: &[String]) -> Result<PathBuf> {
    let mut project_dir: Option<PathBuf> = None;
    let mut parse_options = true;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--" if parse_options => {
                parse_options = false;
                i += 1;
            }
            "--project" | "-p" if parse_options => {
                let dir = args
                    .get(i + 1)
                    .context("--project requires a directory argument")?;
                if project_dir.is_some() {
                    bail!("project directory was specified more than once");
                }
                project_dir = Some(PathBuf::from(dir));
                i += 2;
            }
            other if parse_options && other.starts_with('-') => {
                bail!("unknown tui option `{other}`")
            }
            other => {
                if project_dir.is_some() {
                    bail!("unexpected extra project directory `{other}`");
                }
                project_dir = Some(PathBuf::from(other));
                i += 1;
            }
        }
    }
    Ok(project_dir.unwrap_or_else(default_tui_project_dir))
}

/// The default project directory for `gordian tui` with no `--project`: the
/// current working directory, so the schematic lands next to where the user
/// launched the cockpit (never in a hidden tempdir).
fn default_tui_project_dir() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Run the `tui` subcommand: launch the cockpit on a single-threaded Tokio
/// runtime + `LocalSet`.
///
/// The TUI owns an `Rc` agent handle and terminal event loop on one thread, so
/// turn tasks use `spawn_local`. A current-thread runtime gives us a `LocalSet`
/// to host that.
fn run_tui_command(args: &[String]) -> Result<()> {
    let project_dir = parse_tui_args(args)?;
    let loaded = config::load_or_create()?;
    std::fs::create_dir_all(&project_dir)
        .with_context(|| format!("creating project dir {}", project_dir.display()))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("starting the Tokio runtime")?;
    let local = tokio::task::LocalSet::new();
    local.block_on(&runtime, tui::run(project_dir, loaded.config, loaded.path))
}

/// Parsed `agent` invocation: where to work, what to do, and whether the
/// post-turn self-correction review runs after a committed change.
struct AgentInvocation {
    project_dir: PathBuf,
    prompt: Option<String>,
    input: Option<PathBuf>,
    /// Run the independent post-commit review→fix pass (default on; `--no-review`
    /// turns it off). Read-only/conversational turns never trigger it regardless.
    review: bool,
}

/// Parse `agent` args into an [`AgentInvocation`].
///
/// Accepts both `agent --project <dir> "<prompt>"` and `agent <dir> "<prompt>"`,
/// as well as `agent "<prompt>"` (default project dir), with optional
/// `--no-review` and `--input <file|->` flags. A prompt is optional only when
/// input supplies the session turns.
fn parse_agent_args(args: &[String]) -> Result<AgentInvocation> {
    let mut project_dir: Option<PathBuf> = None;
    let mut positionals: Vec<String> = Vec::new();
    let mut review = true;
    let mut input: Option<PathBuf> = None;
    let mut parse_options = true;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--" if parse_options => {
                parse_options = false;
                i += 1;
            }
            "--project" | "-p" if parse_options => {
                if project_dir.is_some() {
                    bail!("project directory was specified more than once");
                }
                let dir = args
                    .get(i + 1)
                    .context("--project requires a directory argument")?;
                project_dir = Some(PathBuf::from(dir));
                i += 2;
            }
            "--no-review" if parse_options => {
                review = false;
                i += 1;
            }
            "--input" if parse_options => {
                if input.is_some() {
                    bail!("input was specified more than once");
                }
                let path = args.get(i + 1).context("--input requires a file or `-`")?;
                input = Some(PathBuf::from(path));
                i += 2;
            }
            flag if parse_options && flag.starts_with('-') => {
                bail!("unknown agent option `{flag}`");
            }
            other => {
                positionals.push(other.to_string());
                i += 1;
            }
        }
    }

    // With no --project flag, the first of two positionals is the project dir
    // and the second is the prompt (`agent <dir> "<prompt>"` form).
    let prompt = match (project_dir.is_some(), positionals.len()) {
        (_, 0) if input.is_some() => None,
        (_, 0) => bail!("missing prompt: provide a positional prompt or --input <file|->"),
        (true, 1) => Some(positionals.remove(0)),
        (true, _) => bail!("unexpected extra arguments after the prompt"),
        (false, 1) => Some(positionals.remove(0)),
        (false, 2) => {
            project_dir = Some(PathBuf::from(positionals.remove(0)));
            Some(positionals.remove(0))
        }
        (false, _) => bail!("unexpected extra arguments; expected [--project <dir>] \"<prompt>\""),
    };

    let project_dir = project_dir.unwrap_or_else(|| PathBuf::from(DEFAULT_PROJECT_DIR));
    Ok(AgentInvocation {
        project_dir,
        prompt,
        input,
        review,
    })
}

fn input_prompts(path: &Path) -> Result<Vec<String>> {
    let lines: Box<dyn BufRead> = if path.as_os_str() == "-" {
        Box::new(BufReader::new(std::io::stdin()))
    } else {
        Box::new(BufReader::new(std::fs::File::open(path).with_context(
            || format!("opening prompt input {}", path.display()),
        )?))
    };
    lines
        .lines()
        .map(|line| line.context("reading prompt input"))
        .filter_map(|line| match line {
            Ok(line)
                if {
                    let trimmed = line.trim();
                    trimmed.is_empty() || trimmed == "---" || trimmed.starts_with('#')
                } =>
            {
                None
            }
            Ok(line) => Some(Ok(line.trim().to_owned())),
            Err(error) => Some(Err(error)),
        })
        .collect()
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct UsageTotals {
    provider_requests: u64,
    input_tokens: u64,
    output_tokens: u64,
    cache_write_tokens: u64,
    cache_read_tokens: u64,
}

#[derive(Debug, Default)]
struct AgentDebugLog {
    usage: UsageTotals,
}

impl AgentDebugLog {
    fn observe(&mut self, ev: &AgentEvent) -> Option<String> {
        match ev {
            AgentEvent::AssistantDelta(_) => None,
            AgentEvent::AssistantText(text) => {
                let text = text.trim();
                (!text.is_empty()).then(|| format!("assistant: {text}"))
            }
            AgentEvent::ToolStarted { name } => Some(format!("tool -> {name}")),
            AgentEvent::ToolFinished {
                name,
                summary,
                image_path,
            } => {
                let image = image_path
                    .as_deref()
                    .map(|path| format!(" (image: {path})"))
                    .unwrap_or_default();
                Some(format!("tool <- {name}: {summary}{image}"))
            }
            AgentEvent::Usage {
                provider_requests,
                input_tokens,
                output_tokens,
                cache_write_tokens,
                cache_read_tokens,
            } => {
                self.usage.provider_requests += provider_requests;
                self.usage.input_tokens += input_tokens;
                self.usage.output_tokens += output_tokens;
                self.usage.cache_write_tokens += cache_write_tokens;
                self.usage.cache_read_tokens += cache_read_tokens;
                Some(format!(
                    "usage: provider_requests={provider_requests} in={input_tokens} out={output_tokens} \
                     cache_write={cache_write_tokens} cache_read={cache_read_tokens}"
                ))
            }
            AgentEvent::Compacted {
                messages_before,
                messages_after,
            } => Some(format!(
                "compacted: messages {messages_before} -> {messages_after}"
            )),
            AgentEvent::TurnDone => Some("turn done".to_string()),
            AgentEvent::ReviewStarted { round } => Some(format!("review -> round {round}")),
            AgentEvent::Reviewed {
                round,
                score,
                defects,
            } => {
                let defect_summary = if defects.is_empty() {
                    "no defects".to_string()
                } else {
                    format!("{} defect(s): {}", defects.len(), defects.join("; "))
                };
                Some(format!(
                    "review <- round {round}: score {}/10 — {defect_summary}",
                    format_score(*score)
                ))
            }
        }
    }
}

fn format_score(score: f64) -> String {
    if score.fract().abs() < f64::EPSILON {
        format!("{score:.0}")
    } else {
        format!("{score:.1}")
    }
}

/// Run the `agent` subcommand: one headless session against real Bedrock + KiCAD.
fn run_agent_command(args: &[String]) -> Result<()> {
    let AgentInvocation {
        project_dir,
        prompt,
        input,
        review,
    } = parse_agent_args(args)?;
    let mut prompts = prompt.into_iter().collect::<Vec<_>>();
    if let Some(path) = input.as_ref() {
        prompts.extend(input_prompts(path)?);
    }
    if prompts.is_empty() {
        bail!("prompt input contained no prompts");
    }

    let loaded = config::load_or_create()?;
    let config = loaded.config;

    // 1. Detect KiCAD (symbol libs + kicad).
    let env = config::detect_kicad(&config).context(
        "no KiCAD installation found — install KiCAD 9+/10 or set kicad.symbolDir / \
         kicad.footprintDir / kicad.cliPath in config.toml so the agent can resolve \
         symbols and run ERC",
    )?;
    // 2. Build the LLM client from TOML config.
    let mut client = gordian_core::GenaiProvider::from_config(&config.llm).with_context(|| {
        format!(
            "could not build the LLM client — set llm.adapter, llm.model, and llm.apiKey in {}",
            loaded.path.display()
        )
    })?;
    if let Ok(thread) = std::env::var("GORDIAN_THREAD_ID") {
        client = client.with_thread_identifier(thread);
    }
    let _log_guard = logging::init(&project_dir, client.thread_identifier());
    tracing::info!(
        "kicad: {} (cli: {}, pcbnew: {}, symbols: {})",
        env.version(),
        env.cli_path().display(),
        env.pcbnew_path().display(),
        env.symbol_dir().display()
    );
    tracing::info!("config: {}", loaded.path.display());
    tracing::info!("thread:  {}", client.thread_identifier());

    // 3. Tool context over the real project directory. Schematic mutators derive
    //    its human-style floorplan from the netlist (`infer_ir`), so no separate
    //    layout client is wired here.
    let ctx =
        AgentRuntime::for_project_with_config(env.clone(), project_dir.clone(), config.clone())
            .context("building the tool context for the project")?;
    let sch_path = ctx.sch_path().to_path_buf();
    tracing::info!("project: {}", project_dir.display());
    tracing::info!("turns:   {}", prompts.len());

    // 4. Run every turn on the same agent. By default each routes
    //    through `run_turn_reviewed`: after a turn that COMMITS a design change,
    //    an independent reviewer pass scores the netlist and feeds high-confidence
    //    defects into a bounded follow-up fix turn. `--no-review` runs the plain
    //    turn. A live events channel mirrors the TUI transcript in stderr so
    //    headless runs remain debuggable.
    let runtime = tokio::runtime::Runtime::new().context("starting the Tokio runtime")?;
    let system = system_prompt();
    let mut agent = Agent::new(client, ctx, system);

    tracing::info!(target: logging::EVENTS_TARGET, "--- agent events ---");
    let run = runtime.block_on(async {
        let mut turns = Vec::with_capacity(prompts.len());
        for (index, prompt) in prompts.iter().enumerate() {
            let turn = index + 1;
            tracing::info!(target: logging::EVENTS_TARGET, "turn {turn}: {prompt}");
            let started = std::time::Instant::now();
            let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
            let printer = tokio::spawn(async move {
                let mut log = AgentDebugLog::default();
                while let Some(ev) = events_rx.recv().await {
                    if let Some(line) = log.observe(&ev) {
                        tracing::info!(target: logging::EVENTS_TARGET, "{line}");
                    }
                }
                log.usage
            });

            let outcome = if review && config.agent.post_commit_review {
                agent
                    .run_turn_reviewed(
                        prompt,
                        prompt,
                        Some(&events_tx),
                        config.agent.review_fix_rounds as usize,
                    )
                    .await
            } else {
                agent.run_turn(prompt, Some(&events_tx)).await
            }?;
            drop(events_tx);
            let usage = printer.await.unwrap_or_default();
            let elapsed = started.elapsed().as_secs_f64();
            tracing::info!(
                target: logging::EVENTS_TARGET,
                "turn {turn} done: stop={:?} requests={} elapsed={elapsed:.1}s",
                outcome.stop_reason,
                usage.provider_requests
            );
            turns.push((outcome, usage));
        }
        Ok::<_, anyhow::Error>(turns)
    });
    // A timed-out `spawn_blocking` placement cannot be cancelled by Tokio. Do
    // not let one detached tool keep the one-shot headless CLI alive forever
    // after its turn result and diagnostics are already available.
    runtime.shutdown_timeout(Duration::from_secs(1));
    let turns = run.context("running the agent session")?;
    let usage = turns
        .iter()
        .fold(UsageTotals::default(), |mut total, (_, usage)| {
            total.provider_requests += usage.provider_requests;
            total.input_tokens += usage.input_tokens;
            total.output_tokens += usage.output_tokens;
            total.cache_write_tokens += usage.cache_write_tokens;
            total.cache_read_tokens += usage.cache_read_tokens;
            total
        });
    let outcome = &turns.last().expect("at least one prompt").0;

    // 5. Report the outcome.
    tracing::info!("--- agent turn ---");
    tracing::info!("tool calls made: {}", outcome.tool_calls_made);
    tracing::info!("applied (wrote schematic): {}", outcome.applied);
    tracing::info!("stop reason: {:?}", outcome.stop_reason);
    tracing::info!(
        "provider requests: {} (all model invocations; separate from the agent main-request safety budget)",
        usage.provider_requests
    );
    tracing::info!(
        "tokens: input {} output {} cache_write {} cache_read {}",
        usage.input_tokens,
        usage.output_tokens,
        usage.cache_write_tokens,
        usage.cache_read_tokens
    );
    tracing::info!("final reply:\n{}", outcome.final_text.trim());

    // 6. Final ERC: re-run on whatever the agent produced (the source of truth).
    tracing::info!("--- ERC ---");
    if !sch_path.exists() {
        tracing::warn!("no schematic was written at {}", sch_path.display());
        bail!("the agent did not produce a schematic");
    }
    let report = env
        .erc(&sch_path)
        .with_context(|| format!("running ERC on {}", sch_path.display()))?;
    tracing::info!("errors:   {}", report.error_count());
    tracing::info!("warnings: {}", report.warning_count());
    tracing::info!("schematic: {}", sch_path.display());

    if report.error_count() > 0 {
        // A nonzero ERC error count is real signal, not a tool failure — surface
        // it as a failing exit so scripts notice, but after printing the path.
        bail!(
            "ERC reported {} error(s) on the generated schematic",
            report.error_count()
        );
    }
    if turns
        .iter()
        .any(|(outcome, _)| outcome.stop_reason != StopReason::Completed)
    {
        bail!("one or more agent turns ended without completing their quality contract");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_project_flag_and_prompt() {
        let inv = parse_agent_args(&[
            "--project".into(),
            "/tmp/demo".into(),
            "make a board".into(),
        ])
        .unwrap();
        assert_eq!(inv.project_dir, PathBuf::from("/tmp/demo"));
        assert_eq!(inv.prompt.as_deref(), Some("make a board"));
        assert!(inv.review, "review defaults on");
    }

    #[test]
    fn parses_positional_dir_and_prompt() {
        let inv = parse_agent_args(&["/tmp/demo".into(), "make a board".into()]).unwrap();
        assert_eq!(inv.project_dir, PathBuf::from("/tmp/demo"));
        assert_eq!(inv.prompt.as_deref(), Some("make a board"));
    }

    #[test]
    fn parses_prompt_only_with_default_dir() {
        let inv = parse_agent_args(&["make a board".into()]).unwrap();
        assert_eq!(inv.project_dir, PathBuf::from(DEFAULT_PROJECT_DIR));
        assert_eq!(inv.prompt.as_deref(), Some("make a board"));
    }

    #[test]
    fn no_review_flag_disables_review() {
        let inv = parse_agent_args(&["--no-review".into(), "make a board".into()]).unwrap();
        assert_eq!(inv.prompt.as_deref(), Some("make a board"));
        assert!(!inv.review, "--no-review turns the post-commit review off");
    }

    #[test]
    fn errors_with_no_prompt() {
        assert!(parse_agent_args(&[]).is_err());
        assert!(parse_agent_args(&["--project".into(), "/tmp/demo".into()]).is_err());
    }

    #[test]
    fn tui_args_reject_ambiguous_or_unknown_project_arguments() {
        assert!(parse_tui_args(&["one".into(), "two".into()]).is_err());
        assert!(
            parse_tui_args(&[
                "--project".into(),
                "one".into(),
                "--project".into(),
                "two".into()
            ])
            .is_err()
        );
        assert!(parse_tui_args(&["--unknown".into()]).is_err());
    }

    #[test]
    fn agent_args_reject_unknown_and_duplicate_options() {
        assert!(parse_agent_args(&["--unknown".into(), "prompt".into()]).is_err());
        assert!(
            parse_agent_args(&[
                "--project".into(),
                "one".into(),
                "--project".into(),
                "two".into(),
                "prompt".into(),
            ])
            .is_err()
        );
    }

    #[test]
    fn agent_double_dash_allows_a_dash_prefixed_prompt() {
        let inv = parse_agent_args(&["--".into(), "--literal prompt".into()]).unwrap();
        assert_eq!(inv.prompt.as_deref(), Some("--literal prompt"));
        assert!(inv.review);
    }

    #[test]
    fn input_allows_an_optional_positional_prompt() {
        let inv = parse_agent_args(&[
            "--project".into(),
            "/tmp/demo".into(),
            "first".into(),
            "--input".into(),
            "-".into(),
        ])
        .unwrap();
        assert_eq!(inv.prompt.as_deref(), Some("first"));
        assert_eq!(inv.input, Some(PathBuf::from("-")));

        let inv = parse_agent_args(&["--input".into(), "prompts.txt".into()]).unwrap();
        assert_eq!(inv.prompt, None);
        assert_eq!(inv.input, Some(PathBuf::from("prompts.txt")));
    }

    #[test]
    fn tui_double_dash_allows_a_dash_prefixed_project_path() {
        assert_eq!(
            parse_tui_args(&["--".into(), "--literal-project".into()]).unwrap(),
            PathBuf::from("--literal-project")
        );
    }

    #[test]
    fn recognizes_only_standalone_help_requests() {
        assert!(is_help_request(&["--help".into()]));
        assert!(is_help_request(&["-h".into()]));
        assert!(!is_help_request(&[]));
        assert!(!is_help_request(&["--help".into(), "extra".into()]));
    }

    #[test]
    fn debug_log_formats_agent_events_and_accumulates_usage() {
        let mut log = AgentDebugLog::default();

        assert_eq!(
            log.observe(&AgentEvent::AssistantText("  checking schematic\n".into())),
            Some("assistant: checking schematic".into())
        );
        assert_eq!(
            log.observe(&AgentEvent::ToolStarted {
                name: "sync_board".into()
            }),
            Some("tool -> sync_board".into())
        );
        assert_eq!(
            log.observe(&AgentEvent::ToolFinished {
                name: "sync_board".into(),
                summary: "written".into(),
                image_path: Some(".gordian/renders/render-001.png".into()),
            }),
            Some("tool <- sync_board: written (image: .gordian/renders/render-001.png)".into())
        );
        assert_eq!(
            log.observe(&AgentEvent::Reviewed {
                round: 1,
                score: 6.0,
                defects: vec!["- U1: missing decoupling".into()],
            }),
            Some("review <- round 1: score 6/10 — 1 defect(s): - U1: missing decoupling".into())
        );
        assert_eq!(
            log.observe(&AgentEvent::Usage {
                provider_requests: 1,
                input_tokens: 100,
                output_tokens: 20,
                cache_write_tokens: 30,
                cache_read_tokens: 40,
            }),
            Some("usage: provider_requests=1 in=100 out=20 cache_write=30 cache_read=40".into())
        );

        assert_eq!(
            log.usage,
            UsageTotals {
                provider_requests: 1,
                input_tokens: 100,
                output_tokens: 20,
                cache_write_tokens: 30,
                cache_read_tokens: 40,
            }
        );
    }

    #[test]
    fn debug_log_skips_noisy_streaming_deltas() {
        let mut log = AgentDebugLog::default();
        assert_eq!(
            log.observe(&AgentEvent::AssistantDelta("partial".into())),
            None
        );
        assert_eq!(log.observe(&AgentEvent::AssistantText("   ".into())), None);
    }
}
