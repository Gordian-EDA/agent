//! Multi-turn context tests: the agent must carry conversation history across
//! [`Agent::run_turn`] calls, support unwinding the last turn, clearing, and
//! compacting. Uses a RECORDING mock [`LlmClient`] that snapshots the exact
//! `messages` slice it receives per call, so the tests assert on what the
//! model would actually see.
//!
//! The tools the loop drives still need KiCAD (via [`ToolCtx::detect_for_test`]);
//! all tests SKIP gracefully when no KiCAD is detected.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use agent::llm::{Completion, ContentBlock, LlmClient, Message, Role, ToolDef};
use agent::tools::ToolCtx;
use agent::{Agent, AutoApprove};
use anyhow::Result;
use async_trait::async_trait;

/// A scripted mock that also records every `messages` argument it was given.
struct RecordingLlm {
    script: Mutex<VecDeque<Completion>>,
    seen: Arc<Mutex<Vec<Vec<Message>>>>,
}

impl RecordingLlm {
    fn new(completions: Vec<Completion>) -> (Self, Arc<Mutex<Vec<Vec<Message>>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                script: Mutex::new(completions.into_iter().collect()),
                seen: Arc::clone(&seen),
            },
            seen,
        )
    }
}

#[async_trait]
impl LlmClient for RecordingLlm {
    async fn complete(
        &self,
        _system: &str,
        messages: &[Message],
        _tools: &[ToolDef],
    ) -> Result<Completion> {
        self.seen.lock().unwrap().push(messages.to_vec());
        self.script
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("RecordingLlm script exhausted"))
    }
}

fn final_text(text: &str) -> Completion {
    Completion {
        text: text.to_string(),
        stop_reason: "end_turn".to_string(),
        ..Default::default()
    }
}

/// All text content of a message, concatenated.
fn text_of(m: &Message) -> String {
    m.content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect()
}

fn ctx() -> Option<ToolCtx> {
    ToolCtx::detect_for_test()
}

#[tokio::test]
async fn second_turn_sees_the_first_turns_messages() {
    let Some(ctx) = ctx() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let (mock, seen) = RecordingLlm::new(vec![final_text("answer one"), final_text("answer two")]);
    let mut agent = Agent::new(Box::new(mock), ctx);
    let mut approvals = AutoApprove::yes();

    agent
        .run_turn("first prompt", &mut approvals, None)
        .await
        .unwrap();
    agent
        .run_turn("second prompt", &mut approvals, None)
        .await
        .unwrap();

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "one model call per turn");

    // The second call must carry the whole first exchange plus the new prompt.
    let second = &seen[1];
    assert_eq!(second.len(), 3, "user1 + assistant1 + user2: {second:#?}");
    assert_eq!(second[0].role, Role::User);
    assert_eq!(text_of(&second[0]), "first prompt");
    assert_eq!(second[1].role, Role::Assistant);
    assert_eq!(text_of(&second[1]), "answer one");
    assert_eq!(second[2].role, Role::User);
    assert_eq!(text_of(&second[2]), "second prompt");
}

#[tokio::test]
async fn pop_last_turn_unwinds_the_last_exchange() {
    let Some(ctx) = ctx() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let (mock, seen) =
        RecordingLlm::new(vec![final_text("a1"), final_text("a2"), final_text("a3")]);
    let mut agent = Agent::new(Box::new(mock), ctx);
    let mut approvals = AutoApprove::yes();

    agent.run_turn("one", &mut approvals, None).await.unwrap();
    agent.run_turn("two", &mut approvals, None).await.unwrap();
    assert!(agent.pop_last_turn(), "there is a turn to pop");
    agent.run_turn("three", &mut approvals, None).await.unwrap();

    let seen = seen.lock().unwrap();
    let third = &seen[2];
    // Turn "two" was unwound: the model sees one + a1 + three only.
    assert_eq!(third.len(), 3, "{third:#?}");
    assert_eq!(text_of(&third[0]), "one");
    assert_eq!(text_of(&third[1]), "a1");
    assert_eq!(text_of(&third[2]), "three");
}

#[tokio::test]
async fn pop_with_no_turns_is_false_and_clear_resets() {
    let Some(ctx) = ctx() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let (mock, seen) = RecordingLlm::new(vec![final_text("a1"), final_text("a2")]);
    let mut agent = Agent::new(Box::new(mock), ctx);
    let mut approvals = AutoApprove::yes();

    assert!(!agent.pop_last_turn(), "nothing to pop on a fresh agent");

    agent.run_turn("one", &mut approvals, None).await.unwrap();
    agent.clear_history();
    let stats = agent.context_stats();
    assert_eq!(stats.messages, 0, "clear empties the history");
    assert_eq!(stats.turns, 0);

    agent.run_turn("two", &mut approvals, None).await.unwrap();
    let seen = seen.lock().unwrap();
    let second = &seen[1];
    assert_eq!(second.len(), 1, "post-clear turn starts fresh: {second:#?}");
    assert_eq!(text_of(&second[0]), "two");
}

#[tokio::test]
async fn context_stats_reflect_the_session() {
    let Some(ctx) = ctx() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let (mock, _seen) = RecordingLlm::new(vec![final_text("a1")]);
    let mut agent = Agent::new(Box::new(mock), ctx);
    let mut approvals = AutoApprove::yes();

    let empty = agent.context_stats();
    assert_eq!((empty.turns, empty.messages), (0, 0));

    agent
        .run_turn("hello there", &mut approvals, None)
        .await
        .unwrap();
    let stats = agent.context_stats();
    assert_eq!(stats.turns, 1);
    assert_eq!(stats.messages, 2, "user + assistant");
    assert!(stats.approx_chars >= "hello there".len() + "a1".len());
}

#[tokio::test]
async fn compact_replaces_history_with_a_summary_pair() {
    let Some(ctx) = ctx() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    let (mock, seen) = RecordingLlm::new(vec![
        final_text("a1"),
        final_text("THE SUMMARY"),
        final_text("a2"),
    ]);
    let mut agent = Agent::new(Box::new(mock), ctx);
    let mut approvals = AutoApprove::yes();

    agent.run_turn("one", &mut approvals, None).await.unwrap();
    let (before, after) = agent.compact(None).await.unwrap();
    assert_eq!(before, 2, "user + assistant before compaction");
    assert_eq!(after, 2, "summary pair after compaction");

    agent.run_turn("two", &mut approvals, None).await.unwrap();
    let seen = seen.lock().unwrap();
    let third = &seen[2];
    assert_eq!(third.len(), 3, "summary pair + new prompt: {third:#?}");
    assert_eq!(third[0].role, Role::User);
    assert!(
        text_of(&third[0]).contains("THE SUMMARY"),
        "summary carried: {}",
        text_of(&third[0])
    );
    assert_eq!(third[1].role, Role::Assistant);
    assert_eq!(text_of(&third[2]), "two");

    // Compaction is a context barrier: nothing before it can be unwound.
    assert!(!agent.pop_last_turn() || agent.context_stats().messages >= 2);
}

#[tokio::test]
async fn usage_tokens_flow_through_completions() {
    let Some(ctx) = ctx() else {
        eprintln!("SKIP: no KiCAD detected");
        return;
    };
    use agent::AgentEvent;
    use tokio::sync::mpsc::unbounded_channel;

    let completion = Completion {
        text: "done".to_string(),
        stop_reason: "end_turn".to_string(),
        input_tokens: 1234,
        output_tokens: 56,
        ..Default::default()
    };
    let (mock, _seen) = RecordingLlm::new(vec![completion]);
    let mut agent = Agent::new(Box::new(mock), ctx);
    let mut approvals = AutoApprove::yes();
    let (tx, mut rx) = unbounded_channel();

    agent
        .run_turn("hi", &mut approvals, Some(&tx))
        .await
        .unwrap();

    let mut saw_usage = false;
    while let Ok(ev) = rx.try_recv() {
        if let AgentEvent::Usage {
            input_tokens,
            output_tokens,
        } = ev
        {
            assert_eq!((input_tokens, output_tokens), (1234, 56));
            saw_usage = true;
        }
    }
    assert!(saw_usage, "a Usage event is emitted per completion");
}
