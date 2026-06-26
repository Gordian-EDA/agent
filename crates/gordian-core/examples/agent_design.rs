//! Headless end-to-end agent run: hand the LLM a natural-language circuit request,
//! let it drive the real tool loop (search_symbols / validate / apply_design), then
//! render the schematic it produced to a content-only PNG.
//!
//! Usage:
//!   cargo run --release -p agent --example agent_design -- <out.png> "<prompt>"
//!
//! Needs the platform Gordian TOML config populated with `llm.model` and
//! `llm.apiKey`. Prints the model's final reply, the tool-call count, and the
//! engine's layout-warning list.

mod config_support;

use gordian_core::{Agent, AutoApprove, Provider as _};
use kicad_cli::KicadCli;
use kicad_env::KicadEnv;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let out = args.next().expect("usage: agent_design <out.png> <prompt>");
    let prompt = args.next().expect("usage: agent_design <out.png> <prompt>");

    if let Some(parent) = std::path::Path::new(&out).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let config = config_support::load_config()?;
    let env = KicadEnv::detect_with(
        config.kicad.symbol_dir.as_deref(),
        config.kicad.footprint_dir.as_deref(),
        config.kicad.cli_path.as_deref(),
    )
    .expect("no KiCAD environment detected");
    let client = gordian_core::GenaiProvider::from_config(&config.llm)?;
    let (provider, model) = client.status();
    eprintln!("provider={provider} model={model}\nprompt: {prompt}\n");

    // Fresh throwaway project for this run.
    let tmp = tempfile::tempdir()?;
    let ctx = gordian_core::AgentRuntime::for_project_with_config(
        env.clone(),
        tmp.path().to_path_buf(),
        config,
    )?;
    let sch_path = ctx.sch_path().to_path_buf();
    // The agent's own MULTI-BLOCK source (what it wrote via create_design) — preserved so the
    // multi-sheet path (tools/multisheet.py) can render one clean sheet per block. The lifted
    // YAML below is FLAT; this draft keeps the block structure.
    let draft_path = tmp.path().join(".gordian/draft.circuit.yaml");

    let mut agent = Agent::new(client, ctx, gordian_core::prompts::system_prompt());

    // Stream events so the run's tool calls are visible while it works.
    let (tx, mut rx) = mpsc::unbounded_channel();
    let printer = tokio::spawn(async move {
        let (mut tin, mut tout, mut cache_write, mut cache_read) = (0u64, 0u64, 0u64, 0u64);
        while let Some(ev) = rx.recv().await {
            match ev {
                gordian_core::AgentEvent::Usage {
                    input_tokens,
                    output_tokens,
                    cache_write_tokens,
                    cache_read_tokens,
                    ..
                } => {
                    tin += input_tokens;
                    tout += output_tokens;
                    cache_write += cache_write_tokens;
                    cache_read += cache_read_tokens;
                }
                gordian_core::AgentEvent::ToolStarted { name } => eprintln!("  tool -> {name}"),
                gordian_core::AgentEvent::ToolFinished { name, summary, .. } => {
                    eprintln!("       {name}: {summary}")
                }
                gordian_core::AgentEvent::AssistantText(t) if !t.trim().is_empty() => {
                    eprintln!("  ...: {}", t.trim());
                }
                _ => {}
            }
        }
        (tin, tout, cache_write, cache_read)
    });

    let mut approvals = AutoApprove::yes();
    let outcome = agent.run_turn(&prompt, &mut approvals, Some(&tx)).await?;
    drop(tx);
    let (tin, tout, cache_write, cache_read) = printer.await.unwrap_or((0, 0, 0, 0));

    eprintln!(
        "\n--- outcome ---\napplied={} tool_calls={} stop={:?} tokens(in={tin} out={tout} cache_write={cache_write} cache_read={cache_read})",
        outcome.applied, outcome.tool_calls_made, outcome.stop_reason
    );
    eprintln!("final reply:\n{}\n", outcome.final_text.trim());

    // Save the agent's DRAFT (its multi-block source) regardless of whether it committed,
    // so a dense design can be re-emitted as multi-sheet even if the agent only previewed.
    if draft_path.exists() {
        std::fs::copy(
            &draft_path,
            std::path::Path::new(&out).with_extension("draft.yaml"),
        )
        .ok();
    }

    if !sch_path.exists() {
        eprintln!("NO SCHEMATIC WRITTEN — the agent did not commit a design.");
        return Ok(());
    }

    // Render the committed schematic to a content-only PNG (sheet border excluded).
    let svg_dir = tempfile::tempdir()?;
    let svg_path = KicadCli::new(&env).export_svg_opts(&sch_path, svg_dir.path(), true)?;
    let svg = std::fs::read_to_string(&svg_path)?;
    let png = gordian_core::render::svg_to_png(&svg, 1600)?;
    std::fs::write(&out, png)?;
    // Keep the source sch + a lifted YAML next to the PNG so a defect can be
    // reproduced deterministically (re-render via layout_spike) without re-spending
    // an LLM call.
    std::fs::copy(
        &sch_path,
        std::path::Path::new(&out).with_extension("kicad_sch"),
    )
    .ok();
    if let Ok(yaml) = sch_io::read::lift(&env, &sch_path) {
        std::fs::write(
            std::path::Path::new(&out).with_extension("circuit.yaml"),
            yaml,
        )
        .ok();
    }
    if draft_path.exists() {
        std::fs::copy(
            &draft_path,
            std::path::Path::new(&out).with_extension("draft.yaml"),
        )
        .ok();
    }

    match KicadCli::new(&env).erc(&sch_path) {
        Ok(r) => eprintln!(
            "ERC: {} errors, {} warnings",
            r.error_count(),
            r.warning_count()
        ),
        Err(e) => eprintln!("ERC failed: {e}"),
    }
    println!("rendered {out}");
    Ok(())
}
