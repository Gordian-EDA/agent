//! Headless LLM-driven PCB run: hand the model a natural-language BOARD request,
//! let it drive the real PCB tool loop (search footprints → regenerate_board →
//! place_board → route_board → check_board), then locate the saved `.kicad_pcb`
//! and report.
//!
//! This is the PCB analog of `agent_design` (which exercises the schematic side).
//! It tests the AGENT-FACING surface — the tool specs, the board rules/hints syntax,
//! and the system-prompt PCB guidance — the way a real user drives it, end to end.
//!
//! ```text
//! cargo run --release -p gordian-core --example board_agent -- <out.kicad_pcb> "<prompt>"
//! ```
//!
//! Needs the platform Gordian TOML config populated with `llm.adapter`,
//! `llm.model`, and `llm.apiKey`, plus an installed KiCAD (footprint library +
//! `kicad-cli pcb drc`).

mod config_support;

use gordian_core::{Agent, AutoApprove, Provider as _};
use kicad_cli::KicadCli;
use kicad_env::KicadEnv;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let out = args
        .next()
        .expect("usage: board_agent <out.kicad_pcb> <prompt>");
    let prompt = args
        .next()
        .expect("usage: board_agent <out.kicad_pcb> <prompt>");

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

    let tmp = tempfile::tempdir()?;
    let ctx = gordian_core::AgentRuntime::for_project_with_config(
        env.clone(),
        tmp.path().to_path_buf(),
        config,
    )?;
    let pcb_path = ctx.pcb_path();

    let mut agent = Agent::new(client, ctx, gordian_core::prompts::system_prompt());

    // Stream the tool calls so the run is visible while it works.
    let (tx, mut rx) = mpsc::unbounded_channel();
    let printer = tokio::spawn(async move {
        let (mut requests, mut tin, mut tout, mut cache_write, mut cache_read) =
            (0u64, 0u64, 0u64, 0u64, 0u64);
        while let Some(ev) = rx.recv().await {
            match ev {
                gordian_core::AgentEvent::Usage {
                    provider_requests,
                    input_tokens,
                    output_tokens,
                    cache_write_tokens,
                    cache_read_tokens,
                    ..
                } => {
                    requests += provider_requests;
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
        (requests, tin, tout, cache_write, cache_read)
    });

    let mut approvals = AutoApprove::yes();
    let outcome = agent.run_turn(&prompt, &mut approvals, Some(&tx)).await?;
    if let Err(e) = gordian_core::tools_pcb::save_session_if_open(agent.ctx()) {
        eprintln!("warning: could not save live KiCAD session before closing: {e}");
    }
    agent.ctx().close_kicad_session();
    drop(tx);
    let (requests, tin, tout, cache_write, cache_read) = printer.await.unwrap_or((0, 0, 0, 0, 0));

    eprintln!(
        "\n--- outcome ---\napplied={} tool_calls={} stop={:?} provider_requests={requests} tokens(in={tin} out={tout} cache_write={cache_write} cache_read={cache_read})",
        outcome.applied, outcome.tool_calls_made, outcome.stop_reason
    );
    eprintln!("final reply:\n{}\n", outcome.final_text.trim());

    if !pcb_path.exists() {
        eprintln!("NO BOARD WRITTEN — the agent did not export a .kicad_pcb.");
        return Ok(());
    }

    std::fs::copy(&pcb_path, &out)?;
    // The sibling .kicad_pro carries the design rules (net class) — keep it next to
    // the board so a re-run of kicad-cli DRC checks against the engine's rules.
    let pro = pcb_path.with_extension("kicad_pro");
    if pro.exists() {
        std::fs::copy(&pro, std::path::Path::new(&out).with_extension("kicad_pro")).ok();
    }
    let fab = tmp.path().join("fab");
    if fab.is_dir() {
        let fab_out = std::path::Path::new(&out).with_extension("fab");
        if fab_out.exists() {
            std::fs::remove_dir_all(&fab_out).ok();
        }
        std::fs::create_dir_all(&fab_out)?;
        for entry in std::fs::read_dir(&fab)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() {
                std::fs::copy(&path, fab_out.join(entry.file_name())).ok();
            }
        }
        eprintln!("fab bundle copied: {}", fab_out.display());
    }

    match KicadCli::new(&env).drc(std::path::Path::new(&out)) {
        Ok(r) => eprintln!(
            "DRC: {} errors, {} unconnected, {} total violations",
            r.error_count(),
            r.unconnected_items.len(),
            r.violations.len()
        ),
        Err(e) => eprintln!("DRC failed: {e}"),
    }
    println!("board written: {out}");
    Ok(())
}
