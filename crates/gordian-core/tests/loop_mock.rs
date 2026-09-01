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
