//! Exercise the TUI apply-gate **bridge** end to end with a scripted mock LLM.
//!
//! The cockpit's real bridge (`tui::TuiApprovals`) forwards the agent's
//! `approve(diff)` over a channel and awaits a oneshot the UI fulfils on an
//! `a`/`r` keypress. This test recreates that exact pattern with a public stand-in
//! and drives a real [`agent::Agent`] (so the async approval flow — agent awaits
//! `approve()`, a "UI" task receives the diff, a simulated `a` resolves `true`,
//! the write commits — is covered without a terminal).
//!
//! Needs KiCAD for the tools; SKIPs gracefully otherwise.

use agent::llm::{Completion, LlmClient, Message, ToolCall, ToolDef};
use agent::tools::ToolCtx;
use agent::{Agent, Approvals};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Mutex;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::sync::oneshot;

/// A mock LLM replaying a fixed script of completions, one per `complete()`.
struct MockLlm {
    script: Mutex<std::collections::VecDeque<Completion>>,
}

impl MockLlm {
    fn script(c: Vec<Completion>) -> Self {
        Self {
            script: Mutex::new(c.into_iter().collect()),
        }
    }
}

#[async_trait]
impl LlmClient for MockLlm {
    async fn complete(
        &self,
        _system: &str,
        _messages: &[Message],
        _tools: &[ToolDef],
    ) -> Result<Completion> {
        self.script
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("mock script exhausted"))
    }
}

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

fn tool_call(id: &str, name: &str, input: Value) -> Completion {
    Completion {
        text: String::new(),
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: name.into(),
            input,
        }],
        stop_reason: "tool_use".into(),
        ..Default::default()
    }
}

fn final_text(t: &str) -> Completion {
    Completion {
        text: t.into(),
        tool_calls: Vec::new(),
        stop_reason: "end_turn".into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn bridge_approval_a_keypress_commits_the_write() {
    let Some(ctx) = ToolCtx::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let sch_path = ctx.sch_path().to_path_buf();
    assert!(!sch_path.exists());

    let script = vec![
        tool_call(
            "t1",
            "apply_design",
            json!({ "yaml": TINY_YAML, "commit": true }),
        ),
        final_text("done"),
    ];
    let mut agent = Agent::new(Box::new(MockLlm::script(script)), ctx);

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

    let outcome = agent
        .run_turn("add two resistors", &mut approvals, None)
        .await
        .unwrap();
    ui.await.unwrap();

    assert!(outcome.applied, "approved gate must commit: {outcome:?}");
    assert!(sch_path.exists(), "approved write must land the .kicad_sch");
}

#[tokio::test]
async fn bridge_rejection_r_keypress_blocks_the_write() {
    let Some(ctx) = ToolCtx::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let sch_path = ctx.sch_path().to_path_buf();

    let script = vec![
        tool_call(
            "t1",
            "apply_design",
            json!({ "yaml": TINY_YAML, "commit": true }),
        ),
        final_text("done"),
    ];
    let mut agent = Agent::new(Box::new(MockLlm::script(script)), ctx);

    let (gate_tx, mut gate_rx) = unbounded_channel::<GateRequest>();
    let mut approvals = BridgeApprovals { gate_tx };

    let ui = tokio::spawn(async move {
        if let Some((_diff, reply)) = gate_rx.recv().await {
            // Simulated `r` keypress → reject.
            let _ = reply.send(false);
        }
    });

    let outcome = agent
        .run_turn("add two resistors", &mut approvals, None)
        .await
        .unwrap();
    ui.await.unwrap();

    assert!(
        !outcome.applied,
        "rejected gate must not commit: {outcome:?}"
    );
    assert!(!sch_path.exists(), "rejected write must not land the file");
}
