//! Pure agent-loop tests: a [`ScriptedClient`] drives the loop against a
//! [`StubBackend`] (no KiCAD, no network), so the control flow — tool dispatch →
//! result feedback → the preview → approve → commit gate → final text — and the
//! multi-turn history are exercised deterministically.
//!
//! This is the behavior oracle for the [`ToolEffect::Gated`] gate: it reproduces
//! the schematic apply-gate's preview→approve→commit semantics (and the
//! "ready=false ⇒ no prompt, self-repair" and rejection paths) without KiCAD. The
//! stub uses the REAL KiCAD tool NAMES (`search_symbols`/`edit_design`/
//! `apply_design`), so the loop's own concrete classifiers (effect / wants_apply /
//! authoring-for-commit) are exercised for free.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use gordian_core::testing::{ScriptedClient, final_text, tool_call};
use gordian_core::{
    Agent, AgentEvent, ApplyInfo, Approvals, AutoApprove, Provider, ReviewOutcome, RunMode,
    TestBackend, ToolCall, ToolOutcome,
};
use serde_json::{Value, json};

/// A stub tool backend mirroring the schematic gate over the real KiCAD tool
/// names: `search_symbols` (read), `edit_design` (authoring), `apply_design`
/// (gated). A `Preview` of `apply_design` returns a diff with `ready` from the
/// input's `compiles` flag; a `Commit` returns `{written:true}` and
/// `ApplyInfo{committed,summary}`.
#[derive(Default)]
struct StubBackend {
    /// Records (name, mode) of every `run` call, so a test can assert the gate
    /// previewed-then-committed (and never committed on rejection).
    runs: Arc<Mutex<Vec<(String, RunMode)>>>,
    /// Scripted independent reviews, popped front-to-back by `review_committed`
    /// (front = first round). Empty = nothing to review (`review_committed`
    /// returns `None`), the default for the non-review tests.
    reviews: Arc<Mutex<VecDeque<ReviewOutcome>>>,
}

#[async_trait]
impl TestBackend for StubBackend {
    async fn run(&self, call: &ToolCall, mode: RunMode, _reviewer: &dyn Provider) -> ToolOutcome {
        self.runs.lock().unwrap().push((call.name.clone(), mode));
        match (call.name.as_str(), mode) {
            ("apply_design", RunMode::Preview) => {
                let compiles = call.input.get("compiles").and_then(Value::as_bool).unwrap_or(true);
                ToolOutcome {
                    value: if compiles {
                        json!({ "ok": true, "would_write": true, "diff": { "added": ["R1"] } })
                    } else {
                        json!({ "ok": false, "errors": 1, "diagnostics": ["bad part"] })
                    },
                    apply: Some(ApplyInfo { ready: compiles, ..Default::default() }),
                    ..Default::default()
                }
            }
            ("apply_design", RunMode::Commit) => ToolOutcome {
                value: json!({ "ok": true, "written": true }),
                apply: Some(ApplyInfo {
                    ready: true,
                    committed: true,
                    summary: "ERC 0 errors, 0 warnings".into(),
                }),
                ..Default::default()
            },
            _ => ToolOutcome::plain(json!({ "ok": true })),
        }
    }

    async fn review_committed(&self, _intent: &str, _r: &dyn Provider) -> Option<ReviewOutcome> {
        self.reviews.lock().unwrap().pop_front()
    }
}

/// Build an agent over a stub backend and a scripted client.
fn agent(backend: StubBackend, completions: Vec<gordian_core::Completion>) -> Agent {
    Agent::with_test_backend(Box::new(ScriptedClient::new(completions)), Box::new(backend), "sys")
}

#[tokio::test]
async fn gate_previews_then_commits_on_approve() {
    let runs = Arc::new(Mutex::new(Vec::new()));
    let backend = StubBackend { runs: Arc::clone(&runs), ..Default::default() };
    let mut agent = agent(
        backend,
        vec![
            tool_call("t1", "search_symbols", json!({})),
            tool_call("t2", "apply_design", json!({ "commit": true })),
            final_text("done"),
        ],
    );
    let mut approvals = AutoApprove::yes();

    let out = agent.run_turn("go", &mut approvals, None).await.unwrap();

    assert!(out.applied, "approve=yes commits: {out:?}");
    assert_eq!(out.final_text, "done");
    let runs = runs.lock().unwrap();
    // The gate previewed, THEN committed (the preview probe is internal).
    assert_eq!(
        *runs,
        vec![
            ("search_symbols".into(), RunMode::Normal),
            ("apply_design".into(), RunMode::Preview),
            ("apply_design".into(), RunMode::Commit),
        ]
    );
    // The internal preview probe is not counted as a tool call.
    assert_eq!(out.tool_calls_made, 2, "search + apply (preview probe uncounted)");
}

#[tokio::test]
async fn gate_rejects_and_never_commits() {
    let runs = Arc::new(Mutex::new(Vec::new()));
    let backend = StubBackend { runs: Arc::clone(&runs), ..Default::default() };
    let mut agent = agent(
        backend,
        vec![
            tool_call("t1", "apply_design", json!({ "commit": true })),
            final_text("ok then"),
        ],
    );
    let mut approvals = AutoApprove::no();

    let out = agent.run_turn("go", &mut approvals, None).await.unwrap();

    assert!(!out.applied, "reject must not commit: {out:?}");
    let runs = runs.lock().unwrap();
    assert_eq!(
        *runs,
        vec![("apply_design".into(), RunMode::Preview)],
        "rejected: previewed but NEVER committed"
    );
}

#[tokio::test]
async fn gate_skips_approval_when_preview_not_ready() {
    // A non-compiling apply: preview returns ready=false, so the loop returns the
    // diagnostics straight back with NO approval prompt and NO commit. A rejecting
    // approver proves approve() is never consulted.
    let runs = Arc::new(Mutex::new(Vec::new()));
    let backend = StubBackend { runs: Arc::clone(&runs), ..Default::default() };
    let mut agent = agent(
        backend,
        vec![
            tool_call("t1", "apply_design", json!({ "commit": true, "compiles": false })),
            final_text("will fix"),
        ],
    );

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
    assert_eq!(*runs, vec![("apply_design".into(), RunMode::Preview)], "only the preview ran");
}

#[tokio::test]
async fn stall_after_authoring_is_nudged_then_commits() {
    // Research, premature text-only stop → NUDGE, apply+commit, done.
    let mut agent = agent(
        StubBackend::default(),
        vec![
            tool_call("t1", "search_symbols", json!({})),
            final_text("I looked it up."), // stalls without committing
            tool_call("t2", "apply_design", json!({ "commit": true })),
            final_text("done"),
        ],
    );
    let mut approvals = AutoApprove::yes();

    let out = agent.run_turn("go", &mut approvals, None).await.unwrap();
    assert!(out.applied, "the nudge drove the stalled model to commit: {out:?}");
    assert_eq!(out.final_text, "done");
}

#[tokio::test]
async fn stall_nudge_is_bounded_and_gives_up() {
    // A model that simply will NOT commit must terminate after MAX_COMMIT_NUDGES
    // (2): the script ends after the 3rd stop; a 3rd nudge would exhaust it.
    let mut agent = agent(
        StubBackend::default(),
        vec![
            tool_call("t1", "search_symbols", json!({})),
            final_text("stop 1"), // → nudge 1
            final_text("stop 2"), // → nudge 2
            final_text("stop 3"), // nudges exhausted → return
        ],
    );
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
    let (client, seen) =
        ScriptedClient::recording(vec![final_text("answer one"), final_text("answer two")]);
    let mut agent =
        Agent::with_test_backend(Box::new(client), Box::new(StubBackend::default()), "sys");
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
    let completion = gordian_core::Completion {
        text: "done".into(),
        stop_reason: "end_turn".into(),
        input_tokens: 1234,
        output_tokens: 56,
        ..Default::default()
    };
    let mut agent = agent(StubBackend::default(), vec![completion]);
    let mut approvals = AutoApprove::yes();
    let (tx, mut rx) = unbounded_channel();

    agent.run_turn("hi", &mut approvals, Some(&tx)).await.unwrap();

    let mut saw_usage = false;
    while let Ok(ev) = rx.try_recv() {
        if let AgentEvent::Usage { input_tokens, output_tokens, .. } = ev {
            assert_eq!((input_tokens, output_tokens), (1234, 56));
            saw_usage = true;
        }
    }
    assert!(saw_usage, "a Usage event is emitted per completion");
}

#[tokio::test]
async fn assistant_prose_streams_as_deltas_then_one_final_text() {
    // The loop drives the provider's stream: a turn's prose arrives as incremental
    // AssistantDelta events (concatenating to the full text) and is finalized once
    // as a single AssistantText.
    use tokio::sync::mpsc::unbounded_channel;
    let mut agent = agent(StubBackend::default(), vec![final_text("hello there, world")]);
    let mut approvals = AutoApprove::yes();
    let (tx, mut rx) = unbounded_channel();

    let out = agent.run_turn("go", &mut approvals, Some(&tx)).await.unwrap();
    assert_eq!(out.final_text, "hello there, world");

    let mut deltas = Vec::new();
    let mut finals = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        match ev {
            AgentEvent::AssistantDelta(t) => deltas.push(t),
            AgentEvent::AssistantText(t) => finals.push(t),
            _ => {}
        }
    }
    assert!(deltas.len() >= 2, "prose streams as multiple deltas: {deltas:?}");
    assert_eq!(deltas.concat(), "hello there, world", "deltas reassemble the text");
    assert_eq!(finals, vec!["hello there, world".to_string()], "finalized exactly once");
}

#[tokio::test]
async fn applied_event_carries_the_domain_summary() {
    use tokio::sync::mpsc::unbounded_channel;
    let mut agent = agent(
        StubBackend::default(),
        vec![
            tool_call("t1", "apply_design", json!({ "commit": true })),
            final_text("done"),
        ],
    );
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

/// Drain a turn's events into the list of `Reviewed { round, defects }` it emitted.
fn reviewed_rounds(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<AgentEvent>,
) -> Vec<(usize, Vec<String>)> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let AgentEvent::Reviewed { round, defects, .. } = ev {
            out.push((round, defects));
        }
    }
    out
}

#[tokio::test]
async fn reviewed_turn_feeds_a_defect_into_a_fix_turn_then_re_reviews_clean() {
    use tokio::sync::mpsc::unbounded_channel;

    // Round 0 review finds a defect → one fix turn → round 1 review is clean.
    let reviews = Arc::new(Mutex::new(VecDeque::from(vec![
        ReviewOutcome { score: 6.0, defects: vec!["R1 has no pulldown".into()] },
        ReviewOutcome { score: 9.0, defects: vec![] },
    ])));
    let backend = StubBackend { reviews: Arc::clone(&reviews), ..Default::default() };
    let mut agent = agent(
        backend,
        vec![
            // Turn 1: author + commit.
            tool_call("t1", "apply_design", json!({ "commit": true })),
            final_text("first draft committed"),
            // Fix turn (driven by the round-0 defect): re-commit.
            tool_call("t2", "apply_design", json!({ "commit": true })),
            final_text("defect fixed"),
        ],
    );
    let mut approvals = AutoApprove::yes();
    let (tx, mut rx) = unbounded_channel();

    let out = agent
        .run_turn_reviewed("add a button", "add a button", &mut approvals, Some(&tx), 1)
        .await
        .unwrap();

    // The loop ended on the fix turn's outcome.
    assert!(out.applied);
    assert_eq!(out.final_text, "defect fixed");
    // Both review rounds were consumed (round 0 found a defect, round 1 clean).
    assert!(reviews.lock().unwrap().is_empty(), "both scripted reviews consumed");

    let rounds = reviewed_rounds(&mut rx);
    assert_eq!(
        rounds,
        vec![(0, vec!["R1 has no pulldown".to_string()]), (1, vec![])],
        "round 0 emits the defect, round 1 emits the clean re-review: {rounds:?}"
    );
}

#[tokio::test]
async fn reviewed_turn_with_a_clean_first_review_runs_no_fix_turn() {
    use tokio::sync::mpsc::unbounded_channel;

    // A single clean review: one Reviewed event, no fix turn (the script has no
    // extra completions, so a spurious fix turn would exhaust it and panic).
    let reviews =
        Arc::new(Mutex::new(VecDeque::from(vec![ReviewOutcome { score: 10.0, defects: vec![] }])));
    let backend = StubBackend { reviews: Arc::clone(&reviews), ..Default::default() };
    let mut agent = agent(
        backend,
        vec![
            tool_call("t1", "apply_design", json!({ "commit": true })),
            final_text("committed"),
        ],
    );
    let mut approvals = AutoApprove::yes();
    let (tx, mut rx) = unbounded_channel();

    let out = agent
        .run_turn_reviewed("make it", "make it", &mut approvals, Some(&tx), 1)
        .await
        .unwrap();

    assert!(out.applied);
    assert_eq!(out.final_text, "committed");
    let rounds = reviewed_rounds(&mut rx);
    assert_eq!(rounds, vec![(0, vec![])], "one clean review, no fix turn: {rounds:?}");
}

#[tokio::test]
async fn reviewed_turn_skips_review_when_nothing_committed() {
    use tokio::sync::mpsc::unbounded_channel;

    // A read-only/conversational turn: no commit, so `review_committed` is never
    // consulted (the scripted review stays queued) and no Reviewed event fires —
    // the gate that keeps us from paying a reviewer call every turn.
    let reviews = Arc::new(Mutex::new(VecDeque::from(vec![ReviewOutcome {
        score: 1.0,
        defects: vec!["should never surface".into()],
    }])));
    let backend = StubBackend { reviews: Arc::clone(&reviews), ..Default::default() };
    // A pure-text answer: no authoring tool runs, so the turn commits nothing and
    // is not nudged.
    let mut agent = agent(backend, vec![final_text("here's what i found")]);
    let mut approvals = AutoApprove::yes();
    let (tx, mut rx) = unbounded_channel();

    let out = agent
        .run_turn_reviewed("what parts?", "what parts?", &mut approvals, Some(&tx), 1)
        .await
        .unwrap();

    assert!(!out.applied, "a read-only turn commits nothing");
    assert!(reviewed_rounds(&mut rx).is_empty(), "no review on a non-applied turn");
    assert_eq!(reviews.lock().unwrap().len(), 1, "the scripted review was never consumed");
}
