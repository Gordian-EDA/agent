//! The agent-loop's LLM seam: the [`Provider`] trait and its streaming surface,
//! spoken entirely in genai's own conversation types.
//!
//! There is one production [`Provider`] — the genai-backed
//! [`super::GenaiProvider`] — so the loop is generic over [`Provider`] only to
//! keep its single seam swappable for the deterministic, no-network tests
//! (`testing::ScriptedClient` and the review stubs). A [`Provider`] runs one
//! completion in either shape: await the whole [`StreamEnd`]
//! ([`Provider::complete`]) or consume it incrementally as genai
//! [`ChatStreamEvent`]s ([`Provider::stream`]). Both completion methods have a
//! default that derives one from the other, so an implementor overrides whichever
//! is natural — a streaming backend overrides `stream` (and gets `complete` by
//! draining); a one-shot reviewer overrides `complete`. Override at least one —
//! overriding neither recurses.

use std::pin::Pin;

use anyhow::Result;
use async_trait::async_trait;
use futures::stream::{self, Stream, StreamExt};

use genai::chat::{ChatMessage, ChatStreamEvent, ContentPart, StreamEnd, Tool};

/// A boxed stream of genai [`ChatStreamEvent`]s — the return type of
/// [`Provider::stream`]. Text arrives as [`ChatStreamEvent::Chunk`]s; the terminal
/// [`ChatStreamEvent::End`] carries the captured tool calls + usage.
pub type EventStream<'a> = Pin<Box<dyn Stream<Item = Result<ChatStreamEvent>> + Send + 'a>>;

/// The agent loop's LLM seam, over genai's conversation types.
#[async_trait]
pub trait Provider: Send + Sync {
    /// A `(provider, model)` pair for status display.
    fn status(&self) -> (String, String) {
        ("test".to_string(), "(scripted)".to_string())
    }

    /// Run one completion and await the whole result. Default: drain
    /// [`Provider::stream`] into its terminal [`StreamEnd`].
    async fn complete(
        &self,
        system: &str,
        messages: &[ChatMessage],
        tools: &[Tool],
    ) -> Result<StreamEnd> {
        drain_stream(self.stream(system, messages, tools).await?).await
    }

    /// Run one completion as a genai [`ChatStreamEvent`] stream. Default: wrap
    /// [`Provider::complete`] into a single `End` event (no live deltas).
    async fn stream<'a>(
        &'a self,
        system: &'a str,
        messages: &'a [ChatMessage],
        tools: &'a [Tool],
    ) -> Result<EventStream<'a>> {
        let end = self.complete(system, messages, tools).await?;
        Ok(stream::once(async move { Ok(ChatStreamEvent::End(end)) }).boxed())
    }
}

/// Drain an [`EventStream`] to its terminal [`StreamEnd`]: concatenate the text
/// chunks and, when the end didn't capture content text, fold them back in so a
/// caller can read the reply uniformly via [`StreamEnd::captured_texts`]. Errors
/// if the stream ends without an `End` event.
pub async fn drain_stream(mut events: EventStream<'_>) -> Result<StreamEnd> {
    let mut text = String::new();
    while let Some(ev) = events.next().await {
        match ev? {
            ChatStreamEvent::Chunk(chunk) => text.push_str(&chunk.content),
            ChatStreamEvent::End(mut end) => {
                if !text.is_empty() && end.captured_first_text().is_none() {
                    let mut content = end.captured_content.take().unwrap_or_default();
                    content.prepend(ContentPart::from_text(text));
                    end.captured_content = Some(content);
                }
                return Ok(end);
            }
            _ => {}
        }
    }
    anyhow::bail!("stream ended without an End event")
}
