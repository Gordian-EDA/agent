//! Provider-agnostic LLM client.
//!
//! The public types ([`Message`], [`ContentBlock`], [`ToolDef`], [`ToolCall`],
//! [`Completion`]) and the [`LlmClient`] trait are independent of any vendor.
//! [`BedrockClient`] is the one concrete implementation: a thin client over the
//! AWS Bedrock **Converse** API, authenticated with a bearer token (not SigV4).
//!
//! The Converse wire format is AWS-defined and provider-neutral; the request
//! shape here mirrors the proven Python spike:
//!
//! ```text
//! POST https://bedrock-runtime.{region}.amazonaws.com/model/{url-encoded id}/converse
//! Authorization: Bearer {token}
//! body: { system, messages, inferenceConfig, toolConfig? }
//! ```

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::config::{Config, OpenAiConfig, Provider};

/// Build the configured client from the environment / local `.env`, selecting
/// the backend via [`crate::config::selected_provider`] (OpenAI-compatible when
/// `OPENAI_API_KEY` is set, else AWS Bedrock). Returned boxed behind the
/// [`LlmClient`] trait so callers are provider-agnostic.
pub fn from_env() -> Result<Box<dyn LlmClient>> {
    match crate::config::selected_provider() {
        Provider::OpenAi => Ok(Box::new(OpenAiClient::from_env()?)),
        Provider::Bedrock => Ok(Box::new(BedrockClient::from_env()?)),
    }
}

/// Conversation role.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    fn as_wire(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

/// An image attached to a tool result, already base64-encoded for the wire.
#[derive(Clone, Debug, PartialEq)]
pub struct ImageData {
    /// Converse wire format identifier: "png", "jpeg", "gif", or "webp".
    pub format: String,
    /// Base64-encoded image bytes.
    pub base64: String,
}

/// A single block of message content.
#[derive(Clone, Debug, PartialEq)]
pub enum ContentBlock {
    /// Plain text.
    Text(String),
    /// A tool invocation the assistant requested.
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    /// The result of running a tool, fed back to the model.
    ToolResult {
        tool_use_id: String,
        content: String,
        /// Images attached to the result (rendered schematics). Empty for
        /// text-only results.
        images: Vec<ImageData>,
    },
}

/// A message in the conversation.
#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    /// Build a single-text user message.
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text(text.into())],
        }
    }

    /// Build a single-text assistant message.
    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: vec![ContentBlock::Text(text.into())],
        }
    }
}

/// A tool the model may call.
#[derive(Clone, Debug)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema describing the tool's input.
    pub input_schema: Value,
}

/// A tool call extracted from a completion.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
}

/// The parsed result of a single completion request.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Completion {
    /// All text blocks, concatenated.
    pub text: String,
    /// Any tool calls the model requested.
    pub tool_calls: Vec<ToolCall>,
    /// Raw stop reason from the provider (e.g. `end_turn`, `tool_use`).
    pub stop_reason: String,
    /// Prompt tokens the provider reports for this call (0 when absent).
    /// This is the size of everything sent: system + history + tools.
    pub input_tokens: u64,
    /// Generated tokens the provider reports for this call (0 when absent).
    pub output_tokens: u64,
}

/// Provider-agnostic completion interface.
#[async_trait]
pub trait LlmClient: Send + Sync {
    /// Run one completion. `system` is the system prompt; `messages` is the
    /// conversation; `tools` may be empty.
    async fn complete(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDef],
    ) -> Result<Completion>;
}

/// Thin AWS Bedrock Converse client using a bearer token.
pub struct BedrockClient {
    config: Config,
    http: reqwest::Client,
    /// Max output tokens per request.
    max_tokens: u32,
    /// Sampling temperature.
    temperature: f32,
}

impl BedrockClient {
    /// Build a client from an explicit [`Config`].
    pub fn new(config: Config) -> Result<Self> {
        let http = reqwest::Client::builder()
            .build()
            .context("building reqwest client")?;
        Ok(Self {
            config,
            http,
            max_tokens: 4096,
            temperature: 0.0,
        })
    }

    /// Build a client from the environment / local `.env`.
    pub fn from_env() -> Result<Self> {
        Self::new(Config::from_env()?)
    }

    /// Override the max output tokens (builder-style).
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Override the sampling temperature (builder-style).
    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = temperature;
        self
    }

    /// Build the Converse request body for the given inputs. Separated out so
    /// it can be unit-tested without a network call.
    fn build_request(&self, system: &str, messages: &[Message], tools: &[ToolDef]) -> Value {
        let wire_messages: Vec<Value> = messages.iter().map(message_to_wire).collect();

        let mut body = json!({
            "system": [{ "text": system }],
            "messages": wire_messages,
            "inferenceConfig": {
                "maxTokens": self.max_tokens,
                "temperature": self.temperature,
            },
        });

        if !tools.is_empty() {
            let tool_specs: Vec<Value> = tools
                .iter()
                .map(|t| {
                    json!({
                        "toolSpec": {
                            "name": t.name,
                            "description": t.description,
                            "inputSchema": { "json": t.input_schema },
                        }
                    })
                })
                .collect();
            body["toolConfig"] = json!({ "tools": tool_specs });
        }

        body
    }
}

#[async_trait]
impl LlmClient for BedrockClient {
    async fn complete(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDef],
    ) -> Result<Completion> {
        let body = self.build_request(system, messages, tools);
        let url = self.config.converse_url();

        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.config.bearer_token)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("sending Bedrock Converse request")?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .context("reading Bedrock Converse response body")?;

        if !status.is_success() {
            // The body may carry a useful error message but never the token.
            bail!("Bedrock Converse returned HTTP {status}: {text}");
        }

        let value: Value =
            serde_json::from_str(&text).context("parsing Bedrock Converse response JSON")?;
        parse_completion(&value)
    }
}

/// Thin client over any OpenAI-compatible **chat completions** endpoint, using
/// an API key as a bearer token. Speaks the standard `/chat/completions` shape,
/// so it works against OpenAI itself or a proxy (the configured gateway proxies
/// Anthropic models). The provider-neutral [`Message`]/[`ContentBlock`] types
/// map onto OpenAI roles: a [`ContentBlock::ToolResult`] becomes a `tool`
/// message (with any attached images riding on a trailing `user` message, since
/// OpenAI `tool` messages are text-only).
pub struct OpenAiClient {
    config: OpenAiConfig,
    http: reqwest::Client,
    max_tokens: u32,
    /// Sampling temperature, sent only when set. Omitted by default: newer
    /// Anthropic models (proxied through the gateway) reject `temperature` as
    /// deprecated, so the safe default is to not send it at all.
    temperature: Option<f32>,
}

impl OpenAiClient {
    /// Build from an explicit [`OpenAiConfig`].
    pub fn new(config: OpenAiConfig) -> Result<Self> {
        let http = reqwest::Client::builder().build().context("building reqwest client")?;
        Ok(Self { config, http, max_tokens: 4096, temperature: None })
    }

    /// Build from the environment / local `.env`.
    pub fn from_env() -> Result<Self> {
        Self::new(OpenAiConfig::from_env()?)
    }

    /// Override the max output tokens (builder-style).
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Set the sampling temperature (builder-style). Left unset by default — see
    /// [`OpenAiClient::temperature`].
    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = Some(temperature);
        self
    }

    /// Build the chat-completions request body. Separated for unit testing.
    fn build_request(&self, system: &str, messages: &[Message], tools: &[ToolDef]) -> Value {
        let mut body = json!({
            "model": self.config.model,
            "messages": messages_to_openai(system, messages),
            "max_tokens": self.max_tokens,
        });
        if let Some(t) = self.temperature {
            body["temperature"] = json!(t);
        }
        if !tools.is_empty() {
            let specs: Vec<Value> = tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.input_schema,
                        }
                    })
                })
                .collect();
            body["tools"] = json!(specs);
            body["tool_choice"] = json!("auto");
        }
        body
    }
}

#[async_trait]
impl LlmClient for OpenAiClient {
    async fn complete(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDef],
    ) -> Result<Completion> {
        let body = self.build_request(system, messages, tools);
        let resp = self
            .http
            .post(self.config.chat_url())
            .bearer_auth(&self.config.api_key)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("sending OpenAI chat-completions request")?;

        let status = resp.status();
        let text = resp.text().await.context("reading OpenAI response body")?;
        if !status.is_success() {
            // The body may carry a useful error message but never the key.
            bail!("OpenAI chat-completions returned HTTP {status}: {text}");
        }
        let value: Value =
            serde_json::from_str(&text).context("parsing OpenAI chat-completions response JSON")?;
        parse_openai_completion(&value)
    }
}

/// Map the provider-neutral system prompt + messages onto the OpenAI chat wire.
/// One input [`Message`] may expand into several wire messages: a `user` turn
/// carrying tool results becomes one `tool` message per result, plus a trailing
/// `user` message for any images those results attached.
fn messages_to_openai(system: &str, messages: &[Message]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    if !system.is_empty() {
        out.push(json!({ "role": "system", "content": system }));
    }
    for m in messages {
        match m.role {
            Role::Assistant => {
                let mut text = String::new();
                let mut tool_calls: Vec<Value> = Vec::new();
                for b in &m.content {
                    match b {
                        ContentBlock::Text(t) => text.push_str(t),
                        ContentBlock::ToolUse { id, name, input } => tool_calls.push(json!({
                            "id": id,
                            "type": "function",
                            "function": { "name": name, "arguments": input.to_string() },
                        })),
                        ContentBlock::ToolResult { .. } => {}
                    }
                }
                let mut msg = json!({ "role": "assistant" });
                // `content` must be present; null is allowed alongside tool_calls.
                msg["content"] = if text.is_empty() { Value::Null } else { json!(text) };
                if !tool_calls.is_empty() {
                    msg["tool_calls"] = json!(tool_calls);
                }
                out.push(msg);
            }
            Role::User => {
                let mut parts: Vec<Value> = Vec::new();
                let mut images: Vec<Value> = Vec::new();
                for b in &m.content {
                    match b {
                        ContentBlock::Text(t) => parts.push(json!({ "type": "text", "text": t })),
                        ContentBlock::ToolResult { tool_use_id, content, images: imgs } => {
                            out.push(json!({
                                "role": "tool",
                                "tool_call_id": tool_use_id,
                                "content": content,
                            }));
                            for img in imgs {
                                images.push(json!({
                                    "type": "image_url",
                                    "image_url": {
                                        "url": format!("data:image/{};base64,{}", img.format, img.base64),
                                    },
                                }));
                            }
                        }
                        ContentBlock::ToolUse { .. } => {}
                    }
                }
                // Tool-result images must follow the tool messages, on a user turn.
                parts.extend(images);
                if !parts.is_empty() {
                    out.push(json!({ "role": "user", "content": parts }));
                }
            }
        }
    }
    out
}

/// Extract a [`Completion`] from a parsed chat-completions response. Separated
/// out so it can be unit-tested against a captured response string.
pub(crate) fn parse_openai_completion(value: &Value) -> Result<Completion> {
    let message = value
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .and_then(|c| c.get("message"))
        .ok_or_else(|| anyhow!("chat-completions response missing choices[0].message"))?;

    // `content` is usually a string, but some gateways return an array of parts.
    let text = match message.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<String>(),
        _ => String::new(),
    };

    let mut tool_calls = Vec::new();
    if let Some(calls) = message.get("tool_calls").and_then(|t| t.as_array()) {
        for call in calls {
            let id = call.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
            let func = call.get("function");
            let name = func
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let args = func.and_then(|f| f.get("arguments")).and_then(Value::as_str).unwrap_or("{}");
            // Arguments arrive as a JSON string; parse to the structured input.
            let input = serde_json::from_str(args).unwrap_or(Value::Null);
            tool_calls.push(ToolCall { id, name, input });
        }
    }

    let stop_reason = value
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .and_then(|c| c.get("finish_reason"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let usage = |key: &str| {
        value.get("usage").and_then(|u| u.get(key)).and_then(Value::as_u64).unwrap_or(0)
    };

    Ok(Completion {
        text,
        tool_calls,
        stop_reason,
        input_tokens: usage("prompt_tokens"),
        output_tokens: usage("completion_tokens"),
    })
}

/// Map one [`Message`] to the Converse wire shape.
fn message_to_wire(message: &Message) -> Value {
    let content: Vec<Value> = message.content.iter().map(content_block_to_wire).collect();
    json!({
        "role": message.role.as_wire(),
        "content": content,
    })
}

/// Map one [`ContentBlock`] to the Converse wire shape.
fn content_block_to_wire(block: &ContentBlock) -> Value {
    match block {
        ContentBlock::Text(t) => json!({ "text": t }),
        ContentBlock::ToolUse { id, name, input } => json!({
            "toolUse": {
                "toolUseId": id,
                "name": name,
                "input": input,
            }
        }),
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            images,
        } => {
            let mut blocks = vec![json!({ "text": content })];
            for img in images {
                blocks.push(json!({
                    "image": {
                        "format": img.format,
                        "source": { "bytes": img.base64 },
                    }
                }));
            }
            json!({
                "toolResult": {
                    "toolUseId": tool_use_id,
                    "content": blocks,
                }
            })
        }
    }
}

/// Extract a [`Completion`] from a parsed Converse response. Separated out so
/// it can be unit-tested against a captured response string.
pub(crate) fn parse_completion(value: &Value) -> Result<Completion> {
    let content = value
        .get("output")
        .and_then(|o| o.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
        .ok_or_else(|| anyhow!("Converse response missing output.message.content"))?;

    let mut text = String::new();
    let mut tool_calls = Vec::new();

    for block in content {
        if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
            text.push_str(t);
        } else if let Some(tool_use) = block.get("toolUse") {
            let id = tool_use
                .get("toolUseId")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let name = tool_use
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let input = tool_use.get("input").cloned().unwrap_or(Value::Null);
            tool_calls.push(ToolCall { id, name, input });
        }
    }

    let stop_reason = value
        .get("stopReason")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let usage = |key: &str| {
        value
            .get("usage")
            .and_then(|u| u.get(key))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };

    Ok(Completion {
        text,
        tool_calls,
        stop_reason,
        input_tokens: usage("inputTokens"),
        output_tokens: usage("outputTokens"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> BedrockClient {
        BedrockClient::new(Config {
            bearer_token: "test-token".to_string(),
            region: "us-east-1".to_string(),
            model: crate::config::DEFAULT_MODEL.to_string(),
        })
        .unwrap()
    }

    #[test]
    fn request_has_system_and_messages() {
        let c = client();
        let body = c.build_request("be terse", &[Message::user("6 times 7")], &[]);

        assert_eq!(body["system"][0]["text"], "be terse");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"][0]["text"], "6 times 7");
        assert_eq!(body["inferenceConfig"]["maxTokens"], 4096);
        // No tools → no toolConfig key.
        assert!(body.get("toolConfig").is_none());
    }

    #[test]
    fn request_includes_tool_config_when_tools_present() {
        let c = client();
        let tool = ToolDef {
            name: "get_design".to_string(),
            description: "fetch the current design".to_string(),
            input_schema: json!({ "type": "object", "properties": {} }),
        };
        let body = c.build_request("sys", &[Message::user("hi")], std::slice::from_ref(&tool));

        let spec = &body["toolConfig"]["tools"][0]["toolSpec"];
        assert_eq!(spec["name"], "get_design");
        assert_eq!(spec["description"], "fetch the current design");
        assert_eq!(spec["inputSchema"]["json"]["type"], "object");
    }

    #[test]
    fn assistant_tool_use_and_user_tool_result_map_to_wire() {
        let c = client();
        let messages = vec![
            Message::user("add R1"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "tu_1".to_string(),
                    name: "apply_design".to_string(),
                    input: json!({ "yaml": "..." }),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "tu_1".to_string(),
                    content: "ok".to_string(),
                    images: Vec::new(),
                }],
            },
        ];
        let body = c.build_request("sys", &messages, &[]);

        let assistant = &body["messages"][1];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["content"][0]["toolUse"]["toolUseId"], "tu_1");
        assert_eq!(assistant["content"][0]["toolUse"]["name"], "apply_design");
        assert_eq!(assistant["content"][0]["toolUse"]["input"]["yaml"], "...");

        let user = &body["messages"][2];
        assert_eq!(user["role"], "user");
        assert_eq!(user["content"][0]["toolResult"]["toolUseId"], "tu_1");
        assert_eq!(user["content"][0]["toolResult"]["content"][0]["text"], "ok");
    }

    #[test]
    fn parses_text_completion() {
        // A captured-shape Converse response with a plain text answer.
        let raw = json!({
            "output": {
                "message": {
                    "role": "assistant",
                    "content": [{ "text": "42" }]
                }
            },
            "stopReason": "end_turn",
            "usage": { "inputTokens": 10, "outputTokens": 1 }
        });
        let completion = parse_completion(&raw).unwrap();
        assert_eq!(completion.text, "42");
        assert!(completion.tool_calls.is_empty());
        assert_eq!(completion.stop_reason, "end_turn");
        assert_eq!(completion.input_tokens, 10);
        assert_eq!(completion.output_tokens, 1);
    }

    #[test]
    fn missing_usage_defaults_to_zero_tokens() {
        let raw = json!({
            "output": { "message": { "role": "assistant", "content": [{ "text": "hi" }] } },
            "stopReason": "end_turn"
        });
        let completion = parse_completion(&raw).unwrap();
        assert_eq!(completion.input_tokens, 0);
        assert_eq!(completion.output_tokens, 0);
    }

    #[test]
    fn parses_tool_use_completion() {
        let raw = json!({
            "output": {
                "message": {
                    "role": "assistant",
                    "content": [
                        { "text": "Let me search." },
                        { "toolUse": {
                            "toolUseId": "tu_abc",
                            "name": "search_symbols",
                            "input": { "query": "STM32H743" }
                        }}
                    ]
                }
            },
            "stopReason": "tool_use"
        });
        let completion = parse_completion(&raw).unwrap();
        assert_eq!(completion.text, "Let me search.");
        assert_eq!(completion.stop_reason, "tool_use");
        assert_eq!(completion.tool_calls.len(), 1);
        let call = &completion.tool_calls[0];
        assert_eq!(call.id, "tu_abc");
        assert_eq!(call.name, "search_symbols");
        assert_eq!(call.input["query"], "STM32H743");
    }

    #[test]
    fn parse_errors_on_malformed_response() {
        let raw = json!({ "nonsense": true });
        assert!(parse_completion(&raw).is_err());
    }

    #[test]
    fn tool_result_with_image_maps_to_converse_image_block() {
        let c = client();
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
        let body = c.build_request("sys", &messages, &[]);

        let content = &body["messages"][0]["content"][0]["toolResult"]["content"];
        assert_eq!(content[0]["text"], "{\"ok\":true}");
        assert_eq!(content[1]["image"]["format"], "png");
        assert_eq!(content[1]["image"]["source"]["bytes"], "aGVsbG8=");
    }

    // --- OpenAI-compatible backend ---

    fn openai_client() -> OpenAiClient {
        OpenAiClient::new(OpenAiConfig {
            api_key: "test-key".to_string(),
            base_url: "https://api.example.com/api".to_string(),
            model: "claude-opus-4-8".to_string(),
        })
        .unwrap()
    }

    #[test]
    fn openai_chat_url_appends_path_and_trims_slash() {
        let cfg = OpenAiConfig {
            api_key: "k".to_string(),
            base_url: "https://api.example.com/api/".to_string(),
            model: "m".to_string(),
        };
        assert_eq!(cfg.chat_url(), "https://api.example.com/api/chat/completions");
    }

    #[test]
    fn openai_request_has_model_system_and_messages() {
        let c = openai_client();
        let body = c.build_request("be terse", &[Message::user("6 times 7")], &[]);
        assert_eq!(body["model"], "claude-opus-4-8");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "be terse");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"][0]["text"], "6 times 7");
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn openai_request_includes_tools_as_functions() {
        let c = openai_client();
        let tool = ToolDef {
            name: "get_design".to_string(),
            description: "fetch the current design".to_string(),
            input_schema: json!({ "type": "object", "properties": {} }),
        };
        let body = c.build_request("sys", &[Message::user("hi")], std::slice::from_ref(&tool));
        let func = &body["tools"][0]["function"];
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(func["name"], "get_design");
        assert_eq!(func["parameters"]["type"], "object");
        assert_eq!(body["tool_choice"], "auto");
    }

    #[test]
    fn openai_assistant_tool_use_becomes_tool_calls_with_string_args() {
        let c = openai_client();
        let messages = vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "tu_1".to_string(),
                name: "apply_design".to_string(),
                input: json!({ "yaml": "..." }),
            }],
        }];
        let body = c.build_request("sys", &messages, &[]);
        let assistant = &body["messages"][1];
        assert_eq!(assistant["role"], "assistant");
        assert!(assistant["content"].is_null());
        let call = &assistant["tool_calls"][0];
        assert_eq!(call["id"], "tu_1");
        assert_eq!(call["type"], "function");
        assert_eq!(call["function"]["name"], "apply_design");
        // Arguments must be a JSON *string*, not an object.
        assert_eq!(call["function"]["arguments"], "{\"yaml\":\"...\"}");
    }

    #[test]
    fn openai_tool_result_becomes_tool_message_and_images_ride_user_turn() {
        let c = openai_client();
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
        let body = c.build_request("sys", &messages, &[]);
        // [0]=system, [1]=tool message, [2]=user image message.
        let tool_msg = &body["messages"][1];
        assert_eq!(tool_msg["role"], "tool");
        assert_eq!(tool_msg["tool_call_id"], "tu_9");
        assert_eq!(tool_msg["content"], "{\"ok\":true}");
        let img_msg = &body["messages"][2];
        assert_eq!(img_msg["role"], "user");
        assert_eq!(img_msg["content"][0]["type"], "image_url");
        assert_eq!(img_msg["content"][0]["image_url"]["url"], "data:image/png;base64,aGVsbG8=");
    }

    #[test]
    fn openai_parses_text_completion() {
        let raw = json!({
            "choices": [{ "message": { "role": "assistant", "content": "42" }, "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 1 }
        });
        let completion = parse_openai_completion(&raw).unwrap();
        assert_eq!(completion.text, "42");
        assert!(completion.tool_calls.is_empty());
        assert_eq!(completion.stop_reason, "stop");
        assert_eq!(completion.input_tokens, 10);
        assert_eq!(completion.output_tokens, 1);
    }

    #[test]
    fn openai_parses_tool_call_completion() {
        let raw = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_abc",
                        "type": "function",
                        "function": { "name": "search_symbols", "arguments": "{\"query\":\"STM32H743\"}" }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        let completion = parse_openai_completion(&raw).unwrap();
        assert_eq!(completion.stop_reason, "tool_calls");
        assert_eq!(completion.tool_calls.len(), 1);
        let call = &completion.tool_calls[0];
        assert_eq!(call.id, "call_abc");
        assert_eq!(call.name, "search_symbols");
        assert_eq!(call.input["query"], "STM32H743");
    }

    #[test]
    fn openai_parses_array_content() {
        let raw = json!({
            "choices": [{ "message": { "content": [{ "type": "text", "text": "hello " }, { "type": "text", "text": "world" }] } }]
        });
        let completion = parse_openai_completion(&raw).unwrap();
        assert_eq!(completion.text, "hello world");
    }

    #[test]
    fn openai_parse_errors_on_malformed_response() {
        assert!(parse_openai_completion(&json!({ "nonsense": true })).is_err());
    }
}
