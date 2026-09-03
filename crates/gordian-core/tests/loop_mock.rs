//! Completion-contract coverage with a scripted provider and real schematic tools.

use gordian_core::prompts::system_prompt;
use gordian_core::testing::{ScriptedClient, final_text, tool_call};
use gordian_core::{Agent, AgentRuntime, StopReason};
use serde_json::json;

fn place_two_resistors() -> gordian_core::StreamEnd {
    tool_call(
        "place",
        "place_parts",
        json!({
            "parts": [
                {"ref": "R1", "part": "Device:R", "value": "10k", "pins": {"1": "VCC", "2": "GND"}},
                {"ref": "R2", "part": "Device:R", "value": "10k", "pins": {"1": "VCC", "2": "GND"}}
            ]
        }),
    )
}

#[tokio::test]
async fn clean_check_followed_by_final_text_completes_without_a_quality_nudge() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let (client, seen) = ScriptedClient::recording(vec![
        place_two_resistors(),
        tool_call("check", "check_schematic", json!({})),
        final_text("done"),
    ]);
    let mut agent = Agent::new(client, ctx, system_prompt());

    let outcome = agent
        .run_turn("create a two-resistor divider", None)
        .await
        .unwrap();

    assert_eq!(outcome.stop_reason, StopReason::Completed);
    assert_eq!(outcome.tool_calls_made, 2);
    assert_eq!(seen.lock().unwrap().len(), 3, "no extra model request");
}

#[tokio::test]
async fn reviewed_turn_uses_check_schematic_without_a_reviewer_model_call() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let (client, seen) = ScriptedClient::recording(vec![
        place_two_resistors(),
        tool_call("check", "check_schematic", json!({})),
        final_text("done"),
    ]);
    let mut agent = Agent::new(client, ctx, system_prompt());

    let outcome = agent
        .run_turn_reviewed(
            "create a two-resistor divider",
            "create a two-resistor divider",
            None,
            1,
        )
        .await
        .unwrap();

    assert_eq!(outcome.stop_reason, StopReason::Completed);
    assert_eq!(seen.lock().unwrap().len(), 3, "review used no VLM request");
}

#[tokio::test]
async fn unchanged_tool_cycles_reach_the_provider_request_limit() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let script = (0..56)
        .map(|index| tool_call(&format!("read-{index}"), "project_info", json!({})))
        .collect();
    let (client, seen) = ScriptedClient::recording(script);
    let mut agent = Agent::new(client, ctx, system_prompt());

    let outcome = agent
        .run_turn("inspect the project repeatedly", None)
        .await
        .unwrap();

    assert_eq!(
        outcome.stop_reason,
        StopReason::ProviderRequestLimit { requests: 56 }
    );
    assert_eq!(outcome.tool_calls_made, 56);
    assert_eq!(seen.lock().unwrap().len(), 56);
}

/// The request ceiling belongs to the whole turn, not to whichever subturn is
/// running. A turn is the model's own work plus every review round, and the
/// five-minute promise is made about all of them together — a per-subturn budget
/// silently multiplied by the number of review rounds.
#[tokio::test]
async fn the_request_ceiling_spans_a_whole_reviewed_turn() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let mut script = vec![place_two_resistors()];
    script.extend(
        (0..80).map(|index| tool_call(&format!("read-{index}"), "project_info", json!({}))),
    );
    let (client, seen) = ScriptedClient::recording(script);
    let mut agent = Agent::new(client, ctx, system_prompt());

    let outcome = agent
        .run_turn_reviewed("create a divider", "create a divider", None, 2)
        .await
        .unwrap();

    assert!(
        matches!(outcome.stop_reason, StopReason::ProviderRequestLimit { .. }),
        "{:?}",
        outcome.stop_reason
    );
    let spent = seen.lock().unwrap().len();
    assert!(
        spent <= 56,
        "a reviewed turn spent {spent} requests against a ceiling of 56"
    );
}

/// A cut-off turn keeps the last legal partial write for the next turn to continue.
#[tokio::test]
async fn a_turn_cut_off_mid_edit_keeps_the_partial_schematic() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let sch_path = ctx.sch_path().to_path_buf();
    let mut script = vec![
        place_two_resistors(),
        tool_call("check", "check_schematic", json!({})),
        // The teardown half of a repair, and then nothing puts it back.
        tool_call("tear", "remove_symbols", json!({"refs": ["R2"]})),
    ];
    script.extend(
        (0..80).map(|index| tool_call(&format!("spin-{index}"), "project_info", json!({}))),
    );
    let (client, _) = ScriptedClient::recording(script);
    let mut agent = Agent::new(client, ctx, system_prompt());

    let outcome = agent.run_turn("build a divider", None).await.unwrap();
    assert!(
        matches!(outcome.stop_reason, StopReason::ProviderRequestLimit { .. }),
        "{:?}",
        outcome.stop_reason
    );

    let after = std::fs::read_to_string(&sch_path).unwrap();
    assert!(
        !after.contains("R2"),
        "the last legal partial edit should remain for the next turn"
    );
}
