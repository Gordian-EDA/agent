//! The single [`Provider`] backend, built on the [`genai`] crate.
//!
//! [`GenaiProvider`] serves all three configured backends behind one
//! implementation — the OpenAI-compatible gateway, the native Anthropic
//! Messages API, and AWS Bedrock (Converse) — by forcing genai's adapter,
//! endpoint, and auth through a [`ServiceTargetResolver`]. genai owns the wire
//! serialization; this module only maps Gordian's neutral conversation types
//! ([`Message`]/[`ContentBlock`]/[`ToolDef`]) onto genai's and back.
//!
//! ## Load-bearing quirks
//!
//! - **Gateway `thread_identifier`** — the respan gateway groups a thread's
//!   completions by a top-level `thread_identifier` field. genai's OpenAI
//!   adapter merges [`ChatOptions::with_extra_body`] into the request body
//!   top-level, so the id rides there (gateway backend only).
//! - **Assistant `content: ""`-not-null and object-or-string tool arguments**
//!   are handled natively by genai's OpenAI adapter — no workaround needed.
//! - **Tool-result images** ride a trailing `user` turn: genai's
//!   [`ToolResponse`] is text-only, so an image attached to a tool result is
//!   emitted as a binary [`ContentPart`] on a following user message — the same
//!   behavior the hand-rolled OpenAI client had.
//!
//! Streaming reads the *assembled* tool calls and usage off genai's terminal
//! [`StreamEnd`] event (`with_capture_tool_calls(true)` + `with_capture_usage`),
//! so no client-side tool-call delta assembly is needed.
//!
//! ## Prompt caching
//!
//! A multi-call agent turn (up to ~90 model calls) re-sends the same large,
//! unchanging prefix — the system prompt + the tool-def JSON — on every call.
//! Without caching that whole prefix is re-billed each time. [`chat_options`]
//! attaches a single request-level cache breakpoint via
//! [`ChatOptions::with_cache_control`]; genai routes it per backend:
//!
//! - **Anthropic** (native, and the gateway when it proxies Anthropic via the
//!   real Messages API): genai's Anthropic adapter auto-marks the *end of the
//!   static prefix* — the last system block, which caches tools + system
//!   together — with `cache_control: {type: "ephemeral"}`. The per-call-varying
//!   messages stay after the breakpoint, uncached. This is the priority backend
//!   and the one this breakpoint is shaped for.
//! - **Gateway** (genai's OpenAI adapter, `AdapterKind::OpenAI`): the same
//!   option maps to a top-level `prompt_cache_retention: "in_memory"` passthrough
//!   flag (alongside `thread_identifier`). OpenAI-style endpoints cache the
//!   common prefix automatically with no marker; the flag is the only knob the
//!   OpenAI wire shape exposes. genai's OpenAI adapter *cannot* emit the raw
//!   Anthropic `cache_control` content-block marker — so if the gateway proxies
//!   Anthropic, caching there depends on the gateway translating the flag (or
//!   caching by default), not on us injecting the Anthropic marker through this
//!   adapter.
//! - **Bedrock** (Converse): the option is carried through the same way and
//!   Bedrock reports `cacheReadInputTokens` / `cacheWriteInputTokens` back.
//!
//! Cache usage rides back on the response: genai folds Anthropic's
//! `cache_creation_input_tokens` / `cache_read_input_tokens` into
//! `Usage::prompt_tokens_details`, surfaced here as
//! [`Completion::cache_write_tokens`] / [`Completion::cache_read_tokens`] — a
//! cache-write on the first call and a cache-read on the rest of the turn.
//! `prompt_tokens` (our `input_tokens`) already includes both.

use anyhow::Result;
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream;
use serde_json::json;

use genai::adapter::AdapterKind;
use genai::chat::{
    CacheControl, ChatMessage, ChatOptions, ChatRequest, ChatStreamEvent, ContentPart,
    MessageContent, StreamEnd, Tool, ToolCall as GToolCall, ToolResponse, Usage,
};
use genai::resolver::{AuthData, Endpoint, ServiceTargetResolver};
use genai::{Client, Headers, ModelIden, ServiceTarget};

use crate::config::{AnthropicConfig, BedrockConfig, OpenAiConfig};
use crate::provider::{EventStream, Provider, StreamEvent};
use crate::types::{Completion, ContentBlock, Message, Role, ToolCall, ToolDef};

/// Which configured backend a [`GenaiProvider`] is wired to. Selects the genai
/// adapter and the request quirks (only the gateway carries `thread_identifier`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Backend {
    Gateway,
    Anthropic,
    Bedrock,
}

/// One [`Provider`] over genai, serving any of the three backends. Holds the
/// configured genai [`Client`] (with its [`ServiceTargetResolver`] baked in),
/// the model id, the request `max_tokens`, and a per-thread identifier for the
/// gateway's `thread_identifier` grouping.
pub struct GenaiProvider {
    client: Client,
    model: String,
    backend: Backend,
    max_tokens: u32,
    thread_id: String,
}

impl GenaiProvider {
    /// Build the gateway-backed provider. The base_url override and the custom
    /// bearer header are injected via a [`ServiceTargetResolver`]:
    /// [`AuthData::RequestOverride`] is genai's escape hatch for arbitrary auth
    /// headers — here a plain bearer key, with the `/chat/completions` URL fixed.
    pub fn gateway(config: OpenAiConfig) -> Self {
        let key = config.api_key.clone();
        let base = config.base_url.trim_end_matches('/').to_string();
        let resolver = ServiceTargetResolver::from_resolver_fn(
            move |target: ServiceTarget| -> genai::resolver::Result<ServiceTarget> {
                let ServiceTarget { model, .. } = target;
                let model = ModelIden::new(AdapterKind::OpenAI, model.model_name);
                let endpoint = Endpoint::from_owned(format!("{base}/"));
                let headers = Headers::from([("Authorization", format!("Bearer {key}"))]);
                let auth = AuthData::RequestOverride {
                    url: format!("{base}/chat/completions"),
                    headers,
                };
                Ok(ServiceTarget { endpoint, auth, model })
            },
        );
        Self::with_resolver(resolver, config.model, Backend::Gateway)
    }

    /// Build the native-Anthropic provider over genai's Anthropic adapter,
    /// injecting the API key explicitly.
    pub fn anthropic(config: AnthropicConfig) -> Self {
        let key = config.api_key.clone();
        let resolver = ServiceTargetResolver::from_resolver_fn(
            move |target: ServiceTarget| -> genai::resolver::Result<ServiceTarget> {
                let ServiceTarget { endpoint, model, .. } = target;
                let model = ModelIden::new(AdapterKind::Anthropic, model.model_name);
                Ok(ServiceTarget { endpoint, auth: AuthData::from_single(key.clone()), model })
            },
        );
        Self::with_resolver(resolver, config.model, Backend::Anthropic)
    }

    /// Build the AWS Bedrock provider over genai's `BedrockApiAdapter`
    /// ([`AdapterKind::BedrockApi`]) — bearer-token Converse, no AWS SDK / SigV4.
    pub fn bedrock(config: BedrockConfig) -> Self {
        let token = config.bearer_token.clone();
        let endpoint_url = config.endpoint();
        let resolver = ServiceTargetResolver::from_resolver_fn(
            move |target: ServiceTarget| -> genai::resolver::Result<ServiceTarget> {
                let ServiceTarget { model, .. } = target;
                let model = ModelIden::new(AdapterKind::BedrockApi, model.model_name);
                let endpoint = Endpoint::from_owned(endpoint_url.clone());
                Ok(ServiceTarget {
                    endpoint,
                    auth: AuthData::from_single(token.clone()),
                    model,
                })
            },
        );
        Self::with_resolver(resolver, config.model, Backend::Bedrock)
    }

    fn with_resolver(resolver: ServiceTargetResolver, model: String, backend: Backend) -> Self {
        let client = Client::builder().with_service_target_resolver(resolver).build();
        // 16k, not 4k: a single create_design carries the whole circuit YAML, which for a large
        // board exceeds 4k tokens and gets TRUNCATED — the truncated args then fail to parse and
        // the yaml silently vanishes. The configured models allow >= 16k.
        Self { client, model, backend, max_tokens: 16384, thread_id: random_thread_id() }
    }

    fn chat_options(&self) -> ChatOptions {
        // One request-level cache breakpoint at the end of the stable prefix
        // (system + tool defs). genai's Anthropic adapter turns this into a
        // `cache_control: {type: "ephemeral"}` marker on the last system block —
        // so the multi-KB prefix is billed once per turn, not once per call,
        // and the per-call-varying messages after it stay uncached. The OpenAI
        // gateway maps it to a `prompt_cache_retention` flag; Bedrock carries it
        // through. See the module docs for the per-backend story.
        let mut opts =
            ChatOptions::default().with_max_tokens(self.max_tokens).with_cache_control(CacheControl::Ephemeral);
        // The respan gateway groups this thread's completions by a top-level
        // `thread_identifier`. genai's OpenAI adapter merges extra_body into the
        // request body top-level, so the id lands where the gateway expects it.
        if self.backend == Backend::Gateway {
            opts = opts.with_extra_body(json!({ "thread_identifier": self.thread_id }));
        }
        opts
    }

    fn stream_options(&self) -> ChatOptions {
        // Capture the assembled tool calls + usage on the terminal End event —
        // genai accumulates the SSE tool-call fragments for us.
        self.chat_options().with_capture_tool_calls(true).with_capture_usage(true)
    }

    fn build_request(&self, system: &str, messages: &[Message], tools: &[ToolDef]) -> ChatRequest {
        let mut req = ChatRequest::new(to_genai_messages(messages));
        if !system.is_empty() {
            req = req.with_system(system);
        }
        if !tools.is_empty() {
            req = req.with_tools(tools.iter().map(to_genai_tool).collect::<Vec<_>>());
        }
        req
    }
}

#[async_trait]
impl Provider for GenaiProvider {
    async fn complete(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDef],
    ) -> Result<Completion> {
        let req = self.build_request(system, messages, tools);
        let opts = self.chat_options();
        let resp = self.client.exec_chat(self.model.as_str(), req, Some(&opts)).await?;

        let text = resp.texts().join("");
        let tool_calls = resp.tool_calls().into_iter().map(from_genai_tool_call).collect();
        let stop_reason = resp.stop_reason.as_ref().map(|s| s.raw().to_string()).unwrap_or_default();
        let (input_tokens, output_tokens) = usage_tokens(
            resp.usage.prompt_tokens,
            resp.usage.completion_tokens,
        );
        let (cache_write_tokens, cache_read_tokens) = cache_tokens(&resp.usage);

        Ok(Completion {
            text,
            tool_calls,
            stop_reason,
            input_tokens,
            output_tokens,
            cache_write_tokens,
            cache_read_tokens,
        })
    }

    async fn stream<'a>(
        &'a self,
        system: &'a str,
        messages: &'a [Message],
        tools: &'a [ToolDef],
    ) -> Result<EventStream<'a>> {
        let req = self.build_request(system, messages, tools);
        let opts = self.stream_options();
        let resp = self.client.exec_chat_stream(self.model.as_str(), req, Some(&opts)).await?;

        // Drive genai's event stream lazily: emit a `TextDelta` per assistant
        // chunk and one terminal `Completed` (assembled tool calls + usage read
        // off the `End` event). `ToolCallChunk` events are absorbed — the
        // neutral stream surfaces tool calls only on the final `Completed`.
        let events = stream::unfold(StreamState::Running(resp.stream, String::new()), step);
        Ok(events.boxed())
    }

    fn new_thread(&mut self) {
        self.thread_id = random_thread_id();
    }
}

/// State threaded through [`stream::unfold`] while draining genai's stream:
/// either still running (carrying the live stream + accumulated text) or done.
enum StreamState {
    Running(genai::chat::ChatStream, String),
    Done,
}

/// One `unfold` step: pull genai events until there is a [`StreamEvent`] to
/// yield (a text delta or the terminal completion) or the stream ends.
async fn step(state: StreamState) -> Option<(Result<StreamEvent>, StreamState)> {
    let (mut stream, mut text) = match state {
        StreamState::Running(s, t) => (s, t),
        StreamState::Done => return None,
    };
    loop {
        match stream.next().await {
            Some(Ok(ChatStreamEvent::Chunk(chunk))) => {
                text.push_str(&chunk.content);
                let delta = StreamEvent::TextDelta(chunk.content);
                return Some((Ok(delta), StreamState::Running(stream, text)));
            }
            Some(Ok(ChatStreamEvent::End(end))) => {
                let completion = stream_end_to_completion(end, std::mem::take(&mut text));
                return Some((Ok(StreamEvent::Completed(completion)), StreamState::Done));
            }
            // Tool-call chunks, reasoning, start markers: keep draining.
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Some((Err(e.into()), StreamState::Done)),
            None => return None,
        }
    }
}

// ============================================================================
// Mapping helpers: Gordian neutral types <-> genai types.
// ============================================================================

fn to_genai_tool(t: &ToolDef) -> Tool {
    Tool::new(t.name.clone())
        .with_description(t.description.clone())
        .with_schema(t.input_schema.clone())
}

fn from_genai_tool_call(tc: &GToolCall) -> ToolCall {
    ToolCall { id: tc.call_id.clone(), name: tc.fn_name.clone(), input: tc.fn_arguments.clone() }
}

/// genai reports token usage as `Option<i32>`; clamp negatives to 0 and widen.
fn usage_tokens(prompt: Option<i32>, completion: Option<i32>) -> (u64, u64) {
    (prompt.unwrap_or(0).max(0) as u64, completion.unwrap_or(0).max(0) as u64)
}

/// Extract `(cache_write, cache_read)` token counts from genai's [`Usage`].
///
/// genai folds Anthropic's `cache_creation_input_tokens` /
/// `cache_read_input_tokens` (and the Bedrock/OpenAI equivalents) into
/// `prompt_tokens_details`. Absent when the backend reports no caching.
fn cache_tokens(usage: &Usage) -> (u64, u64) {
    usage
        .prompt_tokens_details
        .as_ref()
        .map(|d| {
            (
                d.cache_creation_tokens.unwrap_or(0).max(0) as u64,
                d.cached_tokens.unwrap_or(0).max(0) as u64,
            )
        })
        .unwrap_or((0, 0))
}

/// Map Gordian messages onto genai's [`ChatMessage`] list.
///
/// A user turn carrying tool results expands into a `tool` message (one
/// [`ToolResponse`] per result) plus — for any images those results attached —
/// a trailing `user` message of binary parts, since genai's `ToolResponse` is
/// text-only. This reproduces the load-bearing behavior of the prior client.
fn to_genai_messages(messages: &[Message]) -> Vec<ChatMessage> {
    let mut out: Vec<ChatMessage> = Vec::new();
    for m in messages {
        match m.role {
            Role::Assistant => {
                let mut parts: Vec<ContentPart> = Vec::new();
                for b in &m.content {
                    match b {
                        ContentBlock::Text(t) => parts.push(ContentPart::from_text(t.clone())),
                        ContentBlock::ToolUse { id, name, input } => {
                            parts.push(ContentPart::ToolCall(GToolCall {
                                call_id: id.clone(),
                                fn_name: name.clone(),
                                fn_arguments: input.clone(),
                                thought_signatures: None,
                            }));
                        }
                        ContentBlock::ToolResult { .. } => {}
                    }
                }
                out.push(ChatMessage::assistant(MessageContent::from_parts(parts)));
            }
            Role::User => {
                let mut user_parts: Vec<ContentPart> = Vec::new();
                let mut tool_parts: Vec<ContentPart> = Vec::new();
                let mut trailing_images: Vec<ContentPart> = Vec::new();
                for b in &m.content {
                    match b {
                        ContentBlock::Text(t) => user_parts.push(ContentPart::from_text(t.clone())),
                        ContentBlock::ToolResult { tool_use_id, content, images } => {
                            tool_parts.push(ContentPart::ToolResponse(ToolResponse::new(
                                tool_use_id.clone(),
                                content.clone(),
                            )));
                            for img in images {
                                trailing_images.push(ContentPart::from_binary_base64(
                                    format!("image/{}", img.format),
                                    img.base64.clone(),
                                    None,
                                ));
                            }
                        }
                        ContentBlock::ToolUse { .. } => {}
                    }
                }
                if !tool_parts.is_empty() {
                    out.push(ChatMessage::tool(MessageContent::from_parts(tool_parts)));
                }
                user_parts.extend(trailing_images);
                if !user_parts.is_empty() {
                    out.push(ChatMessage::user(MessageContent::from_parts(user_parts)));
                }
            }
        }
    }
    out
}

/// Fold genai's terminal [`StreamEnd`] into a [`Completion`]: the captured tool
/// calls, stop reason, and usage are read off it; `text` is the deltas we
/// concatenated while streaming.
fn stream_end_to_completion(end: StreamEnd, text: String) -> Completion {
    let tool_calls = end
        .captured_tool_calls()
        .map(|calls| calls.into_iter().map(from_genai_tool_call).collect())
        .unwrap_or_default();
    let stop_reason =
        end.captured_stop_reason.as_ref().map(|s| s.raw().to_string()).unwrap_or_default();
    let (input_tokens, output_tokens) = end
        .captured_usage
        .as_ref()
        .map(|u| usage_tokens(u.prompt_tokens, u.completion_tokens))
        .unwrap_or((0, 0));
    let (cache_write_tokens, cache_read_tokens) =
        end.captured_usage.as_ref().map(cache_tokens).unwrap_or((0, 0));
    Completion {
        text,
        tool_calls,
        stop_reason,
        input_tokens,
        output_tokens,
        cache_write_tokens,
        cache_read_tokens,
    }
}

/// A fresh random id for a new thread (one conversation/session). Random — not
/// derived from time or content — so distinct threads never collide. Adequate
/// for the gateway's thread-grouping; not a security token.
fn random_thread_id() -> String {
    let (a, b) = (fastrand::u64(..), fastrand::u64(..));
    format!(
        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        (a & 0xffffffff) as u32,
        ((a >> 32) & 0xffff) as u16,
        ((a >> 48) & 0xfff) as u16,
        (b & 0xfff) as u16,
        (b >> 12) & 0xffffffffffff
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ImageData;
    use serde_json::Value;

    /// The serialized role label of a genai message (`User`/`Assistant`/`Tool`/
    /// `System` — genai serializes the variant name verbatim).
    fn role_of(m: &ChatMessage) -> String {
        let v = serde_json::to_value(m).unwrap();
        v.get("role").and_then(Value::as_str).unwrap_or_default().to_string()
    }

    #[test]
    fn assistant_tool_use_maps_to_a_genai_tool_call_part() {
        let messages = vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "tu_1".to_string(),
                name: "apply_design".to_string(),
                input: json!({ "yaml": "..." }),
            }],
        }];
        let out = to_genai_messages(&messages);
        assert_eq!(out.len(), 1);
        assert_eq!(role_of(&out[0]), "Assistant");
        // The genai ToolCall round-trips back to our neutral ToolCall shape.
        let v = serde_json::to_value(&out[0]).unwrap();
        let body = v.to_string();
        assert!(body.contains("apply_design"), "tool name preserved: {body}");
        assert!(body.contains("tu_1"), "call id preserved: {body}");
    }

    #[test]
    fn tool_result_becomes_tool_message_and_images_ride_trailing_user_turn() {
        let messages = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tu_9".to_string(),
                content: "{\"ok\":true}".to_string(),
                images: vec![ImageData {
                    format: "png".to_string(),
                    base64: "aGVsbG8=".to_string(),
                }],
            }],
        }];
        let out = to_genai_messages(&messages);
        // [0] = tool message (the tool response), [1] = user message (the image).
        assert_eq!(out.len(), 2);
        assert_eq!(role_of(&out[0]), "Tool");
        assert_eq!(role_of(&out[1]), "User");
    }

    #[test]
    fn tool_def_maps_name_description_and_schema() {
        let tool = ToolDef {
            name: "get_design".to_string(),
            description: "fetch the current design".to_string(),
            input_schema: json!({ "type": "object", "properties": {} }),
        };
        let g = to_genai_tool(&tool);
        let v = serde_json::to_value(&g).unwrap();
        assert_eq!(v.get("name").and_then(Value::as_str), Some("get_design"));
        assert_eq!(
            v.get("description").and_then(Value::as_str),
            Some("fetch the current design")
        );
        assert_eq!(v["schema"]["type"], "object");
    }

    #[test]
    fn genai_tool_call_lifts_to_neutral_tool_call() {
        let g = GToolCall {
            call_id: "call_abc".to_string(),
            fn_name: "search_symbols".to_string(),
            fn_arguments: json!({ "query": "STM32H743" }),
            thought_signatures: None,
        };
        let lifted = from_genai_tool_call(&g);
        assert_eq!(lifted.id, "call_abc");
        assert_eq!(lifted.name, "search_symbols");
        assert_eq!(lifted.input["query"], "STM32H743");
    }

    #[test]
    fn usage_tokens_clamps_negatives_and_widens() {
        assert_eq!(usage_tokens(Some(10), Some(1)), (10, 1));
        assert_eq!(usage_tokens(None, None), (0, 0));
        assert_eq!(usage_tokens(Some(-1), Some(-5)), (0, 0));
    }

    #[test]
    fn chat_options_carry_the_ephemeral_cache_breakpoint() {
        // For the cache-capable Anthropic backend, the request options must
        // carry one ephemeral cache_control breakpoint — genai's Anthropic
        // adapter turns this into the `cache_control: {type: "ephemeral"}` marker
        // on the last system block (the end of the system + tools prefix).
        let provider = GenaiProvider::anthropic(AnthropicConfig {
            api_key: "k".to_string(),
            model: "claude-opus-4-8".to_string(),
        });
        let opts = provider.chat_options();
        assert_eq!(opts.cache_control, Some(CacheControl::Ephemeral));
        // Serialized form is what genai consumes; the marker must be present.
        let v = serde_json::to_value(&opts).unwrap();
        assert_eq!(v.get("cache_control").and_then(Value::as_str), Some("Ephemeral"));
        // Streaming reuses the same options, so it carries the breakpoint too.
        assert_eq!(provider.stream_options().cache_control, Some(CacheControl::Ephemeral));
    }

    #[test]
    fn cache_tokens_reads_creation_and_read_counts() {
        use genai::chat::PromptTokensDetails;
        let usage = Usage {
            prompt_tokens: Some(1000),
            prompt_tokens_details: Some(PromptTokensDetails {
                cache_creation_tokens: Some(800),
                cached_tokens: Some(0),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(cache_tokens(&usage), (800, 0), "first call: cache write");

        let usage = Usage {
            prompt_tokens: Some(1000),
            prompt_tokens_details: Some(PromptTokensDetails {
                cache_creation_tokens: Some(0),
                cached_tokens: Some(800),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(cache_tokens(&usage), (0, 800), "later call: cache read");

        // No caching reported → zeros, not a panic.
        assert_eq!(cache_tokens(&Usage::default()), (0, 0));
    }

    #[test]
    fn thread_id_is_random_and_uuid_shaped() {
        let a = random_thread_id();
        let b = random_thread_id();
        assert_ne!(a, b, "distinct threads get distinct ids");
        assert_eq!(a.len(), 36, "uuid-shaped");
        assert_eq!(a.matches('-').count(), 4);
    }

    use genai::chat::{MessageContent, Usage};

    fn stream_end(
        content: Option<MessageContent>,
        stop: Option<&str>,
        prompt: Option<i32>,
        completion: Option<i32>,
    ) -> StreamEnd {
        StreamEnd {
            captured_content: content,
            captured_stop_reason: stop.map(|s| s.to_string().into()),
            captured_usage: Some(Usage {
                prompt_tokens: prompt,
                completion_tokens: completion,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn text_completion_lifts_to_neutral_completion_with_tokens() {
        let end = stream_end(
            Some(MessageContent::from_text("42")),
            Some("end_turn"),
            Some(10),
            Some(1),
        );
        let c = stream_end_to_completion(end, "42".to_string());
        assert_eq!(c.text, "42");
        assert!(c.tool_calls.is_empty());
        // StopReason::raw() preserves the wire string verbatim.
        assert_eq!(c.stop_reason, "end_turn");
        assert_eq!((c.input_tokens, c.output_tokens), (10, 1));
    }

    #[test]
    fn tool_call_completion_extracts_object_form_arguments() {
        // genai keeps tool arguments as a structured Value (object form here),
        // which lifts straight onto our neutral ToolCall.input.
        let call = GToolCall {
            call_id: "call_xyz".to_string(),
            fn_name: "create_design".to_string(),
            fn_arguments: json!({ "yaml": "version: 1" }),
            thought_signatures: None,
        };
        let end =
            stream_end(Some(MessageContent::from_tool_calls(vec![call])), Some("tool_use"), None, None);
        let c = stream_end_to_completion(end, String::new());
        assert_eq!(c.stop_reason, "tool_use");
        assert_eq!(c.tool_calls.len(), 1);
        assert_eq!(c.tool_calls[0].name, "create_design");
        assert_eq!(c.tool_calls[0].input["yaml"], "version: 1");
    }

    #[test]
    fn missing_usage_and_stop_reason_default_to_zero_and_empty() {
        // No captured content, usage, or stop reason — a degenerate End.
        let c = stream_end_to_completion(StreamEnd::default(), String::new());
        assert_eq!(c.text, "");
        assert!(c.tool_calls.is_empty());
        assert_eq!(c.stop_reason, "");
        assert_eq!((c.input_tokens, c.output_tokens), (0, 0));
    }
}
