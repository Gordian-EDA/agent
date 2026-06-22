//! Agent-loop tests with a SCRIPTED mock [`LlmClient`] (no network).
//!
//! The mock returns a fixed `Vec<Completion>`, one per `complete()` call in
//! order, so the loop's control flow (tool dispatch → result feedback →
//! apply-gate → final text) is exercised deterministically. The real tools the
//! loop drives still need KiCAD (via [`ToolCtx::detect_for_test`]); both tests
//! SKIP gracefully when no KiCAD is detected.

use std::sync::Mutex;

use agent::llm::{Completion, LlmClient, Message, ToolCall, ToolDef};
use agent::tools::ToolCtx;
use agent::{Agent, AutoApprove};
use anyhow::Result;
use async_trait::async_trait;

/// A mock LLM that replays a fixed script of completions, one per call.
///
/// Each `complete()` pops the next scripted [`Completion`]. Running off the end
/// of the script is a hard error (the loop asked for more than the test scripted
/// — usually a bug in the loop).
struct MockLlm {
    script: Mutex<std::collections::VecDeque<Completion>>,
}

impl MockLlm {
    fn script(completions: Vec<Completion>) -> Self {
        Self {
            script: Mutex::new(completions.into_iter().collect()),
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
        self.script.lock().unwrap().pop_front().ok_or_else(|| {
            anyhow::anyhow!(
                "MockLlm script exhausted: loop called complete() more times than scripted"
            )
        })
    }
}

/// A tiny, self-contained valid design: two resistors so that GND has 2 pins and
/// A / B are single-pin endpoints. Uses the `Device:R` alias (`R`) and the
/// `between:` sugar. Compiles + emits cleanly against real libraries.
const TINY_YAML: &str = "version: 1\n\
blocks:\n\
\x20 main:\n\
\x20   components:\n\
\x20     R1: {part: R, value: 10k, between: [A, GND]}\n\
\x20     R2: {part: R, value: 10k, between: [GND, B]}\n";

/// Build a completion carrying a single tool call (no text).
fn tool_call(id: &str, name: &str, input: serde_json::Value) -> Completion {
    Completion {
        text: String::new(),
        tool_calls: vec![ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            input,
        }],
        stop_reason: "tool_use".to_string(),
        ..Default::default()
    }
}

/// Build a final text completion (end of turn, no tool calls).
fn final_text(text: &str) -> Completion {
    Completion {
        text: text.to_string(),
        tool_calls: Vec::new(),
        stop_reason: "end_turn".to_string(),
        ..Default::default()
    }
}

/// The shared script: (1) search_symbols, (2) apply_design{commit:true}, (3) done.
fn script() -> Vec<Completion> {
    vec![
        tool_call(
            "tu_1",
            "search_symbols",
            serde_json::json!({ "query": "resistor" }),
        ),
        tool_call(
            "tu_2",
            "apply_design",
            serde_json::json!({ "yaml": TINY_YAML, "commit": true }),
        ),
        final_text("done"),
    ]
}

#[tokio::test]
async fn loop_runs_tools_and_gates_apply_on_yes() {
    let Some(ctx) = ToolCtx::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let sch_path = ctx.sch_path().to_path_buf();
    assert!(!sch_path.exists(), "fixture starts with no schematic");

    let mock = MockLlm::script(script());
    let mut agent = Agent::new(Box::new(mock), ctx);
    let mut approvals = AutoApprove::yes();

    let outcome = agent
        .run_turn("add a 10k resistor between A and GND", &mut approvals, None)
        .await
        .unwrap();

    assert!(
        outcome.applied,
        "approve=yes must commit the write: {outcome:?}"
    );
    assert!(
        sch_path.exists(),
        "approved apply must write the .kicad_sch: {outcome:?}"
    );
    assert!(
        outcome.tool_calls_made >= 2,
        "expected at least the search + apply tool calls, got {}",
        outcome.tool_calls_made
    );
    assert_eq!(outcome.final_text, "done", "final text should pass through");
}

#[tokio::test]
async fn stall_after_research_is_nudged_until_it_commits() {
    // The bug: a model that RESEARCHES (search_symbols) then tries to stop with
    // a text-only turn ships NOTHING (applied stays false). The loop must
    // re-prompt it to finish + commit, so it lands the design instead.
    let Some(ctx) = ToolCtx::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let sch_path = ctx.sch_path().to_path_buf();
    assert!(!sch_path.exists(), "fixture starts with no schematic");

    // (1) research, (2) premature text-only stop → NUDGE, (3) apply+commit, (4) done.
    let script = vec![
        tool_call(
            "tu_1",
            "search_symbols",
            serde_json::json!({ "query": "resistor" }),
        ),
        final_text("I looked up the parts."), // stalls without committing
        tool_call(
            "tu_2",
            "apply_design",
            serde_json::json!({ "yaml": TINY_YAML, "commit": true }),
        ),
        final_text("done"),
    ];
    let mock = MockLlm::script(script);
    let mut agent = Agent::new(Box::new(mock), ctx);
    let mut approvals = AutoApprove::yes();

    let outcome = agent
        .run_turn("add a 10k resistor between A and GND", &mut approvals, None)
        .await
        .unwrap();

    assert!(
        outcome.applied,
        "the nudge must drive the stalled model to commit: {outcome:?}"
    );
    assert!(
        sch_path.exists(),
        "the post-nudge commit must write the .kicad_sch: {outcome:?}"
    );
    assert_eq!(outcome.final_text, "done");
}

#[tokio::test]
async fn stall_nudge_is_bounded_and_gives_up() {
    // A model that simply will NOT commit (research, then stop, repeatedly) must
    // not loop forever: at most MAX_COMMIT_NUDGES (2) re-prompts, then the turn
    // returns honestly unapplied. The script ends after the 3rd stop; if the loop
    // nudged a 3rd time it would exhaust the script and error.
    let Some(ctx) = ToolCtx::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };

    let script = vec![
        tool_call(
            "tu_1",
            "search_symbols",
            serde_json::json!({ "query": "resistor" }),
        ),
        final_text("stop 1"), // → nudge 1
        final_text("stop 2"), // → nudge 2
        final_text("stop 3"), // nudges exhausted → return
    ];
    let mock = MockLlm::script(script);
    let mut agent = Agent::new(Box::new(mock), ctx);
    let mut approvals = AutoApprove::yes();

    let outcome = agent
        .run_turn("add a 10k resistor between A and GND", &mut approvals, None)
        .await
        .expect("must terminate, not loop forever / exhaust the script");

    assert!(!outcome.applied, "model never committed: {outcome:?}");
    assert_eq!(outcome.final_text, "stop 3", "returns the last stop's text");
}

#[tokio::test]
async fn loop_rejects_apply_on_no_and_does_not_write() {
    let Some(ctx) = ToolCtx::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let sch_path = ctx.sch_path().to_path_buf();
    assert!(!sch_path.exists(), "fixture starts with no schematic");

    let mock = MockLlm::script(script());
    let mut agent = Agent::new(Box::new(mock), ctx);
    let mut approvals = AutoApprove::no();

    let outcome = agent
        .run_turn("add a 10k resistor between A and GND", &mut approvals, None)
        .await
        .unwrap();

    assert!(
        !outcome.applied,
        "approve=no must NOT report applied: {outcome:?}"
    );
    assert!(
        !sch_path.exists(),
        "rejected apply must NOT write the .kicad_sch: {outcome:?}"
    );
    // The loop still ran the tools and reached the final text.
    assert!(outcome.tool_calls_made >= 2, "tools still ran: {outcome:?}");
    assert_eq!(outcome.final_text, "done");
}
