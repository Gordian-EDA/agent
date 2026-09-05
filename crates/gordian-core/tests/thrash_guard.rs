//! The loop refuses an edit cycle instead of letting the model undo its own work.

use gordian_core::prompts::system_prompt;
use gordian_core::testing::{ScriptedClient, final_text, tool_call};
use gordian_core::{Agent, AgentEvent, AgentRuntime, StopReason};
use serde_json::{Value, json};
use tokio::sync::mpsc;

fn place_bridge(cycle: usize) -> gordian_core::StreamEnd {
    tool_call(
        &format!("place-{cycle}"),
        "place_parts",
        json!({
            "parts": [
                {"ref": "U4", "part": "Device:R", "value": "CH340C", "pins": {"1": "USB_DP"}},
                {"ref": "C12", "part": "Device:C", "value": "100n", "pins": {"1": "+5V", "2": "GND"}}
            ]
        }),
    )
}

fn remove_bridge(cycle: usize) -> gordian_core::StreamEnd {
    tool_call(
        &format!("cut-{cycle}"),
        "remove_symbols",
        json!({"refs": ["U4", "C12"]}),
    )
}

/// Reproduces the arduino loop: `check_schematic` reports a finding with no
/// repair, the model deletes the parts the request named to make it go away,
/// then places them back, three times over. Nothing errors, so nothing stops it
/// — until the guard refuses the third application and says so once.
#[tokio::test]
async fn a_third_remove_and_place_cycle_is_refused() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let mut script = Vec::new();
    for cycle in 0..3 {
        script.push(place_bridge(cycle));
        script.push(remove_bridge(cycle));
    }
    script.extend((0..4).map(|_| final_text("done: the bridge is on the sheet")));

    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut agent = Agent::new(ScriptedClient::new(script), ctx, system_prompt());
    let outcome = agent
        .run_turn("place a USB-serial bridge", Some(&tx))
        .await
        .unwrap();
    drop(agent);

    assert_eq!(outcome.stop_reason, StopReason::Completed);

    let mut calls = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let AgentEvent::ToolFinished { name, result, .. } = event {
            let refused = result.get("error").and_then(Value::as_str) == Some("edit loop refused");
            calls.push((name, refused, result));
        }
    }

    let refusals: Vec<_> = calls.iter().filter(|(_, refused, _)| *refused).collect();
    assert_eq!(
        refusals.len(),
        2,
        "both later removals are refused; the restores in between still run"
    );
    let (first_refused, _, result) = refusals[0];
    assert_eq!(
        first_refused, "remove_symbols",
        "the second removal — the one that would strand the requested parts — never runs"
    );
    let note = result["note"].as_str().unwrap();
    assert!(note.contains("U4") && note.contains("C12"), "{note}");
    assert!(note.contains("was NOT applied"), "{note}");

    let dispatched_removals = calls
        .iter()
        .filter(|(name, refused, _)| name == "remove_symbols" && !refused)
        .count();
    assert_eq!(
        dispatched_removals, 1,
        "only the first removal touched the sheet"
    );
}
