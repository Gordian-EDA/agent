//! The `autopcb` CLI.
//!
//! - `autopcb` (no args) prints the version.
//! - `autopcb agent [--project <dir>] "<prompt>"` runs ONE headless agent turn
//!   against real Bedrock + real KiCAD, auto-approving the write, and prints the
//!   turn outcome plus the final ERC result. This is the CLI form of the
//!   interactive copilot.
//! - `autopcb tui [--project <dir>]` launches the ratatui copilot cockpit
//!   (spec §11): a chat transcript, a proposed-changes apply-gate, and an input
//!   line, driving the same agent interactively.

mod tui;

use std::path::PathBuf;
use std::process::ExitCode;

use gordian_core::{Agent, AgentEvent, AutoApprove};
use gordian_kicad::PcbTools;
use gordian_kicad::prompts::system_prompt_with_reference;
use gordian_kicad::tools::PcbToolCtx;
use anyhow::{Context, Result, bail};
use kicad_cli_rs::cli::KicadCli;
use kicad_cli_rs::env::KicadEnv;

/// Default project directory when `--project` is omitted.
const DEFAULT_PROJECT_DIR: &str = "autopcb-project";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.first().map(String::as_str) {
        None => {
            println!("auto-pcb {}", env!("CARGO_PKG_VERSION"));
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
                 autopcb                              print version\n  \
                 autopcb agent [--project <dir>] \"<prompt>\"   run one agent turn\n  \
                 autopcb tui [--project <dir>]                  launch the copilot cockpit"
            );
            ExitCode::FAILURE
        }
    }
}

/// Parse `tui` args into a project directory, defaulting to the current
/// working directory when `--project` is omitted — so `autopcb tui` edits
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
                project_dir = Some(PathBuf::from(dir));
                i += 2;
            }
            other => {
                project_dir = Some(PathBuf::from(other));
                i += 1;
            }
        }
    }
    Ok(project_dir.unwrap_or_else(default_tui_project_dir))
}

/// The default project directory for `autopcb tui` with no `--project`: the
/// current working directory, so the schematic lands next to where the user
/// launched the cockpit (never in a hidden tempdir).
fn default_tui_project_dir() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Run the `tui` subcommand: launch the cockpit on a single-threaded Tokio
/// runtime + `LocalSet`.
///
/// The agent's [`ToolCtx`] is intentionally **not** `Send` (its symbol caches use
/// non-thread-safe interior mutability), so the turn task is spawned with
/// `spawn_local` and the whole UI runs on one thread. A current-thread runtime
/// gives us a `LocalSet` to host that.
fn run_tui_command(args: &[String]) -> Result<()> {
    let project_dir = parse_tui_args(args)?;
    std::fs::create_dir_all(&project_dir)
        .with_context(|| format!("creating project dir {}", project_dir.display()))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("starting the Tokio runtime")?;
    let local = tokio::task::LocalSet::new();
    local.block_on(&runtime, tui::run(project_dir))
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

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--project" | "-p" => {
                let dir = args
                    .get(i + 1)
                    .context("--project requires a directory argument")?;
                project_dir = Some(PathBuf::from(dir));
                i += 2;
            }
            "--no-review" => {
                review = false;
                i += 1;
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
        (_, 0) => bail!("missing prompt: autopcb agent [--project <dir>] \"<prompt>\""),
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
    Ok(AgentInvocation { project_dir, prompt, review })
}

/// Run the `agent` subcommand: one headless turn against real Bedrock + KiCAD.
fn run_agent_command(args: &[String]) -> Result<()> {
    let AgentInvocation { project_dir, prompt, review } = parse_agent_args(args)?;

    // 1. Detect KiCAD (symbol libs + kicad-cli).
    let env = KicadEnv::detect().context(
        "no KiCAD installation found — install KiCAD 9+/10 (or set AUTO_PCB_SYMBOL_DIR) so \
         the agent can resolve symbols and run ERC",
    )?;
    eprintln!(
        "kicad: {} (symbols: {})",
        env.cli_version,
        env.symbol_dir.display()
    );

    // 2. Build the LLM client from the environment / local .env (OpenAI-compatible
    //    when OPENAI_API_KEY is set, else AWS Bedrock).
    let client = llm_client::from_env().context(
        "could not build the LLM client — set OPENAI_API_KEY + OPENAI_BASE_URL (or \
         AWS_BEARER_TOKEN_BEDROCK) in the environment or a local .env file",
    )?;

    // 3. Tool context over the real project directory. `apply_design` derives
    //    its human-style floorplan from the netlist (`infer_ir`), so no separate
    //    layout client is wired here.
    let ctx = PcbToolCtx::for_project(env.clone(), project_dir.clone())
        .context("building the tool context for the project")?;
    let sch_path = ctx.sch_path().to_path_buf();
    eprintln!("project: {}", project_dir.display());
    eprintln!("prompt:  {prompt}\n");

    // 4. Run ONE agent turn, auto-approving the apply. By default it routes
    //    through `run_turn_reviewed`: after a turn that COMMITS a design change,
    //    an independent reviewer pass scores the netlist and feeds high-confidence
    //    defects into a bounded follow-up fix turn. `--no-review` runs the plain
    //    turn. A live events channel surfaces each `Reviewed` round to the log.
    let runtime = tokio::runtime::Runtime::new().context("starting the Tokio runtime")?;
    // Retrieval-augment the system prompt: at design-start the intent (the prompt)
    // is known, so inject the single best-matching real human design as a worked
    // few-shot example. Falls back to the plain prompt when no corpus / match.
    let system = system_prompt_with_reference(&env, &prompt);
    let mut agent = Agent::new(client, Box::new(PcbTools::new(ctx)), system);
    let mut approvals = AutoApprove::yes();
    let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();

    let outcome = runtime
        .block_on(async {
            if review {
                // intent == prompt: the design goal the reviewer judges against.
                agent
                    .run_turn_reviewed(&prompt, &prompt, &mut approvals, Some(&events_tx), 1)
                    .await
            } else {
                agent.run_turn(&prompt, &mut approvals, None).await
            }
        })
        .context("running the agent turn")?;

    // Drain and log any review rounds the self-correction pass emitted.
    while let Ok(ev) = events_rx.try_recv() {
        if let AgentEvent::Reviewed { round, score, defects } = ev {
            eprintln!(
                "review (round {round}): score {score}/10 — {}",
                if defects.is_empty() {
                    "no functional defects".to_string()
                } else {
                    format!("{} defect(s): {}", defects.len(), defects.join("; "))
                }
            );
        }
    }

    // 5. Report the outcome.
    println!("--- agent turn ---");
    println!("tool calls made: {}", outcome.tool_calls_made);
    println!("applied (wrote schematic): {}", outcome.applied);
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
        let inv =
            parse_agent_args(&["--no-review".into(), "make a board".into()]).unwrap();
        assert_eq!(inv.prompt, "make a board");
        assert!(!inv.review, "--no-review turns the post-commit review off");
    }

    #[test]
    fn errors_with_no_prompt() {
        assert!(parse_agent_args(&[]).is_err());
        assert!(parse_agent_args(&["--project".into(), "/tmp/demo".into()]).is_err());
    }
}
