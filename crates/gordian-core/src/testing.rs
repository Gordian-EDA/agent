//! Test doubles for driving the agent loop without a network.
//!
//! [`ScriptedClient`] is a [`Provider`] that replays a fixed `Vec<Completion>`,
//! one per `complete()` call, in order — so the loop's control flow (tool
//! dispatch → result feedback → apply-gate → final text) is exercised
//! deterministically. It also records every `messages` slice it was given, so a
//! test can assert on exactly what the model would see across turns.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use futures::stream::{self, StreamExt};
use llm_client::{Completion, EventStream, Message, Provider, StreamEvent, ToolCall, ToolDef};

/// A [`Provider`] that replays a fixed script of completions, one per call, and
/// records the conversation it was shown.
///
/// Running off the end of the script is a hard error (the loop asked for more
/// than the test scripted — usually a loop bug).
pub struct ScriptedClient {
    script: Mutex<VecDeque<Completion>>,
    seen: Arc<Mutex<Vec<Vec<Message>>>>,
}

impl ScriptedClient {
    /// Build a client that replays `completions` in order.
    pub fn new(completions: Vec<Completion>) -> Self {
        Self {
            script: Mutex::new(completions.into_iter().collect()),
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Build a client and a shared handle to the recorded `messages` slices (one
    /// `Vec<Message>` per model call), so a test can assert on what each request
    /// carried.
    pub fn recording(completions: Vec<Completion>) -> (Self, Arc<Mutex<Vec<Vec<Message>>>>) {
        let client = Self::new(completions);
        let seen = Arc::clone(&client.seen);
        (client, seen)
    }

    /// Record `messages` and pop the next scripted completion (one per model
    /// call). Shared by both [`Provider::complete`] and [`Provider::stream`].
    fn next_completion(&self, messages: &[Message]) -> Result<Completion> {
        self.seen.lock().unwrap().push(messages.to_vec());
        self.script.lock().unwrap().pop_front().ok_or_else(|| {
            anyhow::anyhow!("ScriptedClient script exhausted: called more times than scripted")
        })
    }
}

#[async_trait]
impl Provider for ScriptedClient {
    async fn complete(
        &self,
        _system: &str,
        messages: &[Message],
        _tools: &[ToolDef],
    ) -> Result<Completion> {
        self.next_completion(messages)
    }

    /// Replay the next scripted completion as a stream: its text arrives as a
    /// couple of `TextDelta`s (so the loop's live-streaming path is exercised),
    /// then a `Completed` carrying the scripted tool calls, stop reason, and
    /// usage. With empty text it is a single `Completed`.
    async fn stream<'a>(
        &'a self,
        _system: &'a str,
        messages: &'a [Message],
        _tools: &'a [ToolDef],
    ) -> Result<EventStream<'a>> {
        let mut completion = self.next_completion(messages)?;
        let text = std::mem::take(&mut completion.text);
        let mut events: Vec<Result<StreamEvent>> =
            split_in_two(&text).into_iter().map(|s| Ok(StreamEvent::TextDelta(s))).collect();
        // The deltas reconstruct the text; the terminal completion carries the
        // assembled tool calls/usage, with its `text` left empty (drained above).
        events.push(Ok(StreamEvent::Completed(completion)));
        Ok(stream::iter(events).boxed())
    }
}

/// Split `text` roughly in half (on a char boundary), dropping empty halves —
/// so non-empty text streams as two deltas and empty text streams as none.
fn split_in_two(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let mid = text.char_indices().nth(text.chars().count() / 2).map(|(i, _)| i).unwrap_or(text.len());
    let (a, b) = text.split_at(mid);
    [a, b].into_iter().filter(|s| !s.is_empty()).map(str::to_string).collect()
}

/// Build a completion carrying a single tool call (no text).
pub fn tool_call(id: &str, name: &str, input: serde_json::Value) -> Completion {
    Completion {
        text: String::new(),
        tool_calls: vec![ToolCall { id: id.to_string(), name: name.to_string(), input }],
        stop_reason: "tool_use".to_string(),
        ..Default::default()
    }
}

/// Build a final text completion (end of turn, no tool calls).
pub fn final_text(text: &str) -> Completion {
    Completion {
        text: text.to_string(),
        tool_calls: Vec::new(),
        stop_reason: "end_turn".to_string(),
        ..Default::default()
    }
}
