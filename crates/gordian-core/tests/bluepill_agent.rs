//! The interactive-synthesis milestone (Plan 4, Task 4).
//!
//! This is the end-to-end proof that the founding prompt, driven by the agent
//! loop against REAL AWS Bedrock and REAL KiCAD, synthesizes an ERC-clean
//! `.kicad_sch` — NOT a hand-written fixture, but the design the model produces
//! interactively (search_symbols → get_symbol_info → validate → apply, with
//! self-repair off the structured diagnostics).
//!
//! It is `#[ignore]` because it costs live LLM round-trips (a minute+) and needs
//! a platform Gordian TOML config with `llm.model` / `llm.apiKey` and a KiCAD
//! install. It also SKIPs gracefully when either is absent, so an
//! accidental `--ignored` run on a machine without credentials does not fail.
//!
//! Run it manually:
//!   cargo test -p gordian-core --test bluepill_agent -- --ignored --nocapture

mod common;

use gordian_core::AgentRuntime;
use gordian_core::prompts::system_prompt;
use gordian_core::{Agent, AutoApprove};
use kicad_cli::KicadCli;
use kicad_env::KicadEnv;

/// The EXACT founding prompt the whole project is organized around.
const FOUNDING_PROMPT: &str = "Design me a bluepill-style STM32H7 dev board with USB and I2C pins";

#[tokio::test]
#[ignore = "live: needs AWS Bedrock token + KiCAD; costs LLM round-trips"]
async fn bluepill_founding_prompt_yields_erc_clean_schematic() {
    // SKIP gracefully if KiCAD is absent.
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    // SKIP gracefully if no LLM credentials are configured.
    let client = match common::live_provider_from_config() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SKIP: no LLM client ({e})");
            return;
        }
    };

    // Fresh temp project — the agent writes design.kicad_sch into it.
    let tempdir = tempfile::tempdir().expect("tempdir");
    let ctx = AgentRuntime::for_project(env.clone(), tempdir.path().to_path_buf())
        .expect("tool context for temp project");
    let sch_path = ctx.sch_path().to_path_buf();
    assert!(!sch_path.exists(), "the project starts with no schematic");

    let mut agent = Agent::new(client, ctx, system_prompt());
    let mut approvals = AutoApprove::yes();

    let outcome = agent
        .run_turn(FOUNDING_PROMPT, &mut approvals, None)
        .await
        .expect("the agent turn should complete");

    eprintln!(
        "tool_calls_made={} applied={}\nfinal_text:\n{}",
        outcome.tool_calls_made, outcome.applied, outcome.final_text
    );

    // The turn must have produced a real schematic.
    assert!(
        sch_path.exists(),
        "the agent must write design.kicad_sch (applied={}, tool_calls={})",
        outcome.applied,
        outcome.tool_calls_made
    );
    assert!(
        outcome.applied,
        "the agent must commit the design (applied=false): {}",
        outcome.final_text
    );

    // The milestone assertion: the synthesized schematic is ERC-clean.
    let report = KicadCli::new(&env)
        .erc(&sch_path)
        .expect("ERC should run on the generated schematic");
    eprintln!(
        "ERC: {} errors, {} warnings on {}",
        report.error_count(),
        report.warning_count(),
        sch_path.display()
    );
    for v in &report.violations {
        eprintln!("  [{}] {}: {}", v.severity, v.kind, v.description);
    }
    assert_eq!(
        report.error_count(),
        0,
        "the synthesized schematic must be ERC-clean (0 errors)"
    );
}
