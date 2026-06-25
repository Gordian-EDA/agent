//! The vendor-neutral [`Provider`] trait and its streaming surface.
//!
//! A [`Provider`] runs one completion against a conversation in either of two
//! shapes: await the whole [`Completion`] ([`Provider::complete`], required) or
//! consume it incrementally as [`StreamEvent`]s ([`Provider::stream`]). A
//! streaming backend overrides `stream` to emit live deltas; everything else
//! gets the default, which wraps `complete` into one terminal `Completed` event.

use std::pin::Pin;

use anyhow::Result;
use async_trait::async_trait;
use futures::stream::{self, Stream, StreamExt};

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
    /// stop reason, and usage. The default wraps [`Provider::complete`] into a
    /// single `Completed` event (no live deltas); a streaming backend overrides
    /// this to emit `TextDelta`s as they arrive.
    async fn stream<'a>(
        &'a self,
        system: &'a str,
        messages: &'a [Message],
        tools: &'a [ToolDef],
    ) -> Result<EventStream<'a>> {
        let completion = self.complete(system, messages, tools).await?;
        Ok(stream::once(async move { Ok(StreamEvent::Completed(completion)) }).boxed())
    }
}
