//! Live smoke for the production review loop.
//!
//! This intentionally exercises the real KiCAD-backed agent path. It is ignored
//! by default because it needs KiCAD plus TOML LLM config credentials.

mod common;

use gordian_core::tools::run_tool;
use gordian_core::{Agent, AgentEvent, AgentRuntime, AutoApprove};
use serde_json::{Value, json};
use tokio::sync::mpsc::unbounded_channel;

/// LIVE one-turn smoke: commit a small real design, then drive the production
/// post-turn review through [`Agent::run_turn_reviewed`], printing the verdict
/// from emitted `Reviewed` events.
///
/// Run with:
///   cargo test -p gordian-core --test review_loop -- --ignored --nocapture
#[tokio::test]
#[ignore = "live: needs KiCAD and LLM config.toml credentials"]
async fn live_layout_review_smoke() {
    let Some(ctx) = AgentRuntime::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let Ok(client) = common::live_provider_from_config() else {
        eprintln!("SKIP: no LLM config for the live layout-review smoke");
        return;
    };

    let yaml = "version: 1\nblocks:\n  main:\n    components:\n\
        \x20     U1: {part: Device:R, pins: {1: VCC, 2: GND}}\n\
        \x20     C1: {part: Device:C, pins: {1: VCC, 2: GND}}\n\
        \x20     C2: {part: Device:C, pins: {1: VCC, 2: GND}}\n";
    ctx.workspace().write_draft(yaml, None).unwrap();
    let out = run_tool("apply_design", json!({ "__commit": true }), &ctx).unwrap();
    assert_eq!(
        out.get("written").and_then(Value::as_bool),
        Some(true),
        "committed: {out}"
    );

    let mut agent = Agent::new(client, ctx, "sys");
    let mut approvals = AutoApprove::yes();
    let (tx, mut rx) = unbounded_channel();
    agent
        .run_turn_reviewed(
            "Re-apply the current durable draft with apply_design, no change.",
            "a decoupled supply rail",
            &mut approvals,
            Some(&tx),
            1,
        )
        .await
        .expect("the reviewed turn should complete");

    let mut saw_review = false;
    while let Ok(ev) = rx.try_recv() {
        if let AgentEvent::Reviewed {
            round,
            score,
            defects,
        } = ev
        {
            eprintln!("LIVE review round {round}: score={score} defects={defects:#?}");
            assert!((0.0..=10.0).contains(&score), "a sane score: {score}");
            saw_review = true;
        }
    }
    assert!(
        saw_review,
        "the production review ran end-to-end with a live vision call"
    );
}
