//! Exercise the TUI apply-gate **bridge** end to end with a scripted client.
//!
//! The cockpit's real bridge (`tui::TuiApprovals`) forwards the agent's
//! `approve(diff)` over a channel and awaits a oneshot the UI fulfils on an
//! `a`/`r` keypress. This test recreates that exact pattern with a public stand-in
//! (so the async approval flow — agent awaits `approve()`, a "UI" task receives the
//! diff, a simulated `a` resolves `true`, the write commits — is covered without a
//! terminal). It drives a real [`gordian_core::Agent`] over the KiCAD tools.
//!
//! Needs KiCAD for the tools; SKIPs gracefully otherwise.

use gordian_core::testing::{ScriptedClient, final_text, tool_call};
use gordian_core::{Agent, Approvals};
use gordian_core::prompts::system_prompt;
use gordian_core::tools::PcbToolCtx;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::sync::oneshot;

/// The same bridge shape the cockpit uses: forward the diff + a reply oneshot.
type GateRequest = (Value, oneshot::Sender<bool>);

struct BridgeApprovals {
    gate_tx: UnboundedSender<GateRequest>,
}

#[async_trait]
impl Approvals for BridgeApprovals {
    async fn approve(&mut self, diff: &Value) -> bool {
        let (tx, rx) = oneshot::channel();
        if self.gate_tx.send((diff.clone(), tx)).is_err() {
            return false;
        }
        rx.await.unwrap_or(false)
    }
}

const TINY_YAML: &str = "version: 1\n\
blocks:\n\
\x20 main:\n\
\x20   components:\n\
\x20     R1: {part: R, value: 10k, between: [A, GND]}\n\
\x20     R2: {part: R, value: 10k, between: [GND, B]}\n";

fn agent(ctx: PcbToolCtx, completions: Vec<gordian_core::Completion>) -> Agent {
    Agent::new(Box::new(ScriptedClient::new(completions)), ctx, system_prompt())
}

#[tokio::test]
async fn bridge_approval_a_keypress_commits_the_write() -> Result<()> {
    let Some(ctx) = PcbToolCtx::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return Ok(());
    };
    let sch_path = ctx.sch_path().to_path_buf();
    assert!(!sch_path.exists());

    let script = vec![
        tool_call("t1", "apply_design", json!({ "yaml": TINY_YAML, "commit": true })),
        final_text("done"),
    ];
    let mut agent = agent(ctx, script);

    let (gate_tx, mut gate_rx) = unbounded_channel::<GateRequest>();
    let mut approvals = BridgeApprovals { gate_tx };

    // The "UI" task: receive the gate request and simulate pressing `a`.
    let ui = tokio::spawn(async move {
        if let Some((diff, reply)) = gate_rx.recv().await {
            // The diff the UI sees is the dry-run with a structured diff.
            assert_eq!(diff.get("ok").and_then(Value::as_bool), Some(true));
            assert!(diff.get("diff").is_some(), "diff payload present: {diff}");
            // Simulated `a` keypress → approve.
            let _ = reply.send(true);
        }
    });

    let outcome = agent.run_turn("add two resistors", &mut approvals, None).await.unwrap();
    ui.await.unwrap();

    assert!(outcome.applied, "approved gate must commit: {outcome:?}");
    assert!(sch_path.exists(), "approved write must land the .kicad_sch");
    Ok(())
}

#[tokio::test]
async fn bridge_rejection_r_keypress_blocks_the_write() -> Result<()> {
    let Some(ctx) = PcbToolCtx::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return Ok(());
    };
    let sch_path = ctx.sch_path().to_path_buf();

    let script = vec![
        tool_call("t1", "apply_design", json!({ "yaml": TINY_YAML, "commit": true })),
        final_text("done"),
    ];
    let mut agent = agent(ctx, script);

    let (gate_tx, mut gate_rx) = unbounded_channel::<GateRequest>();
    let mut approvals = BridgeApprovals { gate_tx };

    let ui = tokio::spawn(async move {
        if let Some((_diff, reply)) = gate_rx.recv().await {
            // Simulated `r` keypress → reject.
            let _ = reply.send(false);
        }
    });

    let outcome = agent.run_turn("add two resistors", &mut approvals, None).await.unwrap();
    ui.await.unwrap();

    assert!(!outcome.applied, "rejected gate must not commit: {outcome:?}");
    assert!(!sch_path.exists(), "rejected write must not land the file");
    Ok(())
}
