//! Completion-contract coverage with a scripted provider and real schematic tools.

use gordian_core::prompts::system_prompt;
use gordian_core::testing::{ScriptedClient, final_text, tool_call};
use gordian_core::{Agent, AgentRuntime, ContentPart, MessageContent, StopReason};
use serde_json::json;

fn text_of(content: &MessageContent) -> String {
    content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

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
async fn undone_turn_gets_one_explicit_second_chance() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let sch_path = ctx.sch_path().to_path_buf();
    let (client, seen) = ScriptedClient::recording(vec![
        place_two_resistors(),
        tool_call("seed-check", "check_schematic", json!({})),
        final_text("seeded"),
        tool_call(
            "first-edit",
            "set_fields",
            json!({"ref": "R1", "fields": {"Value": "47k"}}),
        ),
        tool_call("undo", "undo", json!({"snapshot": "sch-2"})),
        tool_call("undo-check", "check_schematic", json!({})),
        final_text("done"),
        tool_call(
            "second-edit",
            "set_fields",
            json!({"ref": "R1", "fields": {"Value": "22k"}}),
        ),
        tool_call("final-check", "check_schematic", json!({})),
        final_text("done for real"),
    ]);
    let mut agent = Agent::new(client, ctx, system_prompt());

    agent
        .run_turn("create a two-resistor divider", None)
        .await
        .unwrap();
    let baseline = std::fs::read(&sch_path).unwrap();
    let outcome = agent
        .run_turn("change R1 to 22k", None)
        .await
        .unwrap();

    assert_eq!(outcome.stop_reason, StopReason::Completed);
    assert_eq!(outcome.tool_calls_made, 5);
    let seen = seen.lock().unwrap();
    let second_chance = seen.get(7).expect("feedback triggered another request");
    let feedback = second_chance
        .iter()
        .rev()
        .find(|message| text_of(&message.content).contains("schematic is unchanged"))
        .expect("unchanged-schematic feedback reached the model");
    let feedback = text_of(&feedback.content);
    assert!(feedback.contains("request is not satisfied"), "{feedback}");
    assert!(feedback.contains("`set_fields`"), "{feedback}");
    assert!(feedback.contains("cannot be done and why"), "{feedback}");
    let final_bytes = std::fs::read(&sch_path).unwrap();
    assert_ne!(final_bytes, baseline, "the second chance must change the file");
    assert!(
        String::from_utf8(final_bytes).unwrap().contains("22k"),
        "the second edit must be dispatched after the clean-check lock is reopened"
    );
}

#[tokio::test]
async fn unchanged_tool_cycles_reach_the_provider_request_limit() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let script = (0..32)
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
        StopReason::ProviderRequestLimit { requests: 32 }
    );
    assert_eq!(outcome.tool_calls_made, 32);
    assert_eq!(seen.lock().unwrap().len(), 32);
}
