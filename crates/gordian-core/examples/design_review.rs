//! Agent design with an INDEPENDENT review→fix loop, via [`gordian_core::Agent::run_turn_reviewed`]:
//! turn 1 drafts the design, then a FRESH LLM reviewer (see `gordian_core::review`) audits the committed
//! netlist for electrical-CORRECTNESS faults (pin-function mis-wires, voltage-domain part-selection,
//! topology errors — the class ERC and the layout critic both miss) and feeds any high-confidence
//! defects back as fix turns. This example just drives the method and renders the result.
//!
//! Usage: cargo run --release -p gordian-core --example design_review -- <out.png> "<prompt>"

mod config_support;

use gordian_core::{Agent, AgentEvent, AutoApprove};
use kicad_cli::KicadCli;
use kicad_env::KicadEnv;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let out = args
        .next()
        .expect("usage: design_review <out.png> <prompt>");
    let prompt = args
        .next()
        .expect("usage: design_review <out.png> <prompt>");

    let config = config_support::load_config()?;
    let env = KicadEnv::detect_with(
        config.kicad.symbol_dir.as_deref(),
        config.kicad.footprint_dir.as_deref(),
        config.kicad.cli_path.as_deref(),
    )
    .expect("no KiCAD environment detected");
    let tmp = tempfile::tempdir()?;
    let ctx = gordian_core::AgentRuntime::for_project_with_config(
        env.clone(),
        tmp.path().to_path_buf(),
        config.clone(),
    )?;
    let sch_path = ctx.sch_path().to_path_buf();
    let mut agent = Agent::new(
        gordian_core::GenaiProvider::from_config(&config.llm)?,
        ctx,
        gordian_core::prompts::system_prompt(),
    );
    let mut approvals = AutoApprove::yes();

    // Show the per-round review verdicts (and apply commits) as they happen.
    let (tx, mut rx) = mpsc::unbounded_channel();
    let printer = tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            match ev {
                AgentEvent::Reviewed {
                    round,
                    score,
                    defects,
                } => {
                    eprintln!(
                        "[review {round}] score={score} high-conf critical/major defects={}",
                        defects.len()
                    );
                    for d in &defects {
                        eprintln!("    {d}");
                    }
                }
                AgentEvent::Applied { summary } => {
                    eprintln!("  applied ({summary})");
                }
                _ => {}
            }
        }
    });

    // Turn 1 designs; up to 2 review→fix rounds follow.
    agent
        .run_turn_reviewed(&prompt, &prompt, &mut approvals, Some(&tx), 2)
        .await?;
    drop(tx);
    printer.await.ok();

    if !sch_path.exists() {
        eprintln!("no schematic written");
        return Ok(());
    }
    let svg_dir = tempfile::tempdir()?;
    let svg_path = KicadCli::new(&env).export_svg_opts(&sch_path, svg_dir.path(), true)?;
    let svg = std::fs::read_to_string(&svg_path)?;
    let png = gordian_runtime::render::svg_to_png(&svg, 1600)?;
    std::fs::write(&out, png)?;
    std::fs::copy(
        &sch_path,
        std::path::Path::new(&out).with_extension("kicad_sch"),
    )
    .ok();
    if let Ok(y) = sch_io::read::lift(&env, &sch_path) {
        std::fs::write(std::path::Path::new(&out).with_extension("circuit.yaml"), y).ok();
    }
    println!("done: {out}");
    Ok(())
}
