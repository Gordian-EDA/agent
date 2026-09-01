//! Exercise the TUI per-mutator approval bridge with a scripted client.

use anyhow::Result;
use async_trait::async_trait;
use gordian_core::prompts::system_prompt;
use gordian_core::testing::{ScriptedClient, final_text, tool_call};
use gordian_core::{Agent, AgentRuntime, Approvals};
use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::sync::oneshot;

type GateRequest = (Value, oneshot::Sender<bool>);

struct BridgeApprovals {
    gate_tx: UnboundedSender<GateRequest>,
}

#[async_trait]
impl Approvals for BridgeApprovals {
    async fn approve(&mut self, operation: &Value) -> bool {
        let (tx, rx) = oneshot::channel();
        if self.gate_tx.send((operation.clone(), tx)).is_err() {
            return false;
        }
        rx.await.unwrap_or(false)
    }
}

fn place_parts_call() -> gordian_core::StreamEnd {
    tool_call(
        "t1",
        "place_parts",
        json!({
            "parts": [
                {"ref": "R1", "part": "Device:R", "value": "10k", "pins": {"1": "A", "2": "GND"}},
                {"ref": "R2", "part": "Device:R", "value": "10k", "pins": {"1": "A", "2": "GND"}}
            ]
        }),
    )
}

fn agent(ctx: AgentRuntime, completions: Vec<gordian_core::StreamEnd>) -> Agent<ScriptedClient> {
    Agent::new(ScriptedClient::new(completions), ctx, system_prompt())
}

#[tokio::test]
async fn approval_commits_the_mutation() -> Result<()> {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return Ok(());
    };
    let sch_path = ctx.sch_path().to_path_buf();
    let script = vec![
        place_parts_call(),
        tool_call("t2", "check_schematic", json!({})),
        final_text("done"),
    ];
    let mut agent = agent(ctx, script);
    let (gate_tx, mut gate_rx) = unbounded_channel::<GateRequest>();
    let mut approvals = BridgeApprovals { gate_tx };

    let ui = tokio::spawn(async move {
        if let Some((operation, reply)) = gate_rx.recv().await {
            assert_eq!(operation["approval_kind"], "operation");
            assert_eq!(operation["operation"], "place_parts");
            assert_eq!(operation["arguments"]["parts"][0]["ref"], "R1");
            let _ = reply.send(true);
        }
    });

    let outcome = agent
        .run_turn("add two resistors", &mut approvals, None)
        .await?;
    ui.await?;

    assert!(
        outcome.applied,
        "approved mutation must commit: {outcome:?}"
    );
    assert!(
        sch_path.exists(),
        "approved mutation must write the schematic"
    );
    Ok(())
}

#[tokio::test]
async fn rejection_blocks_the_mutation() -> Result<()> {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return Ok(());
    };
    let sch_path = ctx.sch_path().to_path_buf();
    let mut agent = agent(ctx, vec![place_parts_call(), final_text("not changed")]);
    let (gate_tx, mut gate_rx) = unbounded_channel::<GateRequest>();
    let mut approvals = BridgeApprovals { gate_tx };

    let ui = tokio::spawn(async move {
        if let Some((_operation, reply)) = gate_rx.recv().await {
            let _ = reply.send(false);
        }
    });

    let outcome = agent
        .run_turn("add two resistors", &mut approvals, None)
        .await?;
    ui.await?;

    assert!(!outcome.applied, "rejected mutation must not commit");
    assert!(
        !sch_path.exists(),
        "rejected mutation must not write the file"
    );
    Ok(())
}
