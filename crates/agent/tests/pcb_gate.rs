//! Slice-5 SPEC GATE (deterministic, no creds, no KiCAD): the agent closes a
//! board it fails on the first route by relaxing a rule.
//!
//! This is THE gate the slice promises: "Agent closes a board it failed
//! first-pass by moving a part / relaxing a rule." It runs the FULL agent loop
//! machinery — a mock provider scripts the assistant's tool_use turns, and the
//! loop executes the real `Tools::run` against a KiCAD-free footprint index built
//! from the three vendored kicad-bridge fixtures. No network, no model
//! nondeterminism, no KiCAD: it must hold forever.
//!
//! ## The crafted board
//!
//! Two pin-headers locked on opposite sides of the board, joined by nets A and B,
//! with a full-height keepout WALL on BOTH copper layers splitting the board down
//! the middle (the draft-level mirror of `congested.json`'s saturated-wall
//! defeat). The wall encloses both nets, so the FIRST `route_board` reports honest
//! failures (and an empty `lint_summary` — those gaps are expected, not an engine
//! bug). The triage scripted here is the RELAX-A-RULE path: `set_constraints`
//! replaces the solid wall with a gapped pair, opening a corridor; the SECOND
//! `route_board` then routes clean. (Keepouts don't move parts, so no re-place is
//! needed between the two routes — this is the simplest deterministic triage.)
//!
//! ## Tighten, don't delete
//!
//! This gate asserts the spec promise. If the engine improves and the first route
//! starts SUCCEEDING on the walled board, that is a real engine change: investigate
//! and craft a tighter defeat — do NOT weaken or delete this assertion to make the
//! test pass.

use std::sync::{Arc, Mutex};

use agent::llm::{Completion, ContentBlock, LlmClient, Message, ToolCall, ToolDef};
use agent::tools::ToolCtx;
use agent::{Agent, AutoApprove};
use anyhow::Result;
use async_trait::async_trait;

/// A mock LLM that replays a fixed script of completions, one per `complete()`
/// call, and RECORDS the message history it is handed each call so the test can
/// read back the real tool results the loop produced.
struct MockLlm {
    script: Mutex<std::collections::VecDeque<Completion>>,
    /// The message history handed to the most recent `complete()` call — shared
    /// with the test so it can read back the real tool results the loop produced.
    seen: Arc<Mutex<Vec<Message>>>,
}

impl MockLlm {
    fn script(completions: Vec<Completion>, seen: Arc<Mutex<Vec<Message>>>) -> Self {
        Self {
            script: Mutex::new(completions.into_iter().collect()),
            seen,
        }
    }
}

#[async_trait]
impl LlmClient for MockLlm {
    async fn complete(
        &self,
        _system: &str,
        messages: &[Message],
        _tools: &[ToolDef],
    ) -> Result<Completion> {
        *self.seen.lock().unwrap() = messages.to_vec();
        self.script.lock().unwrap().pop_front().ok_or_else(|| {
            anyhow::anyhow!(
                "MockLlm script exhausted: loop called complete() more times than scripted"
            )
        })
    }
}

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

/// Stage the three vendored `.kicad_mod` fixtures into `<tmp>/Fixtures.pretty/`
/// (lib nickname `Fixtures`) so the footprint index needs no installed KiCAD.
fn staged_footprint_dir() -> (tempfile::TempDir, std::path::PathBuf) {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../kicad-bridge/tests/fixtures/footprints");
    let tmp = tempfile::tempdir().unwrap();
    let pretty = tmp.path().join("Fixtures.pretty");
    std::fs::create_dir_all(&pretty).unwrap();
    for name in [
        "R_0603_1608Metric.kicad_mod",
        "SOT-23.kicad_mod",
        "PinHeader_1x02_P2.54mm_Vertical.kicad_mod",
    ] {
        std::fs::copy(src.join(name), pretty.join(name)).unwrap();
    }
    let p = tmp.path().to_path_buf();
    (tmp, p)
}

/// The scripted triage conversation: build a walled board, fail the first route,
/// relax the keepout, route clean. One completion per `complete()` call.
fn triage_script() -> Vec<Completion> {
    let header = "Fixtures:PinHeader_1x02_P2.54mm_Vertical";
    vec![
        // 1. Declare the board: two pin-headers, nets A and B.
        tool_call(
            "tu_create",
            "create_board",
            serde_json::json!({
                "bounds": { "min_x": 0.0, "max_x": 40.0, "min_y": 0.0, "max_y": 20.0 },
                "parts": [
                    { "reference": "J1", "footprint": header,
                      "pad_nets": { "1": "A", "2": "B" } },
                    { "reference": "J2", "footprint": header,
                      "pad_nets": { "1": "A", "2": "B" } }
                ]
            }),
        ),
        // 2–3. Lock the two connectors on opposite sides — they MUST cross the wall.
        tool_call(
            "tu_mv1",
            "move_part",
            serde_json::json!({ "reference": "J1", "x": 3.0, "y": 10.0 }),
        ),
        tool_call(
            "tu_mv2",
            "move_part",
            serde_json::json!({ "reference": "J2", "x": 37.0, "y": 10.0 }),
        ),
        // 4. Craft the defeat: a full-height keepout WALL on both layers, splitting
        //    the board so both nets are enclosed and the first route must fail.
        tool_call(
            "tu_wall",
            "set_constraints",
            serde_json::json!({
                "keepouts": [
                    { "rect": { "min_x": 19.0, "max_x": 21.0, "min_y": 0.0, "max_y": 20.0 },
                      "layers": ["top", "bottom"] }
                ]
            }),
        ),
        // 5. Place (legal — the wall is a routing obstacle, not a placement no-go).
        tool_call("tu_place", "place_board", serde_json::json!({})),
        // 6. FIRST route — fails honestly (the wall encloses A and B).
        tool_call("tu_route1", "route_board", serde_json::json!({})),
        // 7. TRIAGE (relax-a-rule): replace the solid wall with a gapped PAIR,
        //    opening a corridor through the middle.
        tool_call(
            "tu_relax",
            "set_constraints",
            serde_json::json!({
                "keepouts": [
                    { "rect": { "min_x": 19.0, "max_x": 21.0, "min_y": 0.0, "max_y": 8.0 },
                      "layers": ["top", "bottom"] },
                    { "rect": { "min_x": 19.0, "max_x": 21.0, "min_y": 12.0, "max_y": 20.0 },
                      "layers": ["top", "bottom"] }
                ]
            }),
        ),
        // 8. SECOND route — clean (keepouts don't move parts, so no re-place needed).
        tool_call("tu_route2", "route_board", serde_json::json!({})),
        // 9. Done.
        final_text("Closed the board: relaxed the wall keepout into a gapped pair and re-routed clean."),
    ]
}

/// Pull the JSON tool result for `tool_use_id` out of a captured history.
fn tool_result(history: &[Message], tool_use_id: &str) -> Option<serde_json::Value> {
    for m in history {
        for b in &m.content {
            if let ContentBlock::ToolResult {
                tool_use_id: id,
                content,
                ..
            } = b
                && id == tool_use_id
            {
                return serde_json::from_str(content).ok();
            }
        }
    }
    None
}

#[tokio::test]
async fn agent_closes_a_failed_board_by_relaxing_a_keepout() {
    let (_guard, dir) = staged_footprint_dir();
    let Some(ctx) = ToolCtx::with_footprint_dir_for_test(dir) else {
        eprintln!("SKIP: could not build a fixture ToolCtx");
        return;
    };
    let workspace_route = ctx.workspace().route_path();

    // A shared handle the mock writes the live history into; the test reads it
    // back after the turn to inspect the REAL tool results the loop produced.
    let seen: Arc<Mutex<Vec<Message>>> = Arc::new(Mutex::new(Vec::new()));
    let mock = MockLlm::script(triage_script(), Arc::clone(&seen));
    let mut agent = Agent::new(Box::new(mock), ctx);
    let mut approvals = AutoApprove::yes(); // no apply_design here, but the loop needs one.

    let outcome = agent
        .run_turn(
            "Route this two-connector board; there's a keepout wall in the way — fix it.",
            &mut approvals,
            None,
        )
        .await
        .unwrap();

    // The whole scripted sequence ran through the real loop and ended on the
    // final assistant text (no iteration-cap truncation).
    assert_eq!(
        outcome.stop_reason,
        agent::StopReason::Completed,
        "the triage loop must finish cleanly, not hit the iteration cap: {outcome:?}"
    );
    assert!(
        outcome.final_text.contains("re-routed clean")
            || outcome.final_text.contains("Closed the board"),
        "final assistant text should summarize the close: {:?}",
        outcome.final_text
    );
    // create + 2×move + 2×set_constraints + place + 2×route = 8 tool calls.
    assert_eq!(
        outcome.tool_calls_made, 8,
        "the full scripted tool sequence must execute: {outcome:?}"
    );

    // Read back the REAL tool results the loop produced (recorded by the mock on
    // the final `complete()` call, which carried the whole transcript).
    let history = seen.lock().unwrap().clone();

    // The FIRST route failed honestly: ≥ 1 failed net, an EMPTY lint_summary, and
    // NOT flagged as an engine bug (the wall is a board problem the model triages).
    let route1 = tool_result(&history, "tu_route1").expect("first route result");
    let failed1 = route1["failed"].as_array().expect("failed array");
    assert!(
        !failed1.is_empty(),
        "the walled board MUST fail the first route (≥1 failed net): {route1}"
    );
    assert!(
        route1.get("engine_bug").is_none(),
        "an honest wall failure must NOT be flagged engine_bug: {route1}"
    );
    assert!(
        route1["lint_summary"].as_object().unwrap().is_empty(),
        "honest failures keep lint_summary empty (no real violations): {route1}"
    );
    // The failure note steers the model toward the triage levers.
    assert!(
        route1["note"].as_str().unwrap().contains("triage"),
        "the failed-route note must point at triage: {route1}"
    );

    // The SECOND route (after the relax) routed clean: zero failed, clean lint.
    let route2 = tool_result(&history, "tu_route2").expect("second route result");
    assert!(
        route2["failed"].as_array().unwrap().is_empty(),
        "after relaxing the wall the board MUST route with zero failed nets: {route2}"
    );
    assert!(
        route2.get("engine_bug").is_none(),
        "the clean route must not flag an engine bug: {route2}"
    );
    assert!(
        route2["lint_summary"].as_object().unwrap().is_empty(),
        "the clean route's lint_summary must be empty: {route2}"
    );
    assert!(
        route2["metrics"]["traces"].as_u64().unwrap() > 0,
        "the clean route must emit copper: {route2}"
    );

    // The final state on disk is the CLEAN routed solution (the triage's route2),
    // ready to export. (Export itself is skipped here — no KiCAD in this gate.)
    assert!(
        workspace_route.exists(),
        "a routed solution must be persisted after the successful route"
    );
}

// ── live smoke (creds-gated canary; the mock gate above is the real gate) ───────
//
// A real-model run of the SAME defeat scenario. This is a reality probe, NOT the
// gate: assertions are TOLERANT. It is `#[ignore]`d (costs money + needs network)
// and SKIPs visibly without `AWS_BEARER_TOKEN_BEDROCK`. Run explicitly with:
//   set -a; source .env; set +a; \
//   cargo test -p agent --test pcb_gate -- --ignored --nocapture
//
// The board is pre-walled for the model (we seed create_board + the wall in the
// prompt's framing); the canary checks the model can DRIVE the triage tools at
// all and that the loop terminates inside its iteration cap — it does not demand
// a perfect close (that is the mock gate's job). Whatever the model did is
// printed; only a hard loop/transport failure fails the build.
#[tokio::test]
#[ignore = "hits real Bedrock; run with --ignored"]
async fn live_smoke_model_triages_a_walled_board() {
    if std::env::var("AWS_BEARER_TOKEN_BEDROCK").is_err() {
        eprintln!("SKIP: AWS_BEARER_TOKEN_BEDROCK not set");
        return;
    }
    let (_guard, dir) = staged_footprint_dir();
    let Some(ctx) = ToolCtx::with_footprint_dir_for_test(dir) else {
        eprintln!("SKIP: could not build a fixture ToolCtx");
        return;
    };
    let route_path = ctx.workspace().route_path();

    let client = agent::llm::from_env().expect("LLM config from env/.env");
    let mut agent = Agent::new(Box::new(client), ctx);
    let mut approvals = AutoApprove::yes();

    // Hand the model the footprint nickname so it doesn't have to guess the
    // library, then describe the defeat and ask it to close the board.
    let prompt = "\
Build and route a tiny 2-layer board, 40x20 mm, using the footprint library \
nicknamed `Fixtures` (search_footprints with query `PinHeader` finds \
`Fixtures:PinHeader_1x02_P2.54mm_Vertical`). Place two of those connectors, J1 \
and J2, joined by nets A and B (pad 1 = A, pad 2 = B on each). Lock J1 near the \
west edge (x≈3, y≈10) and J2 near the east edge (x≈37, y≈10). Then add a \
full-height keepout wall on BOTH layers from x=19 to x=21 spanning the whole \
board height. Place and route it. The wall will make the first route fail — \
triage it (relax/replace the keepout or move a part) until route_board reports \
zero failed nets. Reply with a short summary when the board routes clean.";

    let outcome = agent
        .run_turn(prompt, &mut approvals, None)
        .await
        .expect("the loop must not error out at the transport level");

    eprintln!(
        "LIVE SMOKE: stop_reason={:?}, tool_calls={}, final_text={:?}",
        outcome.stop_reason, outcome.tool_calls_made, outcome.final_text
    );

    // Tolerant: the model must have DRIVEN the board tools (not just chatted).
    assert!(
        outcome.tool_calls_made > 0,
        "the model should have called at least one board tool: {outcome:?}"
    );

    // Report the on-disk outcome without failing the build on a model miss: if a
    // route landed, say whether it is clean; the mock gate is what guarantees the
    // close, this just tells us what the live model managed.
    if let Ok(raw) = std::fs::read_to_string(&route_path) {
        let routed: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
        let failed = routed["failed"].as_array().map(Vec::len).unwrap_or(0);
        eprintln!("LIVE SMOKE: a route was persisted with {failed} failed net(s)");
        if failed == 0 {
            eprintln!("LIVE SMOKE: model CLOSED the board clean (canary fully green)");
        } else {
            eprintln!("LIVE SMOKE: model left {failed} failed net(s) — canary probe only, not a gate");
        }
    } else {
        eprintln!("LIVE SMOKE: model produced no persisted route (canary probe only)");
    }
}
