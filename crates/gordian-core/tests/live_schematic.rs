//! The live-schematic tool surface, driven through the agent loop with a
//! SCRIPTED client — no network, real KiCAD tools, real `.kicad_sch` on disk.
//!
//! Skips when no KiCAD is installed: the mutators embed library definitions and
//! `check_schematic` shells out to `kicad-cli`.

use gordian_core::AgentRuntime;
use gordian_core::prompts::system_prompt;
use gordian_core::testing::{ScriptedClient, final_text, tool_call};
use gordian_core::{Agent, AutoApprove};
use serde_json::json;

#[tokio::test]
async fn the_loop_reads_swaps_and_checks_a_live_schematic() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let sch_path = ctx.sch_path().to_path_buf();

    let script = vec![
        tool_call(
            "t1",
            "place_parts",
            json!({"parts": [
                {"ref": "R1", "part": "Device:R", "value": "10k", "pins": {"1": "VCC", "2": "MID"}},
                {"ref": "R2", "part": "Device:R", "value": "10k", "pins": {"1": "MID", "2": "GND"}}
            ]}),
        ),
        tool_call("t2", "read_schematic", json!({})),
        tool_call(
            "t3",
            "swap_symbol",
            json!({"ref": "R1", "lib_id": "Device:R_Small"}),
        ),
        tool_call("t4", "check_schematic", json!({})),
        final_text("done"),
    ];
    let mut agent = Agent::new(ScriptedClient::new(script), ctx, system_prompt());
    let outcome = agent
        .run_turn(
            "wire two resistors, then swap R1",
            &mut AutoApprove::yes(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(outcome.tool_calls_made, 4, "{outcome:?}");

    let text = std::fs::read_to_string(&sch_path).unwrap();
    assert!(
        text.contains("Device:R_Small"),
        "the swap must land in the file"
    );
    assert!(text.contains("\"R2\""), "the untouched part must survive");
    assert!(
        text.contains("(wire"),
        "the connection must survive the swap"
    );
}

/// Moving a connected symbol must preserve the rest of the live circuit.
#[tokio::test]
async fn moving_a_symbol_preserves_connectivity() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let sch_path = ctx.sch_path().to_path_buf();

    let script = vec![
        tool_call(
            "t1",
            "place_parts",
            json!({"parts": [
                {"ref": "R1", "part": "Device:R", "pins": {"1": "VCC", "2": "MID"}},
                {"ref": "R2", "part": "Device:R", "pins": {"1": "MID", "2": "GND"}}
            ]}),
        ),
        tool_call(
            "t2",
            "move_symbols",
            json!({"moves": [{"ref": "R2", "by": [0.0, 25.4]}]}),
        ),
        tool_call("t3", "check_schematic", json!({})),
        final_text("done"),
    ];
    let mut agent = Agent::new(ScriptedClient::new(script), ctx, system_prompt());
    agent
        .run_turn("move R2 away", &mut AutoApprove::yes(), None)
        .await
        .unwrap();

    let text = std::fs::read_to_string(&sch_path).unwrap();
    assert!(text.contains("\"R1\""), "the other part must survive");
    assert!(text.contains("\"R2\""), "the moved part must survive");
    assert!(
        text.contains("(wire"),
        "the connection must survive the move"
    );
}
