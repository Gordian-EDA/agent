//! The vendor-neutral [`Provider`] trait and its streaming surface.
//!
//! A [`Provider`] runs one completion against a conversation in either of two
//! shapes: await the whole [`Completion`] ([`Provider::complete`]) or consume it
//! incrementally as [`StreamEvent`]s ([`Provider::stream`]). Both are required —
//! every backend speaks both shapes directly (the genai backend streams via SSE
//! and blocks via one request; the scripted test double replays either).

use std::pin::Pin;

use anyhow::Result;
use async_trait::async_trait;
use futures::stream::Stream;

use super::types::{Completion, Message, ToolDef};

/// An incremental event from [`Provider::stream`].
#[derive(Clone, Debug, PartialEq)]
pub enum StreamEvent {
    /// A chunk of assistant text, for live rendering. Concatenating every
    /// `TextDelta` in order reconstructs [`Completion::text`].
    TextDelta(String),
    /// The completion finished: the fully-assembled result (text + tool calls +
    /// stop reason + token usage). Always the last event of a successful stream.
    Completed(Completion),
}

/// A boxed stream of [`StreamEvent`]s — the return type of [`Provider::stream`].
pub type EventStream<'a> = Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send + 'a>>;

/// Provider-agnostic completion interface.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Run one completion and await the whole result. `system` is the system
    /// prompt; `messages` is the conversation; `tools` may be empty.
    async fn complete(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDef],
    ) -> Result<Completion>;

    /// Run one completion as an incremental [`StreamEvent`] stream: live text
    /// deltas, then a terminal `Completed` carrying the assembled tool calls,
    /// stop reason, and usage.
    async fn stream<'a>(
        &'a self,
        system: &'a str,
        messages: &'a [Message],
        tools: &'a [ToolDef],
    ) -> Result<EventStream<'a>>;

    /// Start a new thread (a fresh conversation/session, e.g. on `/clear`):
    /// regenerate any per-thread state such as the request `thread_identifier`.
    /// Default: no-op.
    fn new_thread(&mut self) {}
}
