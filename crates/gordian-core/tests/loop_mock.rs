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

/// The loop has no turn budget: it ends when the model stops asking for tools,
/// however long that takes. `reserve_refs` is neither a discovery call nor a
/// state-scoped read, so all 200 really dispatch.
#[tokio::test]
async fn a_long_tool_sequence_runs_to_completion() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let mut script: Vec<_> = (0..200)
        .map(|index| {
            tool_call(
                &format!("reserve-{index}"),
                "reserve_refs",
                json!({"prefix": "R", "count": 1}),
            )
        })
        .collect();
    script.push(final_text("inspected"));
    let (client, seen) = ScriptedClient::recording(script);
    let mut agent = Agent::new(client, ctx, system_prompt());

    let outcome = agent
        .run_turn("reserve references repeatedly", None)
        .await
        .unwrap();

    assert_eq!(outcome.stop_reason, StopReason::Completed);
    assert_eq!(outcome.final_text, "inspected");
    assert_eq!(outcome.tool_calls_made, 200);
    assert_eq!(seen.lock().unwrap().len(), 201);
}

/// The optional user-set cap belongs to the whole turn, not to whichever subturn
/// is running: a turn is the model's own work plus every review round. The first
/// subturn finishes on its own, so the requests the cap stops belong to the
/// review-driven fix subturn that follows it.
#[tokio::test]
async fn the_user_set_cap_spans_a_whole_reviewed_turn() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let mut script = vec![
        // A committed change with a dangling net, so the post-turn check finds a
        // defect and a fix subturn starts.
        tool_call(
            "place",
            "place_parts",
            json!({
                "parts": [
                    {"ref": "R1", "part": "Device:R", "value": "10k", "pins": {"1": "SIG", "2": "GND"}}
                ]
            }),
        ),
        final_text("placed"),
    ];
    script.extend((0..40).map(|index| {
        tool_call(
            &format!("reserve-{index}"),
            "reserve_refs",
            json!({"prefix": "R", "count": 1}),
        )
    }));
    let (client, seen) = ScriptedClient::recording(script);
    let mut agent = Agent::new(client, ctx, system_prompt());
    agent.set_max_requests(Some(8));

    let outcome = agent
        .run_turn_reviewed("place a resistor", "place a resistor", None, 2)
        .await
        .unwrap();

    assert_eq!(
        outcome.stop_reason,
        StopReason::MaxRequestsReached { requests: 8 }
    );
    assert!(
        outcome.final_text.contains("user-set cap"),
        "{}",
        outcome.final_text
    );
    let spent = seen.lock().unwrap().len();
    assert!(
        spent <= 8,
        "a reviewed turn spent {spent} against a cap of 8"
    );
    assert!(
        spent > 2,
        "the cap stopped the first subturn, not the review"
    );
}

/// A turn stopped at the user cap keeps the last legal partial write on disk.
#[tokio::test]
async fn a_turn_stopped_at_the_cap_keeps_the_partial_schematic() {
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
    agent.set_max_requests(Some(6));

    let outcome = agent.run_turn("build a divider", None).await.unwrap();
    assert_eq!(
        outcome.stop_reason,
        StopReason::MaxRequestsReached { requests: 6 }
    );

    let after = std::fs::read_to_string(&sch_path).unwrap();
    assert!(
        !after.contains("R2"),
        "the last legal partial edit should remain on disk"
    );
}
