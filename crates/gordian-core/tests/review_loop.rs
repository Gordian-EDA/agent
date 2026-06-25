//! Scripted test for the IN-LOOP review→fix loop with BOTH a netlist and a VISION
//! layout critic — no KiCAD, no network.
//!
//! A [`ReviewStub`] backend stands in for the production KiCAD backend: its
//! `review_committed` runs the REAL [`gordian_core::review_kicad`] passes (netlist
//! over text, layout over a mocked PNG) against a recording reviewer client, then
//! UNIONS the two defect lists exactly as the production path does. Driving it
//! through [`Agent::run_turn_reviewed`] proves:
//!
//! - the layout-review call is MADE, and carries a [`ContentBlock::Image`] (the
//!   render the vision critic looks at);
//! - a high-confidence LAYOUT defect unions into the fix turn alongside the
//!   netlist defects and feeds a fix turn; and
//! - [`AgentEvent::Reviewed`] fires with that defect.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use gordian_core::testing::{ScriptedClient, final_text, tool_call};
use gordian_core::{
    Agent, AgentEvent, ApplyInfo, AutoApprove, Completion, ContentBlock, ImageData, Message,
    Provider, ReviewOutcome, RunMode, TestBackend, ToolCall, ToolDef, ToolOutcome,
};
use serde_json::{Value, json};

/// A reviewer [`Provider`] that returns ONE canned verdict for every
/// `complete()` and RECORDS the messages it was shown — so a test can assert the
/// layout pass attached an image. The verdict is a layout-critic FINAL_JSON with a
/// high-confidence major defect (so it survives the high-confidence filter).
struct CannedReviewer {
    verdict: String,
    seen: Arc<Mutex<Vec<Vec<Message>>>>,
}

impl CannedReviewer {
    fn new(verdict: &str) -> (Self, Arc<Mutex<Vec<Vec<Message>>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        (Self { verdict: verdict.to_string(), seen: Arc::clone(&seen) }, seen)
    }
}

#[async_trait]
impl Provider for CannedReviewer {
    /// The review ensemble drives `complete` (history-free); the default `stream`
    /// wraps it, so overriding only `complete` is enough.
    async fn complete(
        &self,
        _system: &str,
        messages: &[Message],
        _tools: &[ToolDef],
    ) -> anyhow::Result<Completion> {
        self.seen.lock().unwrap().push(messages.to_vec());
        Ok(Completion { text: self.verdict.clone(), stop_reason: "end_turn".into(), ..Default::default() })
    }
}

/// A layout-critic verdict: a high-confidence MAJOR readability defect on C1.
const LAYOUT_VERDICT: &str = r#"reasoning: C1 is stranded far from U1's power pin.
FINAL_JSON:
{"score": 5, "dimension_scores": {"readability": 5},
 "defects": [{"severity":"major","confidence":"high","category":"spacing",
   "location":"C1","description":"decoupling cap C1 sits across the sheet from U1's power pin"}]}"#;

/// A stub backend whose `review_committed` runs the real netlist + layout review
/// passes (the layout pass over a fixed mocked PNG) against `reviewer`, unioning
/// their defects exactly as the production path does.
struct ReviewStub {
    reviewer: Arc<CannedReviewer>,
    /// Number of times `review_committed` was called (one per fix round).
    reviews_done: Arc<Mutex<usize>>,
}

#[async_trait]
impl TestBackend for ReviewStub {
    async fn run(&self, call: &ToolCall, mode: RunMode, _r: &dyn Provider) -> ToolOutcome {
        match (call.name.as_str(), mode) {
            ("apply_design", RunMode::Preview) => ToolOutcome {
                value: json!({ "ok": true, "would_write": true }),
                images: Vec::new(),
                image_path: None,
                apply: Some(ApplyInfo { ready: true, ..Default::default() }),
            },
            ("apply_design", RunMode::Commit) => ToolOutcome {
                value: json!({ "ok": true, "written": true }),
                images: Vec::new(),
                image_path: None,
                apply: Some(ApplyInfo { ready: true, committed: true, summary: "ok".into() }),
            },
            _ => ToolOutcome::plain(json!({ "ok": true })),
        }
    }

    /// Run BOTH planes against the recording reviewer and union them — the real
    /// `gordian_core::review_kicad` calls, with the layout pass over a 1x1 mock PNG.
    async fn review_committed(&self, intent: &str, _r: &dyn Provider) -> Option<ReviewOutcome> {
        let round = {
            let mut n = self.reviews_done.lock().unwrap();
            let r = *n;
            *n += 1;
            r
        };
        // Round 0 surfaces the defect (drives a fix turn); round 1 is clean so the
        // loop terminates without exhausting the script.
        if round > 0 {
            return Some(ReviewOutcome { score: 9.0, defects: vec![] });
        }
        let reviewer = self.reviewer.as_ref();
        // Netlist plane (text subject).
        let (n_score, mut defects) =
            gordian_core::review_kicad::review_netlist(reviewer, intent, "R1: {between:[A,B]}")
                .await
                .ok()?;
        // Layout plane (vision over a mocked render).
        let image = ImageData { format: "png".into(), base64: tiny_png_b64() };
        let (l_score, l_defects) = gordian_core::review_kicad::review_layout(
            reviewer,
            intent,
            image,
            gordian_core::review_kicad::LayoutKind::Schematic,
        )
        .await
        .ok()?;
        for d in l_defects {
            if !defects.iter().any(|e| gordian_core::review_kicad::same_defect(e, &d)) {
                defects.push(d);
            }
        }
        Some(ReviewOutcome { score: n_score.min(l_score), defects })
    }
}

/// A minimal valid base64 PNG payload (content is irrelevant to the test — only
/// that an image block rides the layout-review request).
fn tiny_png_b64() -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode([0x89, b'P', b'N', b'G', 0, 1, 2, 3])
}

#[tokio::test]
async fn committed_turn_runs_netlist_and_layout_review_then_fixes_the_layout_defect() {
    use tokio::sync::mpsc::unbounded_channel;

    let (reviewer, seen) = CannedReviewer::new(LAYOUT_VERDICT);
    let reviewer = Arc::new(reviewer);
    let backend = ReviewStub {
        reviewer: Arc::clone(&reviewer),
        reviews_done: Arc::new(Mutex::new(0)),
    };

    // The agent's OWN client (drives the turns), separate from the reviewer client.
    let client = ScriptedClient::new(vec![
        tool_call("t1", "apply_design", json!({ "commit": true })), // turn 1: commit
        final_text("committed"),
        tool_call("t2", "apply_design", json!({ "commit": true })), // fix turn: re-commit
        final_text("layout fixed"),
    ]);
    let mut agent = Agent::with_test_backend(Box::new(client), Box::new(backend), "sys");
    let mut approvals = AutoApprove::yes();
    let (tx, mut rx) = unbounded_channel();

    let out = agent
        .run_turn_reviewed("a 555 timer", "a 555 timer", &mut approvals, Some(&tx), 1)
        .await
        .unwrap();

    assert!(out.applied);
    assert_eq!(out.final_text, "layout fixed", "the fix turn ran and re-committed");

    // The Reviewed events: round 0 carries the layout defect, round 1 is clean.
    let mut rounds: Vec<(usize, Vec<String>)> = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let AgentEvent::Reviewed { round, defects, .. } = ev {
            rounds.push((round, defects));
        }
    }
    assert_eq!(rounds.len(), 2, "round 0 (defect) + round 1 (clean): {rounds:?}");
    assert_eq!(rounds[0].0, 0);
    assert!(
        rounds[0].1.iter().any(|d| d.contains("C1") && d.contains("decoupling cap")),
        "the LAYOUT defect unioned into the fed-back list: {:?}",
        rounds[0].1
    );
    assert_eq!(rounds[1], (1, vec![]), "round 1 re-review is clean");

    // The layout-review call was MADE and carried an image content block. The
    // netlist pass is text-only; at least one recorded request must hold an image.
    let seen = seen.lock().unwrap();
    let had_image = seen
        .iter()
        .flatten()
        .flat_map(|m| &m.content)
        .any(|b| matches!(b, ContentBlock::Image(_)));
    assert!(had_image, "the layout review attached a ContentBlock::Image");
}

/// LIVE one-turn smoke (`#[ignore]` — needs KiCAD + `.env` creds): commit a small
/// real design, then drive the REAL production review (netlist + the vision LAYOUT
/// critic over the actual render) through [`Agent::run_turn_reviewed`], printing
/// the unioned verdict from the emitted `Reviewed` events. Run with:
///   cargo test -p gordian-core --test review_loop -- --ignored --nocapture
#[tokio::test]
#[ignore = "live: needs KiCAD and LLM creds in .env"]
async fn live_layout_review_smoke() {
    use tokio::sync::mpsc::unbounded_channel;
    use gordian_core::tools::{PcbToolCtx, run_tool};

    let Some(ctx) = PcbToolCtx::detect_for_test() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let Ok(client) = gordian_core::from_env() else {
        eprintln!("SKIP: no LLM creds (.env) for the live layout-review smoke");
        return;
    };

    // A small design with an IC and a couple of decoupling caps — enough for the
    // layout critic to have something concrete to say about.
    let yaml = "version: 1\nblocks:\n  main:\n    components:\n\
        \x20     U1: {part: Device:R, pins: {1: VCC, 2: GND}}\n\
        \x20     C1: {part: Device:C, pins: {1: VCC, 2: GND}}\n\
        \x20     C2: {part: Device:C, pins: {1: VCC, 2: GND}}\n";
    let out = run_tool("apply_design", json!({ "yaml": yaml, "commit": true }), &ctx).unwrap();
    assert_eq!(out.get("written").and_then(Value::as_bool), Some(true), "committed: {out}");

    // Drive a review-only turn through the production loop so the real netlist +
    // vision review runs over the committed schematic. The model immediately
    // re-commits with no change; the post-turn review then runs.
    let mut agent = Agent::new(client, ctx, "sys");
    let mut approvals = AutoApprove::yes();
    let (tx, mut rx) = unbounded_channel();
    agent
        .run_turn_reviewed(
            &format!("Re-apply this exact design with apply_design(commit:true), no change:\n{yaml}"),
            "a decoupled supply rail",
            &mut approvals,
            Some(&tx),
            1,
        )
        .await
        .expect("the reviewed turn should complete");

    let mut saw_review = false;
    while let Ok(ev) = rx.try_recv() {
        if let AgentEvent::Reviewed { round, score, defects } = ev {
            eprintln!("LIVE review round {round}: score={score} defects={defects:#?}");
            assert!((0.0..=10.0).contains(&score), "a sane score: {score}");
            saw_review = true;
        }
    }
    assert!(saw_review, "the production review ran end-to-end with a live vision call");
}
