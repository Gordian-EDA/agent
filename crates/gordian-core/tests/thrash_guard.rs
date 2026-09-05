//! The loop refuses an edit cycle instead of letting the model undo its own work.

use gordian_core::prompts::system_prompt;
use gordian_core::testing::{ScriptedClient, final_text, tool_call};
use gordian_core::{Agent, AgentEvent, AgentRuntime, StopReason};
use serde_json::{Value, json};
use tokio::sync::mpsc;

/// Run a scripted turn and return one `(tool, refused, result)` row per tool call.
async fn scripted(
    ctx: AgentRuntime,
    instruction: &str,
    mut script: Vec<gordian_core::StreamEnd>,
) -> Vec<(String, bool, Value)> {
    script.extend((0..4).map(|_| final_text("done")));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut agent = Agent::new(ScriptedClient::new(script), ctx, system_prompt());
    let outcome = agent.run_turn(instruction, Some(&tx)).await.unwrap();
    drop(agent);
    assert_eq!(outcome.stop_reason, StopReason::Completed);
    let mut calls = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let AgentEvent::ToolFinished { name, result, .. } = event {
            let refused = result.get("error").and_then(Value::as_str) == Some("edit loop refused");
            calls.push((name, refused, result));
        }
    }
    calls
}

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

/// v4 blue-pill #45/#46/#48: the model removes the user LED the prompt asked
/// for, puts it back inside a whole-block `add_parts`, and removes it again. The
/// re-add never names the same argument set as the removal, so only counting the
/// ref's presence flips catches it.
#[tokio::test]
async fn a_block_shaped_restore_shares_the_removal_budget_of_the_part_it_restores() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let leds = json!({"block": "LEDS", "parts": [
        {"ref": "R4", "part": "Device:R", "value": "1k", "pins": {"1": "+3V3", "2": "LED_A"}},
        {"ref": "D2", "part": "Device:LED", "pins": {"1": "LED_A", "2": "GND"}}
    ]});
    let calls = scripted(
        ctx,
        "add a power LED",
        vec![
            tool_call("place", "place_parts", leds.clone()),
            tool_call("cut-1", "remove_symbols", json!({"refs": ["D2"]})),
            tool_call("restore", "add_parts", leds),
            tool_call("cut-2", "remove_symbols", json!({"refs": ["D2"]})),
        ],
    )
    .await;

    let refusals: Vec<_> = calls.iter().filter(|(_, refused, _)| *refused).collect();
    assert_eq!(refusals.len(), 1, "{calls:#?}");
    assert_eq!(refusals[0].0, "remove_symbols");
    assert!(refusals[0].2["note"].as_str().unwrap().contains("D2"));
    assert!(
        calls
            .iter()
            .any(|(name, refused, _)| name == "add_parts" && !refused),
        "the restore itself is never refused"
    );
}

/// v4 blue-pill #64/#65/#70/#100/#117/#125: six removals of `#PWR`/`#FLG`
/// symbols, each naming a fresh set of KiCAD-minted references, stripped the
/// sheet of its rails. They share one budget now. The guard runs before dispatch,
/// so what the underlying tool would have said about these names is beside the
/// point — the third purge never reaches it.
#[tokio::test]
async fn a_third_purge_of_power_furniture_is_refused_whatever_it_names() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let calls = scripted(
        ctx,
        "clean up the rails",
        vec![
            tool_call("purge-1", "remove_symbols", json!({"refs": ["#FLG1"]})),
            tool_call(
                "purge-2",
                "remove_symbols",
                json!({"refs": ["#PWR_GND_0_4", "#PWR_GND_0_5"]}),
            ),
            tool_call(
                "purge-3",
                "remove_symbols",
                json!({"refs": ["#PWR_+5V_2", "#PWR_GND"]}),
            ),
        ],
    )
    .await;

    let refusals: Vec<_> = calls.iter().filter(|(_, refused, _)| *refused).collect();
    assert_eq!(refusals.len(), 1, "{calls:#?}");
    assert!(
        refusals[0].2["note"]
            .as_str()
            .unwrap()
            .contains("power symbols and flags"),
        "{:#?}",
        refusals[0].2
    );
}
