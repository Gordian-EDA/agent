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

/// The smallest thing KiCAD calls a schematic: a sheet with nothing on it.
const EMPTY_SHEET: &str = "(kicad_sch\n\
\t(version 20250114)\n\
\t(generator \"eeschema\")\n\
\t(generator_version \"9.0\")\n\
\t(uuid \"4a1c0f2e-0000-4000-8000-0000000000aa\")\n\
\t(paper \"A4\")\n\
\t(lib_symbols)\n\
\t(sheet_instances\n\
\t\t(path \"/\"\n\
\t\t\t(page \"1\")\n\
\t\t)\n\
\t)\n\
)\n";

#[tokio::test]
async fn the_loop_reads_swaps_and_checks_a_live_schematic() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let sch_path = ctx.sch_path().to_path_buf();
    std::fs::write(&sch_path, EMPTY_SHEET).unwrap();

    // Two resistors wired together, then R1 retargeted at a different symbol.
    let script = vec![
        tool_call("t1", "add_symbol", json!({"lib_id": "Device:R", "ref": "R1", "value": "10k"})),
        tool_call(
            "t2",
            "add_symbol",
            json!({"lib_id": "Device:R", "ref": "R2", "value": "10k", "near": "R1", "side": "right"}),
        ),
        tool_call("t3", "connect", json!({"from": "R1.2", "to": "R2.1", "net": "MID"})),
        tool_call("t4", "read_schematic", json!({})),
        tool_call("t5", "swap_symbol", json!({"ref": "R1", "lib_id": "Device:R_Small"})),
        tool_call("t6", "check_schematic", json!({})),
        final_text("done"),
    ];
    let mut agent = Agent::new(ScriptedClient::new(script), ctx, system_prompt());
    let outcome = agent
        .run_turn("wire two resistors, then swap R1", &mut AutoApprove::yes(), None)
        .await
        .unwrap();
    assert_eq!(outcome.tool_calls_made, 6, "{outcome:?}");

    let text = std::fs::read_to_string(&sch_path).unwrap();
    assert!(text.contains("Device:R_Small"), "the swap must land in the file");
    assert!(text.contains("\"R2\""), "the untouched part must survive");
    assert!(text.contains("(wire"), "connect must have drawn copper");
    // MID names the node the swap carried across; both resistors still meet on it.
    assert!(text.contains("\"MID\""), "the named net must survive the swap");
}

/// A mutator that would rewire something the call never mentioned must roll
/// back rather than write.
#[tokio::test]
async fn a_move_that_would_change_connectivity_is_refused() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let sch_path = ctx.sch_path().to_path_buf();
    std::fs::write(&sch_path, EMPTY_SHEET).unwrap();

    let script = vec![
        tool_call("t1", "add_symbol", json!({"lib_id": "Device:R", "ref": "R1"})),
        tool_call(
            "t2",
            "add_symbol",
            json!({"lib_id": "Device:R", "ref": "R2", "near": "R1", "side": "right"}),
        ),
        tool_call("t3", "connect", json!({"from": "R1.2", "to": "R2.1", "net": "MID"})),
        tool_call("t4", "move_symbols", json!({"moves": [{"ref": "R2", "by": [0.0, 25.4]}]})),
        final_text("done"),
    ];
    let mut agent = Agent::new(ScriptedClient::new(script), ctx, system_prompt());
    agent
        .run_turn("move R2 away", &mut AutoApprove::yes(), None)
        .await
        .unwrap();

    let text = std::fs::read_to_string(&sch_path).unwrap();
    assert!(
        text.contains("\"MID\""),
        "the refused move must leave the wired schematic intact"
    );
}
