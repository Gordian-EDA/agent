//! Test doubles for driving the agent loop without a network.
//!
//! [`ScriptedClient`] is a [`Provider`] that replays a fixed `Vec<StreamEnd>`,
//! one per call, in order — so the loop's control flow (tool dispatch → result
//! feedback → apply-gate → final text) is exercised deterministically. It also
//! records every `messages` slice it was given, so a test can assert on exactly
//! what the model would see across turns. Build the scripted ends with the
//! [`tool_call`] / [`final_text`] helpers.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::llm::{
    ChatMessage, ChatStreamEvent, EventStream, MessageContent, Provider, StreamChunk, StreamEnd,
    Tool, ToolCall,
};
use anyhow::Result;
use async_trait::async_trait;
use futures::stream::{self, StreamExt};

/// A [`Provider`] that replays a fixed script of completions, one per call, and
/// records the conversation it was shown.
///
/// Running off the end of the script is a hard error (the loop asked for more
/// than the test scripted — usually a loop bug).
pub struct ScriptedClient {
    script: Mutex<VecDeque<StreamEnd>>,
    seen: Arc<Mutex<Vec<Vec<ChatMessage>>>>,
}

impl ScriptedClient {
    /// Build a client that replays `completions` in order.
    pub fn new(completions: Vec<StreamEnd>) -> Self {
        Self {
            script: Mutex::new(completions.into_iter().collect()),
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Build a client and a shared handle to the recorded `messages` slices (one
    /// `Vec<ChatMessage>` per model call), so a test can assert on what each
    /// request carried.
    pub fn recording(completions: Vec<StreamEnd>) -> (Self, Arc<Mutex<Vec<Vec<ChatMessage>>>>) {
        let client = Self::new(completions);
        let seen = Arc::clone(&client.seen);
        (client, seen)
    }

    /// Record `messages` and pop the next scripted completion (one per model
    /// call). Shared by both [`Provider::complete`] and [`Provider::stream`].
    fn next_completion(&self, messages: &[ChatMessage]) -> Result<StreamEnd> {
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
        messages: &[ChatMessage],
        _tools: &[Tool],
    ) -> Result<StreamEnd> {
        self.next_completion(messages)
    }

    /// Replay the next scripted completion as a stream: its text arrives as a
    /// couple of [`ChatStreamEvent::Chunk`]s (so the loop's live-streaming path is
    /// exercised), then a terminal [`ChatStreamEvent::End`] carrying the scripted
    /// tool calls + usage. With empty text it is a single `End`.
    async fn stream<'a>(
        &'a self,
        _system: &'a str,
        messages: &'a [ChatMessage],
        _tools: &'a [Tool],
    ) -> Result<EventStream<'a>> {
        let end = self.next_completion(messages)?;
        let text = end
            .captured_texts()
            .map(|parts| parts.concat())
            .unwrap_or_default();
        let mut events: Vec<Result<ChatStreamEvent>> = split_in_two(&text)
            .into_iter()
            .map(|content| Ok(ChatStreamEvent::Chunk(StreamChunk { content })))
            .collect();
        events.push(Ok(ChatStreamEvent::End(end)));
        Ok(stream::iter(events).boxed())
    }
}

/// Split `text` roughly in half (on a char boundary), dropping empty halves —
/// so non-empty text streams as two deltas and empty text streams as none.
fn split_in_two(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let mid = text
        .char_indices()
        .nth(text.chars().count() / 2)
        .map(|(i, _)| i)
        .unwrap_or(text.len());
    let (a, b) = text.split_at(mid);
    [a, b]
        .into_iter()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Build a completion carrying a single tool call (no text).
pub fn tool_call(id: &str, name: &str, input: serde_json::Value) -> StreamEnd {
    StreamEnd {
        captured_content: Some(MessageContent::from_tool_calls(vec![ToolCall {
            call_id: id.to_string(),
            fn_name: name.to_string(),
            fn_arguments: input,
            thought_signatures: None,
        }])),
        ..Default::default()
    }
}

/// Build a final text completion (end of turn, no tool calls).
pub fn final_text(text: &str) -> StreamEnd {
    StreamEnd {
        captured_content: Some(MessageContent::from_text(text)),
        ..Default::default()
    }
}
