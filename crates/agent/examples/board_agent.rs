//! Headless LLM-driven PCB run: hand the model a natural-language BOARD request,
//! let it drive the real PCB tool loop (search footprints → create_board →
//! place_board → render_board → route_board → export_board), then locate the
//! exported `.kicad_pcb`, run KiCAD DRC on it, and report.
//!
//! This is the PCB analog of `agent_design` (which exercises the schematic side).
//! It tests the AGENT-FACING surface — the tool specs, the board rules/hints syntax,
//! and the system-prompt PCB guidance — the way a real user drives it, end to end.
//!
//! ```text
//! cargo run --release -p agent --example board_agent -- <out.kicad_pcb> "<prompt>"
//! ```
//!
//! Needs the OpenAI-compatible backend (OPENAI_API_KEY / OPENAI_BASE_URL) and an
//! installed KiCAD (footprint library + `kicad-cli pcb drc`).

use agent::{Agent, AutoApprove};
use kicad_bridge::cli::KicadCli;
use kicad_bridge::env::KicadEnv;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let out = args.next().expect("usage: board_agent <out.kicad_pcb> <prompt>");
    let prompt = args.next().expect("usage: board_agent <out.kicad_pcb> <prompt>");

    if let Some(parent) = std::path::Path::new(&out).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let env = KicadEnv::detect().expect("no KiCAD environment detected");
    let (provider, model) = agent::config::provider_status();
    eprintln!("provider={provider} model={model}\nprompt: {prompt}\n");

    let tmp = tempfile::tempdir()?;
    let ctx = agent::tools::ToolCtx::for_project(env.clone(), tmp.path().to_path_buf())?;
    let pcb_path = ctx.pcb_path();

    let client = agent::llm::from_env()?;
    let mut agent = Agent::new(client, ctx);

    // Stream the tool calls so the run is visible while it works.
    let (tx, mut rx) = mpsc::unbounded_channel();
    let printer = tokio::spawn(async move {
        let (mut tin, mut tout) = (0u64, 0u64);
        while let Some(ev) = rx.recv().await {
            match ev {
                agent::AgentEvent::Usage { input_tokens, output_tokens } => {
                    tin += input_tokens;
                    tout += output_tokens;
                }
                agent::AgentEvent::ToolStarted { name } => eprintln!("  tool -> {name}"),
                agent::AgentEvent::AssistantText(t) if !t.trim().is_empty() => {
                    eprintln!("  ...: {}", t.trim());
                }
                _ => {}
            }
        }
        (tin, tout)
    });

    let mut approvals = AutoApprove::yes();
    let outcome = agent.run_turn(&prompt, &mut approvals, Some(&tx)).await?;
    drop(tx);
    let (tin, tout) = printer.await.unwrap_or((0, 0));

    eprintln!(
        "\n--- outcome ---\napplied={} tool_calls={} stop={:?} tokens(in={tin} out={tout})",
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
