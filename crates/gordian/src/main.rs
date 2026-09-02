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

use std::collections::BTreeSet;
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
    read_input_prompts(lines)
}

fn read_input_prompts(lines: impl BufRead) -> Result<Vec<String>> {
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
    revisions: BTreeSet<u64>,
    files: BTreeSet<String>,
}

impl AgentDebugLog {
    fn observe(&mut self, ev: &AgentEvent) -> Option<String> {
        match ev {
            AgentEvent::AssistantDelta(_) => None,
            AgentEvent::AssistantText(text) => {
                let text = text.trim();
                (!text.is_empty()).then(|| format!("assistant: {text}"))
            }
            AgentEvent::ToolStarted { name, args, .. } => {
                Some(format!("tool -> {name} {}", compact_tool_args(args)))
            }
            AgentEvent::ToolFinished {
                name,
                summary,
                image_path,
                elapsed_ms,
                revision,
                result,
            } => {
                if let Some(revision) = revision {
                    self.revisions.insert(*revision);
                }
                collect_result_files(result, &mut self.files);
                let image = image_path
                    .as_deref()
                    .map(|path| format!(" (image: {path})"))
                    .unwrap_or_default();
                let revision = revision
                    .map(|revision| format!(", revision {revision}"))
                    .unwrap_or_default();
                let details = tool_result_details(name, result);
                Some(format!(
                    "tool <- {name} (elapsed {}s{revision}): {summary}{details}{image}",
                    format_tool_elapsed(*elapsed_ms)
                ))
            }
            AgentEvent::ProviderRequest {
                request,
                input_tokens,
                output_tokens,
                cache_write_tokens,
                cache_read_tokens,
                latency_ms,
            } => Some(format!(
                "usage: request #{request} in={input_tokens} out={output_tokens} cached={cache_read_tokens} cache_write={cache_write_tokens} latency={:.1}s",
                *latency_ms as f64 / 1_000.0
            )),
            AgentEvent::Diagnostic {
                level,
                target,
                message,
            } => Some(format!("{level}: {target}: {message}")),
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

fn phase_render_line(event: &AgentEvent) -> Option<String> {
    let AgentEvent::ToolFinished {
        name,
        image_path: Some(path),
        ..
    } = event
    else {
        return None;
    };
    matches!(name.as_str(), "render_schematic" | "render_board")
        .then(|| format!("phase-render: {path}"))
}

fn format_tool_elapsed(elapsed_ms: u64) -> String {
    if elapsed_ms < 1_000 {
        format!("{:.3}", elapsed_ms as f64 / 1_000.0)
    } else {
        format!("{:.1}", elapsed_ms as f64 / 1_000.0)
    }
}

fn compact_tool_args(args: &serde_json::Value) -> String {
    fn compact(value: &serde_json::Value, key: Option<&str>) -> serde_json::Value {
        match value {
            serde_json::Value::String(text) if text.chars().count() > 120 => {
                let prefix = text.chars().take(96).collect::<String>();
                serde_json::Value::String(format!("{prefix}…({} chars)", text.chars().count()))
            }
            serde_json::Value::Array(items) if key == Some("parts") => {
                let refs = items
                    .iter()
                    .filter_map(|part| part.get("ref").and_then(serde_json::Value::as_str))
                    .take(6)
                    .collect::<Vec<_>>();
                let suffix = if items.len() > refs.len() { ", …" } else { "" };
                serde_json::Value::String(format!(
                    "[{}{suffix} {} parts]",
                    refs.join(", "),
                    items.len()
                ))
            }
            serde_json::Value::Array(items) => serde_json::Value::Array(
                items.iter().map(|item| compact(item, None)).collect(),
            ),
            serde_json::Value::Object(fields) => serde_json::Value::Object(
                fields
                    .iter()
                    .map(|(key, value)| (key.clone(), compact(value, Some(key))))
                    .collect(),
            ),
            _ => value.clone(),
        }
    }
    compact(args, None).to_string()
}

fn tool_result_details(name: &str, result: &serde_json::Value) -> String {
    let refused = result.get("error").is_some()
        || result.get("ok").and_then(serde_json::Value::as_bool) == Some(false)
        || result.get("legal").and_then(serde_json::Value::as_bool) == Some(false);
    if refused {
        return format!(" | refusal={result}");
    }

    let keys: &[&str] = match name {
        "check_schematic" => &["errors", "warnings", "erc", "completeness", "diagnostics"],
        "check_board" => &[
            "blocking_findings",
            "reported_findings",
            "copper_violations",
            "unconnected_items",
            "top_violations",
            "top_unconnected",
        ],
        "place_parts" => &["placed", "nets", "gaps", "placement"],
        _ => &[
            "changed",
            "net_delta",
            "placed",
            "placed_refs",
            "retracted",
            "nets_to_reroute",
            "routed",
            "failed",
        ],
    };
    let mut facts = serde_json::Map::new();
    for key in keys {
        if let Some(value) = result.get(*key) {
            let value = if matches!(*key, "diagnostics" | "top_violations" | "top_unconnected") {
                value
                    .as_array()
                    .map(|items| serde_json::Value::Array(items.iter().take(3).cloned().collect()))
                    .unwrap_or_else(|| value.clone())
            } else if matches!(*key, "completeness" | "erc") {
                let mut report = value.clone();
                for findings in ["gaps", "violations"] {
                    if let Some(items) = report
                        .get_mut(findings)
                        .and_then(serde_json::Value::as_array_mut)
                    {
                        items.truncate(3);
                    }
                }
                report
            } else {
                value.clone()
            };
            facts.insert((*key).to_owned(), value);
        }
    }
    if facts.is_empty() {
        String::new()
    } else {
        format!(" | facts={}", serde_json::Value::Object(facts))
    }
}

fn collect_result_files(result: &serde_json::Value, files: &mut BTreeSet<String>) {
    for key in ["file", "path", "sch_path", "pcb_path", "fab_dir", "png_path"] {
        if let Some(path) = result.get(key).and_then(serde_json::Value::as_str) {
            files.insert(path.to_owned());
        }
    }
    if let Some(paths) = result.get("files").and_then(serde_json::Value::as_array) {
        files.extend(paths.iter().filter_map(serde_json::Value::as_str).map(str::to_owned));
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
        "KiCad 10 is required; set kicad.cliPath, kicad.symbolDir, and \
         kicad.footprintDir in config.toml",
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
        "kicad: {} (cli: {}, symbols: {})",
        env.version(),
        env.cli_path().display(),
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
                    if let Some(line) = phase_render_line(&ev) {
                        tracing::info!(target: logging::EVENTS_TARGET, "{line}");
                    }
                    if let Some(line) = log.observe(&ev) {
                        tracing::info!(target: logging::EVENTS_TARGET, "{line}");
                    }
                }
                log
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
            };
            drop(events_tx);
            let log = printer.await.unwrap_or_default();
            let usage = log.usage;
            let elapsed = started.elapsed().as_secs_f64();
            let outcome = outcome.map_err(|error| {
                let message = format!("{error:#}");
                tracing::error!(target: logging::EVENTS_TARGET, "error: gordian::agent: {message}");
                message
            });
            let stop = outcome
                .as_ref()
                .map(|outcome| format!("{:?}", outcome.stop_reason))
                .unwrap_or_else(|_| "Error".to_owned());
            let files = log.files.into_iter().collect::<Vec<_>>().join(",");
            let revisions = log
                .revisions
                .into_iter()
                .map(|revision| revision.to_string())
                .collect::<Vec<_>>()
                .join(",");
            tracing::info!(
                target: logging::EVENTS_TARGET,
                "turn: stop={stop} requests={} elapsed={elapsed:.1}s files=[{files}] revisions=[{revisions}]",
                usage.provider_requests
            );
            tracing::info!(
                target: logging::EVENTS_TARGET,
                "turn {turn} done: stop={stop} requests={} elapsed={elapsed:.1}s",
                usage.provider_requests
            );
            turns.push((outcome, usage));
        }
        turns
    });
    // A timed-out `spawn_blocking` placement cannot be cancelled by Tokio. Do
    // not let one detached tool keep the one-shot headless CLI alive forever
    // after its turn result and diagnostics are already available.
    runtime.shutdown_timeout(Duration::from_secs(1));
    let turns = run;
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
    let turn_errors = turns
        .iter()
        .filter_map(|(outcome, _)| outcome.as_ref().err())
        .cloned()
        .collect::<Vec<_>>();
    if !turn_errors.is_empty() {
        bail!("{} turn(s) failed: {}", turn_errors.len(), turn_errors.join("; "));
    }
    let outcome = turns
        .iter()
        .rev()
        .find_map(|(outcome, _)| outcome.as_ref().ok())
        .expect("at least one completed prompt");

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
    let turn_is_partial = outcome.stop_reason != StopReason::Completed;
    if !sch_path.exists() {
        tracing::warn!("no schematic was written at {}", sch_path.display());
        if turn_is_partial {
            return Ok(());
        }
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
        if turn_is_partial {
            tracing::warn!(
                "partial turn handed back with {} ERC error(s); continue from the saved files",
                report.error_count()
            );
        } else {
            bail!(
                "ERC reported {} error(s) on the generated schematic",
                report.error_count()
            );
        }
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

    #[tokio::test]
    async fn headless_stdin_multi_turn_continue_rebuilds_from_project_files() {
        use gordian_core::testing::{ScriptedClient, final_text, tool_call};
        use serde_json::json;

        let invocation = parse_agent_args(&[
            "--project".into(),
            "/tmp/resumable-project".into(),
            "--input".into(),
            "-".into(),
        ])
        .unwrap();
        assert_eq!(invocation.input, Some(PathBuf::from("-")));
        let prompts = read_input_prompts(std::io::Cursor::new(
            "create a two-resistor divider\ncontinue\n",
        ))
        .unwrap();
        assert_eq!(prompts, ["create a two-resistor divider", "continue"]);

        let Some(ctx) = AgentRuntime::detect_for_test() else {
            eprintln!("SKIP: no KiCAD detected");
            return;
        };
        let env = ctx.env().clone();
        let project_dir = ctx.project_dir().to_path_buf();
        let sch_path = ctx.sch_path().to_path_buf();
        let first_script = vec![
            tool_call(
                "place",
                "place_parts",
                json!({
                    "parts": [
                        {"ref": "R1", "part": "Device:R", "value": "10k", "pins": {"1": "VCC", "2": "MID"}},
                        {"ref": "R2", "part": "Device:R", "value": "10k", "pins": {"1": "MID", "2": "GND"}}
                    ]
                }),
            ),
            tool_call("check", "check_schematic", json!({})),
            final_text("first phase saved"),
        ];
        let mut first = Agent::new(ScriptedClient::new(first_script), ctx, system_prompt());
        first.run_turn(&prompts[0], None).await.unwrap();

        let resumed_ctx = AgentRuntime::new(env, project_dir, sch_path.clone()).unwrap();
        let resumed_script = vec![
            tool_call("read", "read_schematic", json!({})),
            final_text("continued from the saved schematic"),
        ];
        let mut resumed = Agent::new(
            ScriptedClient::new(resumed_script),
            resumed_ctx,
            system_prompt(),
        );
        let outcome = resumed.run_turn(&prompts[1], None).await.unwrap();

        assert_eq!(outcome.stop_reason, StopReason::Completed);
        let schematic = std::fs::read_to_string(sch_path).unwrap();
        assert!(schematic.contains("R1"));
        assert!(schematic.contains("R2"));
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
                name: "sync_board".into(),
                args: serde_json::json!({}),
                seq: 1,
            }),
            Some("tool -> sync_board {}".into())
        );
        assert_eq!(
            log.observe(&AgentEvent::ToolFinished {
                name: "sync_board".into(),
                summary: "written".into(),
                image_path: Some(".gordian/renders/render-001.png".into()),
                elapsed_ms: 1_200,
                revision: Some(7),
                result: serde_json::json!({"revision": 7}),
            }),
            Some(
                "tool <- sync_board (elapsed 1.2s, revision 7): written (image: .gordian/renders/render-001.png)"
                    .into()
            )
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

    #[test]
    fn headless_transcript_emits_one_phase_render_line_per_render() {
        let schematic = AgentEvent::ToolFinished {
            name: "render_schematic".into(),
            summary: "rendered".into(),
            image_path: Some("/tmp/schematic.png".into()),
            elapsed_ms: 20,
            revision: None,
            result: serde_json::json!({"ok": true}),
        };
        let board = AgentEvent::ToolFinished {
            name: "render_board".into(),
            summary: "rendered".into(),
            image_path: Some("/tmp/board.png".into()),
            elapsed_ms: 20,
            revision: None,
            result: serde_json::json!({"ok": true}),
        };

        assert_eq!(
            phase_render_line(&schematic).as_deref(),
            Some("phase-render: /tmp/schematic.png")
        );
        assert_eq!(
            phase_render_line(&board).as_deref(),
            Some("phase-render: /tmp/board.png")
        );
        assert_eq!(phase_render_line(&AgentEvent::TurnDone), None);
    }
}
