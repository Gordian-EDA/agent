//! The `gordian` CLI.
//!
//! - `gordian` (no args) prints the version.
//! - `gordian agent [--project <dir>] "<prompt>"` runs ONE headless agent turn
//!   against real Bedrock + real KiCAD, auto-approving the write, and prints the
//!   live transcript, turn outcome, token totals, and final ERC result. This is
//!   the CLI form of the interactive copilot.
//! - `gordian tui [--project <dir>]` launches the ratatui copilot cockpit
//!   (spec §11): a chat transcript, a proposed-changes apply-gate, and an input
//!   line, driving the same agent interactively.

mod config;
mod tui;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use gordian_core::AgentRuntime;
use gordian_core::prompts::system_prompt;
use gordian_core::{Agent, AgentEvent, AutoApprove};
use kicad_cli::KicadCli;

/// Default project directory when `--project` is omitted.
const DEFAULT_PROJECT_DIR: &str = "gordian-project";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.first().map(String::as_str) {
        None => {
            println!("gordian {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("agent") => match run_agent_command(&args[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e:#}");
                ExitCode::FAILURE
            }
        },
        Some("tui") => match run_tui_command(&args[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e:#}");
                ExitCode::FAILURE
            }
        },
        Some(other) => {
            eprintln!("error: unknown command `{other}`");
            eprintln!(
                "usage:\n  \
                 gordian                              print version\n  \
                 gordian agent [--project <dir>] \"<prompt>\"   run one agent turn\n  \
                 gordian tui [--project <dir>]                  launch the copilot cockpit"
            );
            ExitCode::FAILURE
        }
    }
}

/// Parse `tui` args into a project directory, defaulting to the current
/// working directory when `--project` is omitted — so `gordian tui` edits
/// `./design.kicad_sch` right where you launched it.
fn parse_tui_args(args: &[String]) -> Result<PathBuf> {
    let mut project_dir: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--project" | "-p" => {
                let dir = args
                    .get(i + 1)
                    .context("--project requires a directory argument")?;
                if project_dir.is_some() {
                    bail!("project directory was specified more than once");
                }
                project_dir = Some(PathBuf::from(dir));
                i += 2;
            }
            other if other.starts_with('-') => bail!("unknown tui option `{other}`"),
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
    prompt: String,
    /// Run the independent post-commit review→fix pass (default on; `--no-review`
    /// turns it off). Read-only/conversational turns never trigger it regardless.
    review: bool,
}

/// Parse `agent` args into an [`AgentInvocation`].
///
/// Accepts both `agent --project <dir> "<prompt>"` and `agent <dir> "<prompt>"`,
/// as well as `agent "<prompt>"` (default project dir), with an optional
/// `--no-review` flag. The prompt is the last remaining positional argument.
fn parse_agent_args(args: &[String]) -> Result<AgentInvocation> {
    let mut project_dir: Option<PathBuf> = None;
    let mut positionals: Vec<String> = Vec::new();
    let mut review = true;
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
        (_, 0) => bail!("missing prompt: gordian agent [--project <dir>] \"<prompt>\""),
        (true, 1) => positionals.remove(0),
        (true, _) => bail!("unexpected extra arguments after the prompt"),
        (false, 1) => positionals.remove(0),
        (false, 2) => {
            project_dir = Some(PathBuf::from(positionals.remove(0)));
            positionals.remove(0)
        }
        (false, _) => bail!("unexpected extra arguments; expected [--project <dir>] \"<prompt>\""),
    };

    let project_dir = project_dir.unwrap_or_else(|| PathBuf::from(DEFAULT_PROJECT_DIR));
    Ok(AgentInvocation {
        project_dir,
        prompt,
        review,
    })
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct UsageTotals {
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
            AgentEvent::Applied { summary } => Some(format!("applied: {summary}")),
            AgentEvent::Usage {
                input_tokens,
                output_tokens,
                cache_write_tokens,
                cache_read_tokens,
            } => {
                self.usage.input_tokens += input_tokens;
                self.usage.output_tokens += output_tokens;
                self.usage.cache_write_tokens += cache_write_tokens;
                self.usage.cache_read_tokens += cache_read_tokens;
                Some(format!(
                    "usage: in={input_tokens} out={output_tokens} \
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

/// Run the `agent` subcommand: one headless turn against real Bedrock + KiCAD.
fn run_agent_command(args: &[String]) -> Result<()> {
    let AgentInvocation {
        project_dir,
        prompt,
        review,
    } = parse_agent_args(args)?;

    let loaded = config::load_or_create()?;
    let config = loaded.config;

    // 1. Detect KiCAD (symbol libs + kicad-cli).
    let env = config::detect_kicad(&config).context(
        "no KiCAD installation found — install KiCAD 9+/10 or set kicad.symbolDir / \
         kicad.footprintDir / kicad.cliPath in config.toml so the agent can resolve \
         symbols and run ERC",
    )?;
    eprintln!(
        "kicad: {} (symbols: {})",
        env.cli_version,
        env.symbol_dir.display()
    );
    eprintln!("config: {}", loaded.path.display());

    // 2. Build the LLM client from TOML config.
    let client = gordian_core::GenaiProvider::from_config(&config.llm).with_context(|| {
        format!(
            "could not build the LLM client — set llm.adapter, llm.model, and llm.apiKey in {}",
            loaded.path.display()
        )
    })?;

    // 3. Tool context over the real project directory. `apply_design` derives
    //    its human-style floorplan from the netlist (`infer_ir`), so no separate
    //    layout client is wired here.
    let ctx =
        AgentRuntime::for_project_with_config(env.clone(), project_dir.clone(), config.clone())
            .context("building the tool context for the project")?;
    let sch_path = ctx.sch_path().to_path_buf();
    eprintln!("project: {}", project_dir.display());
    eprintln!("prompt:  {prompt}\n");

    // 4. Run ONE agent turn, auto-approving the apply. By default it routes
    //    through `run_turn_reviewed`: after a turn that COMMITS a design change,
    //    an independent reviewer pass scores the netlist and feeds high-confidence
    //    defects into a bounded follow-up fix turn. `--no-review` runs the plain
    //    turn. A live events channel mirrors the TUI transcript in stderr so
    //    headless runs remain debuggable.
    let runtime = tokio::runtime::Runtime::new().context("starting the Tokio runtime")?;
    let system = system_prompt();
    let mut agent = Agent::new(client, ctx, system);
    let mut approvals = AutoApprove::yes();

    eprintln!("--- agent events ---");
    let (outcome, usage) = runtime
        .block_on(async {
            let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
            let printer = tokio::spawn(async move {
                let mut log = AgentDebugLog::default();
                while let Some(ev) = events_rx.recv().await {
                    if let Some(line) = log.observe(&ev) {
                        eprintln!("{line}");
                    }
                }
                log.usage
            });

            let run = if review && config.agent.post_commit_review {
                // intent == prompt: the design goal the reviewer judges against.
                agent
                    .run_turn_reviewed(
                        &prompt,
                        &prompt,
                        &mut approvals,
                        Some(&events_tx),
                        config.agent.review_fix_rounds as usize,
                    )
                    .await
            } else {
                agent
                    .run_turn(&prompt, &mut approvals, Some(&events_tx))
                    .await
            };
            drop(events_tx);
            let usage = printer.await.unwrap_or_default();
            run.map(|outcome| (outcome, usage))
        })
        .context("running the agent turn")?;

    // 5. Report the outcome.
    println!("--- agent turn ---");
    println!("tool calls made: {}", outcome.tool_calls_made);
    println!("applied (wrote schematic): {}", outcome.applied);
    println!("stop reason: {:?}", outcome.stop_reason);
    println!(
        "tokens: input {} output {} cache_write {} cache_read {}",
        usage.input_tokens, usage.output_tokens, usage.cache_write_tokens, usage.cache_read_tokens
    );
    println!("\nfinal reply:\n{}", outcome.final_text.trim());

    // 6. Final ERC: re-run on whatever the agent produced (the source of truth).
    println!("\n--- ERC ---");
    if !sch_path.exists() {
        println!("no schematic was written at {}", sch_path.display());
        bail!("the agent did not produce a schematic");
    }
    let report = KicadCli::new(&env)
        .erc(&sch_path)
        .with_context(|| format!("running ERC on {}", sch_path.display()))?;
    println!("errors:   {}", report.error_count());
    println!("warnings: {}", report.warning_count());
    println!("schematic: {}", sch_path.display());

    if report.error_count() > 0 {
        // A nonzero ERC error count is real signal, not a tool failure — surface
        // it as a failing exit so scripts notice, but after printing the path.
        bail!(
            "ERC reported {} error(s) on the generated schematic",
            report.error_count()
        );
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
        assert_eq!(inv.prompt, "make a board");
        assert!(inv.review, "review defaults on");
    }

    #[test]
    fn parses_positional_dir_and_prompt() {
        let inv = parse_agent_args(&["/tmp/demo".into(), "make a board".into()]).unwrap();
        assert_eq!(inv.project_dir, PathBuf::from("/tmp/demo"));
        assert_eq!(inv.prompt, "make a board");
    }

    #[test]
    fn parses_prompt_only_with_default_dir() {
        let inv = parse_agent_args(&["make a board".into()]).unwrap();
        assert_eq!(inv.project_dir, PathBuf::from(DEFAULT_PROJECT_DIR));
        assert_eq!(inv.prompt, "make a board");
    }

    #[test]
    fn no_review_flag_disables_review() {
        let inv = parse_agent_args(&["--no-review".into(), "make a board".into()]).unwrap();
        assert_eq!(inv.prompt, "make a board");
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
        assert_eq!(inv.prompt, "--literal prompt");
        assert!(inv.review);
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
                name: "regenerate_board".into()
            }),
            Some("tool -> regenerate_board".into())
        );
        assert_eq!(
            log.observe(&AgentEvent::ToolFinished {
                name: "regenerate_board".into(),
                summary: "written".into(),
                image_path: Some(".gordian/renders/render-001.png".into()),
            }),
            Some(
                "tool <- regenerate_board: written (image: .gordian/renders/render-001.png)".into()
            )
        );
        assert_eq!(
            log.observe(&AgentEvent::Applied {
                summary: "ERC 0 errors, 0 warnings".into()
            }),
            Some("applied: ERC 0 errors, 0 warnings".into())
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
                input_tokens: 100,
                output_tokens: 20,
                cache_write_tokens: 30,
                cache_read_tokens: 40,
            }),
            Some("usage: in=100 out=20 cache_write=30 cache_read=40".into())
        );

        assert_eq!(
            log.usage,
            UsageTotals {
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
