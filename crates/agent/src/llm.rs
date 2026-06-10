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

use crate::config::Config;

/// Build the default configured client from the environment / local `.env`.
///
/// Convenience wrapper around [`BedrockClient::from_env`].
pub fn from_env() -> Result<BedrockClient> {
    BedrockClient::from_env()
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
}
