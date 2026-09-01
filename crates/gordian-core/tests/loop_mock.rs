//! Agent-loop tests with a SCRIPTED client (no network), driving the REAL KiCAD
//! tools via the KiCAD tools (a real `AgentRuntime`).
//!
//! [`ScriptedClient`] returns a fixed `Vec<StreamEnd>`, one per `complete()`
//! call in order, so the loop's control flow (tool dispatch → result feedback →
//! apply-gate → final text) is exercised deterministically. The real tools the
//! loop drives still need KiCAD (via [`AgentRuntime::detect_for_test`]); all tests
//! SKIP gracefully when no KiCAD is detected.

use gordian_core::AgentRuntime;
use gordian_core::prompts::system_prompt;
use gordian_core::testing::{ScriptedClient, final_text, tool_call};
use gordian_core::{Agent, AutoApprove, StopReason};

/// Build an agent over a [`AgentRuntime`] and a scripted client.
fn agent(ctx: AgentRuntime, completions: Vec<gordian_core::StreamEnd>) -> Agent<ScriptedClient> {
    Agent::new(ScriptedClient::new(completions), ctx, system_prompt())
}

/// A tiny, self-contained valid but ERC-dirty design: two resistors make GND a
/// shared net while A / B remain dangling endpoints. Uses the `Device:R` alias
/// (`R`) and the `between:` sugar.
const TINY_YAML: &str = "version: 1\n\
blocks:\n\
\x20 main:\n\
\x20   components:\n\
\x20     R1: {part: R, value: 10k, between: [A, GND]}\n\
\x20     R2: {part: R, value: 10k, between: [GND, B]}\n";

const CLEAN_YAML: &str = "version: 1\n\
blocks:\n\
\x20 main:\n\
\x20   components:\n\
\x20     R1: {part: R, value: 10k, between: [A, GND]}\n\
\x20     R2: {part: R, value: 10k, between: [A, GND]}\n";

/// The shared script: (1) search, (2) author draft, (3) apply, (4) done.
fn script() -> Vec<gordian_core::StreamEnd> {
    vec![
        tool_call(
            "tu_1",
            "search_symbols",
            serde_json::json!({ "query": "resistor" }),
        ),
        tool_call(
            "tu_2",
            "create_design",
            serde_json::json!({ "yaml": CLEAN_YAML }),
        ),
        tool_call("tu_3", "apply_design", serde_json::json!({})),
        final_text("done"),
    ]
}

#[tokio::test]
async fn loop_runs_tools_and_gates_apply_on_yes() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let sch_path = ctx.sch_path().to_path_buf();
    assert!(!sch_path.exists(), "fixture starts with no schematic");

    let mut agent = agent(ctx, script());
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
async fn clean_draft_flow_skips_redundant_validation_and_erc_calls() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let sch_path = ctx.sch_path().to_path_buf();
    let script = vec![
        tool_call(
            "tu_1",
            "create_design",
            serde_json::json!({ "yaml": CLEAN_YAML }),
        ),
        tool_call("tu_2", "apply_design", serde_json::json!({})),
        final_text("done"),
    ];
    let mut agent = agent(ctx, script);
    let mut approvals = AutoApprove::yes();

    let outcome = agent
        .run_turn(
            "create and commit this small resistor design",
            &mut approvals,
            None,
        )
        .await
        .unwrap();

    assert!(outcome.applied, "clean draft should commit: {outcome:?}");
    assert!(
        sch_path.exists(),
        "approved apply should write the schematic"
    );
    assert_eq!(
        outcome.tool_calls_made, 2,
        "create_design already validates and apply_design already runs ERC"
    );
}

#[tokio::test]
async fn stall_after_draft_authoring_is_nudged_until_it_commits() {
    // Once a model has WRITTEN A DRAFT, a text-only stop ships nothing. The loop
    // must re-prompt it to finish + commit so the authored work lands.
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let sch_path = ctx.sch_path().to_path_buf();
    assert!(!sch_path.exists(), "fixture starts with no schematic");

    // (1) author draft, (2) premature text-only stop → NUDGE,
    // (3) apply+commit, (4) done.
    let script = vec![
        tool_call(
            "tu_1",
            "create_design",
            serde_json::json!({ "yaml": CLEAN_YAML }),
        ),
        final_text("I created the draft."), // stalls without committing
        tool_call("tu_2", "apply_design", serde_json::json!({})),
        final_text("done"),
    ];
    let mut agent = agent(ctx, script);
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

/// A commit whose ERC is dirty is nudged, and the repair happens on the
/// committed schematic — not by re-applying a draft over it.
#[tokio::test]
async fn dirty_commit_is_nudged_into_repairing_the_schematic() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let script = vec![
        tool_call(
            "tu_1",
            "create_design",
            serde_json::json!({ "yaml": TINY_YAML }),
        ),
        tool_call("tu_2", "apply_design", serde_json::json!({})),
        final_text("done, despite dangling endpoints"), // → ERC cleanup nudge
        // A and B are single-pin nets, so the honest repair is to tie them
        // together on the committed schematic.
        tool_call(
            "tu_3",
            "connect",
            serde_json::json!({ "from": "R1.1", "to": "R2.2", "net": "SENSE" }),
        ),
        tool_call("tu_4", "check_schematic", serde_json::json!({})),
        // The ERC nudge is bounded; the loop gives up rather than looping.
        final_text("clean now"),
        final_text("clean now"),
        final_text("clean now"),
    ];
    let mut agent = agent(ctx, script);
    let mut approvals = AutoApprove::yes();

    let outcome = agent
        .run_turn(
            "commit, inspect ERC, and clean up the design",
            &mut approvals,
            None,
        )
        .await
        .unwrap();

    assert!(outcome.applied, "the apply should commit: {outcome:?}");
    assert_eq!(outcome.final_text, "clean now");
    assert_eq!(outcome.tool_calls_made, 4);
}

/// Once the schematic has been edited in place, the draft that produced it is
/// history: re-applying it would silently revert the edit, so it is refused.
#[tokio::test]
async fn applying_a_draft_over_an_edited_schematic_is_refused() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let sch_path = ctx.sch_path().to_path_buf();
    let script = vec![
        tool_call(
            "tu_1",
            "create_design",
            serde_json::json!({"yaml": CLEAN_YAML}),
        ),
        tool_call("tu_2", "apply_design", serde_json::json!({})),
        tool_call(
            "tu_3",
            "set_fields",
            serde_json::json!({"ref": "R1", "fields": {"Value": "12k"}}),
        ),
        tool_call("tu_4", "apply_design", serde_json::json!({})),
        final_text("kept the live edit"),
    ];
    let mut agent = agent(ctx, script);
    let mut approvals = AutoApprove::yes();

    agent
        .run_turn("commit, then change a value", &mut approvals, None)
        .await
        .unwrap();

    let text = std::fs::read_to_string(&sch_path).unwrap();
    assert!(text.contains("12k"), "the live edit must survive the re-apply");
}

#[tokio::test]
async fn clean_draft_stall_is_nudged_to_commit_before_watchdog_stop() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let script = vec![
        tool_call(
            "create",
            "create_design",
            serde_json::json!({"yaml": CLEAN_YAML}),
        ),
        tool_call("inspect-1", "project_info", serde_json::json!({})),
        tool_call("inspect-2", "project_info", serde_json::json!({})),
        tool_call("inspect-3", "project_info", serde_json::json!({})),
        tool_call("apply", "apply_design", serde_json::json!({})),
        final_text("committed after the bounded nudge"),
    ];
    let mut agent = agent(ctx, script);
    let mut approvals = AutoApprove::yes();

    let outcome = agent
        .run_turn("author and commit the design", &mut approvals, None)
        .await
        .unwrap();

    assert_eq!(outcome.stop_reason, StopReason::Completed);
    assert!(outcome.applied);
    assert_eq!(outcome.final_text, "committed after the bounded nudge");
}

#[tokio::test]
async fn discovery_stall_is_nudged_into_full_authoring_once() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let script = vec![
        tool_call(
            "discover",
            "search_symbols",
            serde_json::json!({"query": "resistor"}),
        ),
        tool_call("inspect-1", "project_info", serde_json::json!({})),
        tool_call("inspect-2", "project_info", serde_json::json!({})),
        tool_call("inspect-3", "project_info", serde_json::json!({})),
        tool_call(
            "author",
            "create_design",
            serde_json::json!({"yaml": CLEAN_YAML}),
        ),
        tool_call("apply", "apply_design", serde_json::json!({})),
        final_text("authored after the bounded transition nudge"),
    ];
    let mut agent = agent(ctx, script);
    let mut approvals = AutoApprove::yes();

    let outcome = agent
        .run_turn("discover, then author the circuit", &mut approvals, None)
        .await
        .unwrap();

    assert_eq!(outcome.stop_reason, StopReason::Completed);
    assert!(outcome.applied);
    assert_eq!(
        outcome.final_text,
        "authored after the bounded transition nudge"
    );
}

#[tokio::test]
async fn stall_nudge_is_bounded_and_gives_up() {
    // A model that simply will NOT commit (drafts, then stops repeatedly) must
    // not loop forever: at most MAX_COMMIT_NUDGES (2) re-prompts, then the turn
    // returns honestly unapplied. The script ends after the 3rd stop; if the loop
    // nudged a 3rd time it would exhaust the script and error.
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };

    let script = vec![
        tool_call(
            "tu_1",
            "create_design",
            serde_json::json!({ "yaml": TINY_YAML }),
        ),
        final_text("stop 1"), // → nudge 1
        final_text("stop 2"), // → nudge 2
        final_text("stop 3"), // nudges exhausted → return
    ];
    let mut agent = agent(ctx, script);
    let mut approvals = AutoApprove::yes();

    let outcome = agent
        .run_turn("add a 10k resistor between A and GND", &mut approvals, None)
        .await
        .expect("must terminate, not loop forever / exhaust the script");

    assert!(!outcome.applied, "model never committed: {outcome:?}");
    assert_eq!(outcome.final_text, "stop 3", "returns the last stop's text");
}

#[tokio::test]
async fn read_only_symbol_research_does_not_trigger_commit_nudges() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    // The script deliberately has no spare completion. An erroneous commit
    // nudge would request a third response and exhaust it.
    let script = vec![
        tool_call(
            "tu_1",
            "search_symbols",
            serde_json::json!({ "query": "resistor" }),
        ),
        final_text("I found the matching resistor symbols."),
    ];
    let mut agent = agent(ctx, script);
    let mut approvals = AutoApprove::yes();

    let outcome = agent
        .run_turn("find resistor symbols", &mut approvals, None)
        .await
        .expect("read-only research should finish without a commit nudge");

    assert!(!outcome.applied);
    assert_eq!(outcome.final_text, "I found the matching resistor symbols.");
    assert_eq!(outcome.tool_calls_made, 1);
}

#[tokio::test]
async fn loop_rejects_apply_on_no_and_does_not_write() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let sch_path = ctx.sch_path().to_path_buf();
    assert!(!sch_path.exists(), "fixture starts with no schematic");

    let mut agent = agent(ctx, script());
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
