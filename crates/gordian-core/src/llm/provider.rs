//! The one production LLM, [`GenaiProvider`], built on the [`genai`] crate.
//!
//! [`GenaiProvider`] is a streaming [`Provider`](super::Provider) (it overrides
//! only [`Provider::stream`](super::Provider::stream); `complete` is derived by
//! draining it). genai is provider-agnostic — it picks the wire adapter from the
//! model name and reads each provider's standard key itself — so this is just the
//! genai [`Client`] wiring ([`GenaiProvider::from_env`]), the
//! [`Provider::status`](super::Provider::status) label, and the [`StreamEnd`]
//! accessors the agent reads usage off of.
//!
//! [`GenaiProvider::from_env`] needs only `AGENT_MODEL`; genai reads the
//! provider's standard key (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`,
//! `BEDROCK_API_KEY` for `bedrock_api::…`, `OPEN_ROUTER_API_KEY` for
//! `open_router::…`, …). Even a private OpenAI-compatible endpoint is env-native
//! via genai's custom adapter: `AGENT_MODEL=genai_1::<model>` reads
//! `GENAI_1_ENDPOINT` + `GENAI_1_API_KEY`.
//!
//! Notable: one request-level `ephemeral` [`CacheControl`] breakpoint caches the
//! static system+tools prefix (genai routes it per adapter and folds the cache
//! token counts back into [`genai::chat::Usage::prompt_tokens_details`]).

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::StreamExt;

use genai::Client;
use genai::chat::{CacheControl, ChatMessage, ChatOptions, ChatRequest, StreamEnd, Tool};

use super::seam::{EventStream, Provider};

/// Request `max_tokens`. 16k not 4k: one `create_design` carries the whole
/// circuit YAML, which for a large board exceeds 4k and would truncate; the
/// configured models allow ≥16k.
const MAX_TOKENS: u32 = 16384;

/// The one production [`Provider`], over genai: the configured [`Client`] and the
/// model id. genai routes by the model name - bring any key + an `AGENT_MODEL`.
pub struct GenaiProvider {
    client: Client,
    model: String,
}

impl GenaiProvider {
    /// Build the configured provider from the environment / local `.env`.
    /// Requires `AGENT_MODEL`; genai resolves the adapter, endpoint, and key from
    /// it (including private endpoints via `genai_N::`).
    pub fn from_env() -> Result<Self> {
        let _ = dotenvy::dotenv();
        let model = std::env::var("AGENT_MODEL").context(
            "AGENT_MODEL not set — the model id genai routes by, e.g. `claude-sonnet-4-6`, `gpt-4o`, \
             a namespaced `bedrock_api::anthropic.claude-...` / `open_router::openai/gpt-4.1`, or \
             `genai_1::<model>` for a private endpoint (GENAI_1_ENDPOINT + GENAI_1_API_KEY)",
        )?;
        Ok(Self {
            client: Client::default(),
            model,
        })
    }
}

#[async_trait]
impl Provider for GenaiProvider {
    /// A `(provider, model)` pair for status display: `provider` is the genai
    /// adapter the model routes to (`Anthropic`, `OpenAI`, `Bedrock`, ...),
    /// resolved from the model name by [`Client::default_model`] (the same
    /// name-sniffing / `namespace::` rule genai routes by - no network, no
    /// credentials); `model` is the configured `AGENT_MODEL`.
    fn status(&self) -> (String, String) {
        let provider = self
            .client
            .default_model(&self.model)
            .map(|iden| iden.adapter_kind.to_string())
            .unwrap_or_else(|_| "unknown".to_string());
        (provider, self.model.clone())
    }

    async fn complete(
        &self,
        system: &str,
        messages: &[ChatMessage],
        tools: &[Tool],
    ) -> Result<StreamEnd> {
        let mut req = ChatRequest::new(messages.to_vec());
        if !system.is_empty() {
            req = req.with_system(system);
        }
        if !tools.is_empty() {
            req = req.with_tools(tools.to_vec());
        }
        let opts = chat_options();
        let resp = self
            .client
            .exec_chat(self.model.as_str(), req, Some(&opts))
            .await?;
        Ok(StreamEnd {
            captured_usage: Some(resp.usage),
            captured_stop_reason: resp.stop_reason,
            captured_content: Some(resp.content),
            captured_reasoning_content: resp.reasoning_content,
            captured_response_id: resp.response_id,
        })
    }

    async fn stream<'a>(
        &'a self,
        system: &'a str,
        messages: &'a [ChatMessage],
        tools: &'a [Tool],
    ) -> Result<EventStream<'a>> {
        let mut req = ChatRequest::new(messages.to_vec());
        if !system.is_empty() {
            req = req.with_system(system);
        }
        if !tools.is_empty() {
            req = req.with_tools(tools.to_vec());
        }
        // One ephemeral cache breakpoint over the static prefix; capture the
        // assembled tool calls + usage off the terminal End event (the reply text
        // arrives live as Chunk events, so no need to capture content).
        let opts = chat_options();
        let resp = self
            .client
            .exec_chat_stream(self.model.as_str(), req, Some(&opts))
            .await?;
        Ok(resp
            .stream
            .map(|ev| ev.map_err(anyhow::Error::from))
            .boxed())
    }
}

fn chat_options() -> ChatOptions {
    ChatOptions::default()
        .with_max_tokens(MAX_TOKENS)
        .with_cache_control(CacheControl::Ephemeral)
        .with_capture_tool_calls(true)
        .with_capture_usage(true)
}

/// Reply text captured at stream end, all text parts concatenated. Empty when the
/// backend streamed text only as deltas (the agent reads those live instead).
pub fn completed_text(end: &StreamEnd) -> String {
    end.captured_texts()
        .map(|parts| parts.concat())
        .unwrap_or_default()
}

/// `(input, output, cache_write, cache_read)` token counts off a [`StreamEnd`].
///
/// genai reports each as `Option<i32>` and folds the providers'
/// `cache_creation_input_tokens` / `cache_read_input_tokens` (and the
/// Bedrock/OpenAI equivalents) into `prompt_tokens_details`; clamp negatives to
/// 0 and widen. `input` is the full prompt size and *includes* the two cache
/// counts.
pub fn token_usage(end: &StreamEnd) -> (u64, u64, u64, u64) {
    let Some(u) = end.captured_usage.as_ref() else {
        return (0, 0, 0, 0);
    };
    let widen = |n: Option<i32>| n.unwrap_or(0).max(0) as u64;
    let (cache_write, cache_read) = u
        .prompt_tokens_details
        .as_ref()
        .map(|d| (widen(d.cache_creation_tokens), widen(d.cached_tokens)))
        .unwrap_or((0, 0));
    (
        widen(u.prompt_tokens),
        widen(u.completion_tokens),
        cache_write,
        cache_read,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use genai::chat::{MessageContent, PromptTokensDetails, ToolCall, Usage};
    use serde_json::json;

    fn end_with_usage(prompt: Option<i32>, completion: Option<i32>) -> StreamEnd {
        StreamEnd {
            captured_usage: Some(Usage {
                prompt_tokens: prompt,
                completion_tokens: completion,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn token_usage_clamps_negatives_and_widens() {
        assert_eq!(
            token_usage(&end_with_usage(Some(10), Some(1))),
            (10, 1, 0, 0)
        );
        assert_eq!(token_usage(&end_with_usage(None, None)), (0, 0, 0, 0));
        assert_eq!(
            token_usage(&end_with_usage(Some(-1), Some(-5))),
            (0, 0, 0, 0)
        );
        assert_eq!(token_usage(&StreamEnd::default()), (0, 0, 0, 0));
    }

    #[test]
    fn token_usage_reads_cache_creation_and_read_counts() {
        let mut end = end_with_usage(Some(1000), Some(2));
        end.captured_usage.as_mut().unwrap().prompt_tokens_details = Some(PromptTokensDetails {
            cache_creation_tokens: Some(800),
            cached_tokens: Some(0),
            ..Default::default()
        });
        assert_eq!(
            token_usage(&end),
            (1000, 2, 800, 0),
            "first call: cache write"
        );

        end.captured_usage.as_mut().unwrap().prompt_tokens_details = Some(PromptTokensDetails {
            cache_creation_tokens: Some(0),
            cached_tokens: Some(800),
            ..Default::default()
        });
        assert_eq!(
            token_usage(&end),
            (1000, 2, 0, 800),
            "later call: cache read"
        );
    }

    #[test]
    fn completed_text_concatenates_captured_text_parts() {
        let end = StreamEnd {
            captured_content: Some(MessageContent::from_text("hello")),
            ..Default::default()
        };
        assert_eq!(completed_text(&end), "hello");
        assert_eq!(completed_text(&StreamEnd::default()), "");
    }

    #[test]
    fn completed_text_ignores_tool_call_parts() {
        let end = StreamEnd {
            captured_content: Some(MessageContent::from_tool_calls(vec![ToolCall {
                call_id: "c1".into(),
                fn_name: "create_design".into(),
                fn_arguments: json!({ "yaml": "version: 1" }),
                thought_signatures: None,
            }])),
            ..Default::default()
        };
        assert_eq!(
            completed_text(&end),
            "",
            "a tool-call-only completion has no text"
        );
    }
}
