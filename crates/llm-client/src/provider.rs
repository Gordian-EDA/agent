//! The vendor-neutral [`Provider`] trait and its streaming surface.
//!
//! A [`Provider`] runs one completion against a conversation. It exposes two
//! shapes of the same call:
//!
//! - [`Provider::complete`] — await the whole [`Completion`] at once. This is the
//!   natural shape for the request/response backends (Bedrock Converse,
//!   OpenAI chat-completions), which override it directly.
//! - [`Provider::stream`] — incrementally yield [`StreamEvent`]s (live text
//!   deltas, then the assembled tool calls and final completion). This is the
//!   natural shape for an SSE backend (the native Anthropic Messages API).
//!
//! A backend overrides whichever shape is natural; the other is derived. The
//! default [`Provider::stream`] wraps [`Provider::complete`] into a single
//! `Completed` event, and a streaming backend's `complete` drains its own
//! `stream` via [`drain_stream`]. (Exactly one direction is overridden per
//! backend, so there is no mutual recursion.)

use std::pin::Pin;

use anyhow::Result;
use async_trait::async_trait;
use futures::stream::{self, Stream, StreamExt};

use crate::types::{Completion, Message, ToolDef};

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
    ///
    /// The default drains [`Provider::stream`]; request/response backends
    /// override this directly and let the default `stream` wrap it.
    async fn complete(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDef],
    ) -> Result<Completion> {
        drain_stream(self.stream(system, messages, tools).await?).await
    }

    /// Run one completion as an incremental [`StreamEvent`] stream. The default
    /// wraps [`Provider::complete`] into a single `Completed` event (no live
    /// deltas); a streaming backend overrides this to emit `TextDelta`s as they
    /// arrive.
    async fn stream<'a>(
        &'a self,
        system: &'a str,
        messages: &'a [Message],
        tools: &'a [ToolDef],
    ) -> Result<EventStream<'a>> {
        let completion = self.complete(system, messages, tools).await?;
        Ok(stream::once(async move { Ok(StreamEvent::Completed(completion)) }).boxed())
    }

    /// Start a new thread (a fresh conversation/session, e.g. on `/clear`):
    /// regenerate any per-thread state such as the request `thread_identifier`.
    /// Default: no-op.
    fn new_thread(&mut self) {}
}

/// Drain an [`EventStream`] to its final [`Completion`]: text deltas are
/// concatenated and the terminating `Completed` event supplies the tool calls,
/// stop reason, and token usage. Errors if the stream ends without a `Completed`
/// event (a malformed / truncated stream).
pub async fn drain_stream(mut events: EventStream<'_>) -> Result<Completion> {
    let mut text = String::new();
    while let Some(ev) = events.next().await {
        match ev? {
            StreamEvent::TextDelta(t) => text.push_str(&t),
            StreamEvent::Completed(mut c) => {
                // Prefer the assembled text if the backend filled it; otherwise
                // fall back to the concatenated deltas.
                if c.text.is_empty() && !text.is_empty() {
                    c.text = text;
                }
                return Ok(c);
            }
        }
    }
    anyhow::bail!("stream ended without a Completed event")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ToolCall;
    use serde_json::json;

    /// A streaming-only backend: overrides `stream`, gets `complete` for free.
    struct StreamingMock;

    #[async_trait]
    impl Provider for StreamingMock {
        async fn stream<'a>(
            &'a self,
            _system: &'a str,
            _messages: &'a [Message],
            _tools: &'a [ToolDef],
        ) -> Result<EventStream<'a>> {
            let evs = vec![
                Ok(StreamEvent::TextDelta("hel".into())),
                Ok(StreamEvent::TextDelta("lo".into())),
                Ok(StreamEvent::Completed(Completion {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "t1".into(),
                        name: "f".into(),
                        input: json!({}),
                    }],
                    stop_reason: "tool_use".into(),
                    input_tokens: 3,
                    output_tokens: 4,
                    ..Default::default()
                })),
            ];
            Ok(stream::iter(evs).boxed())
        }
    }

    /// A request/response backend: overrides `complete`, gets `stream` for free.
    struct BlockingMock;

    #[async_trait]
    impl Provider for BlockingMock {
        async fn complete(
            &self,
            _system: &str,
            _messages: &[Message],
            _tools: &[ToolDef],
        ) -> Result<Completion> {
            Ok(Completion { text: "answer".into(), stop_reason: "end_turn".into(), ..Default::default() })
        }
    }

    #[tokio::test]
    async fn default_complete_drains_stream_and_concatenates_deltas() {
        let c = StreamingMock.complete("", &[], &[]).await.unwrap();
        assert_eq!(c.text, "hello", "text deltas reassemble");
        assert_eq!(c.tool_calls.len(), 1);
        assert_eq!(c.stop_reason, "tool_use");
        assert_eq!((c.input_tokens, c.output_tokens), (3, 4));
    }

    #[tokio::test]
    async fn default_stream_wraps_complete_into_one_completed_event() {
        let mut s = BlockingMock.stream("", &[], &[]).await.unwrap();
        let ev = s.next().await.unwrap().unwrap();
        assert_eq!(ev, StreamEvent::Completed(Completion {
            text: "answer".into(),
            stop_reason: "end_turn".into(),
            ..Default::default()
        }));
        assert!(s.next().await.is_none(), "exactly one event");
    }
}
