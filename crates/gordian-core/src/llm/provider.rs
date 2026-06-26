//! The one production LLM, [`GenaiProvider`], built on the [`genai`] crate.
//!
//! [`GenaiProvider`] is a streaming [`Provider`](super::Provider) (it overrides
//! only [`Provider::stream`](super::Provider::stream); `complete` is derived by
//! draining it). genai is provider-agnostic — it picks the wire adapter from the
//! model name. Gordian supplies model/auth/endpoint from
//! [`crate::config::LlmConfig`], so core does not read provider environment
//! variables.
//!
//! Notable: one request-level `ephemeral` [`CacheControl`] breakpoint caches the
//! static system+tools prefix (genai routes it per adapter and folds the cache
//! token counts back into [`genai::chat::Usage::prompt_tokens_details`]).

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::StreamExt;

use genai::Client;
use genai::chat::{CacheControl, ChatMessage, ChatOptions, ChatRequest, StreamEnd, Tool};
use genai::resolver::{AuthData, Endpoint};

use super::seam::{EventStream, Provider};
use crate::config::LlmConfig;

/// The one production [`Provider`], over genai: the configured [`Client`] and the
/// model id. genai routes by the model name.
pub struct GenaiProvider {
    client: Client,
    model: String,
    max_tokens: u32,
    ephemeral_cache: bool,
}

impl GenaiProvider {
    /// Build the configured provider from typed config. Persistence and config
    /// source selection belong to the caller.
    pub fn from_config(config: &LlmConfig) -> Result<Self> {
        validate_llm_config(config)?;
        let model = config
            .model
            .clone()
            .context("llm.model not set in Gordian config")?;
        let api_key = config.api_key.clone();
        let endpoint = config.endpoint.clone();
        let mut builder = Client::builder().with_auth_resolver_fn(
            move |_model_iden| -> std::result::Result<Option<AuthData>, genai::resolver::Error> {
                Ok(Some(match &api_key {
                    Some(key) => AuthData::from_single(key.clone()),
                    None => AuthData::None,
                }))
            },
        );
        if let Some(endpoint) = endpoint {
            builder = builder.with_service_target_resolver_fn(
                move |mut target: genai::ServiceTarget| -> std::result::Result<
                    genai::ServiceTarget,
                    genai::resolver::Error,
                > {
                    target.endpoint = Endpoint::from_owned(endpoint.clone());
                    Ok(target)
                },
            );
        }
        Ok(Self {
            client: builder.build(),
            model,
            max_tokens: config.max_tokens,
            ephemeral_cache: config.ephemeral_cache,
        })
    }
}

#[async_trait]
impl Provider for GenaiProvider {
    /// A `(provider, model)` pair for status display: `provider` is the genai
    /// adapter the model routes to (`Anthropic`, `OpenAI`, `Bedrock`, ...),
    /// resolved from the model name by [`Client::default_model`] (the same
    /// name-sniffing / `namespace::` rule genai routes by - no network, no
    /// credentials); `model` is the configured `llm.model`.
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
        let opts = self.chat_options();
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
        let opts = self.chat_options();
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

impl GenaiProvider {
    fn chat_options(&self) -> ChatOptions {
        let mut opts = ChatOptions::default()
            .with_max_tokens(self.max_tokens)
            .with_capture_tool_calls(true)
            .with_capture_usage(true);
        if self.ephemeral_cache {
            opts = opts.with_cache_control(CacheControl::Ephemeral);
        }
        opts
    }
}

fn validate_llm_config(config: &LlmConfig) -> Result<()> {
    if config
        .model
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .is_empty()
    {
        anyhow::bail!("llm.model not set in Gordian config");
    }
    if config.max_tokens == 0 {
        anyhow::bail!("llm.maxTokens must be greater than zero");
    }
    if let Some(api_key) = &config.api_key
        && api_key.trim().is_empty()
    {
        anyhow::bail!("llm.apiKey must not be empty when set");
    }
    if let Some(endpoint) = &config.endpoint {
        let endpoint = endpoint.trim();
        if endpoint.is_empty() {
            anyhow::bail!("llm.endpoint must not be empty when set");
        }
        if !(endpoint.starts_with("http://") || endpoint.starts_with("https://")) {
            anyhow::bail!("llm.endpoint must start with http:// or https://");
        }
    }
    Ok(())
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
