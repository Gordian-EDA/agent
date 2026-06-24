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
use llm_client::{Completion, Message, Provider, ToolCall, ToolDef};

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
    /// `Vec<Message>` per `complete()` call), so a test can assert on what each
    /// request carried.
    pub fn recording(completions: Vec<Completion>) -> (Self, Arc<Mutex<Vec<Vec<Message>>>>) {
        let client = Self::new(completions);
        let seen = Arc::clone(&client.seen);
        (client, seen)
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
        self.seen.lock().unwrap().push(messages.to_vec());
        self.script
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("ScriptedClient script exhausted: complete() called more times than scripted"))
    }
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
