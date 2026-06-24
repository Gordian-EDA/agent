//! Pure-core agent-loop tests: a [`ScriptedClient`] drives the loop against a
//! [`MockToolProvider`] (no KiCAD, no network), so the control flow — tool
//! dispatch → result feedback → the preview → approve → commit gate → final
//! text — and the multi-turn history are exercised deterministically.
//!
//! This is the behavior oracle for the generalized [`ToolEffect::Gated`] gate:
//! it reproduces the schematic apply-gate's preview→approve→commit semantics
//! (and the "ready=false ⇒ no prompt, self-repair" and rejection paths) without
//! any domain code.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use gordian_core::testing::{ScriptedClient, final_text, tool_call};
use gordian_core::{
    Agent, AgentEvent, ApplyInfo, Approvals, AutoApprove, Provider, ReviewOutcome, RunMode,
    ToolCall, ToolDef, ToolEffect, ToolOutcome, ToolProvider,
};
use serde_json::{Value, json};

/// A mock domain provider with one read tool (`search`), one authoring tool
/// (`draft`), and one gated tool (`apply`). `apply` mimics the schematic gate:
/// a `Preview` returns a diff with `ready` from the input's `compiles` flag; a
/// `Commit` returns `{written:true}` and `ApplyInfo{committed,summary}`.
#[derive(Default)]
struct MockToolProvider {
    /// Records (name, mode) of every `run` call, so a test can assert the gate
    /// previewed-then-committed (and never committed on rejection).
    runs: Arc<Mutex<Vec<(String, RunMode)>>>,
}

#[async_trait]
impl ToolProvider for MockToolProvider {
    fn defs(&self) -> Vec<ToolDef> {
        vec![
            ToolDef { name: "search".into(), description: "".into(), input_schema: json!({}) },
            ToolDef { name: "draft".into(), description: "".into(), input_schema: json!({}) },
            ToolDef { name: "apply".into(), description: "".into(), input_schema: json!({}) },
        ]
    }

    fn effect(&self, name: &str) -> ToolEffect {
        match name {
            "apply" => ToolEffect::Gated,
            "draft" => ToolEffect::Authoring,
            _ => ToolEffect::ReadOnly,
        }
    }

    fn wants_apply(&self, call: &ToolCall) -> bool {
        call.input.get("commit").and_then(Value::as_bool) == Some(true)
    }

    fn is_authoring_for_commit(&self, name: &str) -> bool {
        matches!(name, "search" | "draft" | "apply")
    }

    fn commit_nudge(&self) -> &str {
        "NUDGE: commit now."
    }

    async fn run(&self, call: &ToolCall, mode: RunMode, _reviewer: &dyn Provider) -> ToolOutcome {
        self.runs.lock().unwrap().push((call.name.clone(), mode));
        match (call.name.as_str(), mode) {
            ("apply", RunMode::Preview) => {
                let compiles = call.input.get("compiles").and_then(Value::as_bool).unwrap_or(true);
                ToolOutcome {
                    value: if compiles {
                        json!({ "ok": true, "would_write": true, "diff": { "added": ["R1"] } })
                    } else {
                        json!({ "ok": false, "errors": 1, "diagnostics": ["bad part"] })
                    },
                    images: Vec::new(),
                    apply: Some(ApplyInfo { ready: compiles, ..Default::default() }),
                }
            }
            ("apply", RunMode::Commit) => ToolOutcome {
                value: json!({ "ok": true, "written": true }),
                images: Vec::new(),
                apply: Some(ApplyInfo {
                    ready: true,
                    committed: true,
                    summary: "ERC 0 errors, 0 warnings".into(),
                }),
            },
            _ => ToolOutcome::plain(json!({ "ok": true })),
        }
    }

    async fn review_committed(&self, _intent: &str, _r: &dyn Provider) -> Option<ReviewOutcome> {
        None
    }
}

#[tokio::test]
async fn gate_previews_then_commits_on_approve() {
    let runs = Arc::new(Mutex::new(Vec::new()));
    let tools = MockToolProvider { runs: Arc::clone(&runs) };
    let client = ScriptedClient::new(vec![
        tool_call("t1", "search", json!({})),
        tool_call("t2", "apply", json!({ "commit": true })),
        final_text("done"),
    ]);
    let mut agent = Agent::new(Box::new(client), Box::new(tools), "sys");
    let mut approvals = AutoApprove::yes();

    let out = agent.run_turn("go", &mut approvals, None).await.unwrap();

    assert!(out.applied, "approve=yes commits: {out:?}");
    assert_eq!(out.final_text, "done");
    let runs = runs.lock().unwrap();
    // The gate previewed, THEN committed (the preview probe is internal).
    assert_eq!(
        *runs,
        vec![
            ("search".into(), RunMode::Normal),
            ("apply".into(), RunMode::Preview),
            ("apply".into(), RunMode::Commit),
        ]
    );
    // The internal preview probe is not counted as a tool call.
    assert_eq!(out.tool_calls_made, 2, "search + apply (preview probe uncounted)");
}

#[tokio::test]
async fn gate_rejects_and_never_commits() {
    let runs = Arc::new(Mutex::new(Vec::new()));
    let tools = MockToolProvider { runs: Arc::clone(&runs) };
    let client = ScriptedClient::new(vec![
        tool_call("t1", "apply", json!({ "commit": true })),
        final_text("ok then"),
    ]);
    let mut agent = Agent::new(Box::new(client), Box::new(tools), "sys");
    let mut approvals = AutoApprove::no();

    let out = agent.run_turn("go", &mut approvals, None).await.unwrap();

    assert!(!out.applied, "reject must not commit: {out:?}");
    let runs = runs.lock().unwrap();
    assert_eq!(
        *runs,
        vec![("apply".into(), RunMode::Preview)],
        "rejected: previewed but NEVER committed"
    );
}

#[tokio::test]
async fn gate_skips_approval_when_preview_not_ready() {
    // A non-compiling apply: preview returns ready=false, so the loop returns the
    // diagnostics straight back with NO approval prompt and NO commit. A rejecting
    // approver proves approve() is never consulted.
    let runs = Arc::new(Mutex::new(Vec::new()));
    let tools = MockToolProvider { runs: Arc::clone(&runs) };
    let client = ScriptedClient::new(vec![
        tool_call("t1", "apply", json!({ "commit": true, "compiles": false })),
        final_text("will fix"),
    ]);
    let mut agent = Agent::new(Box::new(client), Box::new(tools), "sys");

    struct PanicApprover;
    #[async_trait]
    impl Approvals for PanicApprover {
        async fn approve(&mut self, _p: &Value) -> bool {
            panic!("approval must NOT be consulted when the preview is not ready");
        }
    }
    let mut approvals = PanicApprover;

    let out = agent.run_turn("go", &mut approvals, None).await.unwrap();
    assert!(!out.applied);
    let runs = runs.lock().unwrap();
    assert_eq!(*runs, vec![("apply".into(), RunMode::Preview)], "only the preview ran");
}

#[tokio::test]
async fn stall_after_authoring_is_nudged_then_commits() {
    // Research, premature text-only stop → NUDGE, apply+commit, done.
    let tools = MockToolProvider::default();
    let client = ScriptedClient::new(vec![
        tool_call("t1", "search", json!({})),
        final_text("I looked it up."), // stalls without committing
        tool_call("t2", "apply", json!({ "commit": true })),
        final_text("done"),
    ]);
    let mut agent = Agent::new(Box::new(client), Box::new(tools), "sys");
    let mut approvals = AutoApprove::yes();

    let out = agent.run_turn("go", &mut approvals, None).await.unwrap();
    assert!(out.applied, "the nudge drove the stalled model to commit: {out:?}");
    assert_eq!(out.final_text, "done");
}

#[tokio::test]
async fn stall_nudge_is_bounded_and_gives_up() {
    // A model that simply will NOT commit must terminate after MAX_COMMIT_NUDGES
    // (2): the script ends after the 3rd stop; a 3rd nudge would exhaust it.
    let tools = MockToolProvider::default();
    let client = ScriptedClient::new(vec![
        tool_call("t1", "search", json!({})),
        final_text("stop 1"), // → nudge 1
        final_text("stop 2"), // → nudge 2
        final_text("stop 3"), // nudges exhausted → return
    ]);
    let mut agent = Agent::new(Box::new(client), Box::new(tools), "sys");
    let mut approvals = AutoApprove::yes();

    let out = agent
        .run_turn("go", &mut approvals, None)
        .await
        .expect("must terminate, not exhaust the script");
    assert!(!out.applied);
    assert_eq!(out.final_text, "stop 3");
}

#[tokio::test]
async fn second_turn_sees_the_first_turns_messages() {
    let tools = MockToolProvider::default();
    let (client, seen) = ScriptedClient::recording(vec![final_text("answer one"), final_text("answer two")]);
    let mut agent = Agent::new(Box::new(client), Box::new(tools), "sys");
    let mut approvals = AutoApprove::yes();

    agent.run_turn("first prompt", &mut approvals, None).await.unwrap();
    agent.run_turn("second prompt", &mut approvals, None).await.unwrap();

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "one model call per turn");
    let second = &seen[1];
    assert_eq!(second.len(), 3, "user1 + assistant1 + user2: {second:#?}");
}

#[tokio::test]
async fn usage_tokens_flow_through_completions() {
    use tokio::sync::mpsc::unbounded_channel;
    let tools = MockToolProvider::default();
    let completion = gordian_core::Completion {
        text: "done".into(),
        stop_reason: "end_turn".into(),
        input_tokens: 1234,
        output_tokens: 56,
        ..Default::default()
    };
    let client = ScriptedClient::new(vec![completion]);
    let mut agent = Agent::new(Box::new(client), Box::new(tools), "sys");
    let mut approvals = AutoApprove::yes();
    let (tx, mut rx) = unbounded_channel();

    agent.run_turn("hi", &mut approvals, Some(&tx)).await.unwrap();

    let mut saw_usage = false;
    while let Ok(ev) = rx.try_recv() {
        if let AgentEvent::Usage { input_tokens, output_tokens } = ev {
            assert_eq!((input_tokens, output_tokens), (1234, 56));
            saw_usage = true;
        }
    }
    assert!(saw_usage, "a Usage event is emitted per completion");
}

#[tokio::test]
async fn applied_event_carries_the_domain_summary() {
    use tokio::sync::mpsc::unbounded_channel;
    let tools = MockToolProvider::default();
    let client = ScriptedClient::new(vec![
        tool_call("t1", "apply", json!({ "commit": true })),
        final_text("done"),
    ]);
    let mut agent = Agent::new(Box::new(client), Box::new(tools), "sys");
    let mut approvals = AutoApprove::yes();
    let (tx, mut rx) = unbounded_channel();

    agent.run_turn("go", &mut approvals, Some(&tx)).await.unwrap();

    let mut summary = None;
    while let Ok(ev) = rx.try_recv() {
        if let AgentEvent::Applied { summary: s } = ev {
            summary = Some(s);
        }
    }
    assert_eq!(summary.as_deref(), Some("ERC 0 errors, 0 warnings"));
}
