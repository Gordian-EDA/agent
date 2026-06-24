//! An OpenAI-compatible **chat completions** backend, using an API key as a
//! bearer token. Speaks the standard `/chat/completions` shape, so it works
//! against OpenAI itself or a proxy (the configured gateway proxies Anthropic
//! models). The provider-neutral [`Message`]/[`ContentBlock`] types map onto
//! OpenAI roles: a [`ContentBlock::ToolResult`] becomes a `tool` message (with
//! any attached images riding on a trailing `user` message, since OpenAI `tool`
//! messages are text-only).

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::config::OpenAiConfig;
use crate::provider::Provider;
use crate::types::{Completion, ContentBlock, Message, Role, ToolCall, ToolDef};

/// A fresh random id for a new thread (one conversation/session). Random — not
/// derived from time or content — so distinct threads never collide. Adequate
/// for gateway thread-grouping, not for security.
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

pub struct OpenAiClient {
    config: OpenAiConfig,
    http: reqwest::Client,
    max_tokens: u32,
    /// Sampling temperature, sent only when set. Omitted by default: newer
    /// Anthropic models (proxied through the gateway) reject `temperature` as
    /// deprecated, so the safe default is to not send it at all.
    temperature: Option<f32>,
    /// Randomly generated once per client, i.e. once per conversation/thread (the `Agent`
    /// owns one client for its whole conversation). Sent as `thread_identifier` in the
    /// request body so the respan gateway groups this thread's completions together;
    /// random (not derived from time/content) so two distinct threads never collide.
    thread_id: String,
}

impl OpenAiClient {
    /// Build from an explicit [`OpenAiConfig`].
    pub fn new(config: OpenAiConfig) -> Result<Self> {
        let http = reqwest::Client::builder().build().context("building reqwest client")?;
        // 16k, not 4k: a single create_design carries the whole circuit YAML, which for a large
        // board exceeds 4k tokens and gets TRUNCATED — the truncated args then fail to parse and the
        // yaml silently vanishes ("missing required string field yaml"). Gateway models allow ≥16k.
        Ok(Self { config, http, max_tokens: 16384, temperature: None, thread_id: random_thread_id() })
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
    /// `OpenAiClient::temperature`.
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
        // `extra_body` (OpenAI SDK) merges into the request body top-level; the respan gateway
        // groups this client's completions into one thread by `thread_identifier`.
        body["thread_identifier"] = json!(self.thread_id);
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
impl Provider for OpenAiClient {
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

    fn new_thread(&mut self) {
        self.thread_id = random_thread_id();
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
                // `content` must be present AND a STRING: the respan.ai gateway rejects `null` even
                // alongside tool_calls ("Invalid value for 'content': expected a string, got null"),
                // and GPT-5.5 emits tool-call-only turns (empty text). Serialize "" rather than null.
                msg["content"] = json!(text);
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
            // Tool-call arguments arrive as a JSON STRING in the OpenAI spec, but Anthropic models
            // proxied through the respan gateway return them as an already-structured OBJECT.
            // Accept both, else an object-form `create_design` loses its `yaml` ("missing field").
            let input = match func.and_then(|f| f.get("arguments")) {
                Some(Value::String(s)) => serde_json::from_str(s).unwrap_or_else(|e| {
                    // A parse failure here almost always means the arguments string was TRUNCATED
                    // (output token cap). Make it visible instead of silently dropping the fields.
                    eprintln!("warning: tool-call arguments did not parse (likely truncated): {e}");
                    Value::Null
                }),
                Some(other) => other.clone(),
                None => Value::Null,
            };
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ImageData;

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
        // A tool-call-only turn serializes content as "" (not null) — the respan.ai gateway
        // rejects null content even alongside tool_calls.
        assert_eq!(assistant["content"], json!(""));
        // thread_identifier is sent for gateway thread-grouping.
        assert!(body["thread_identifier"].is_string());
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
    fn openai_parses_object_form_tool_arguments() {
        // Anthropic models via the gateway return `arguments` as a structured OBJECT, not a JSON
        // string; the input must still be recovered (else create_design loses its `yaml`).
        let raw = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_xyz",
                        "type": "function",
                        "function": { "name": "create_design", "arguments": { "yaml": "version: 1" } }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        let completion = parse_openai_completion(&raw).unwrap();
        let call = &completion.tool_calls[0];
        assert_eq!(call.name, "create_design");
        assert_eq!(call.input["yaml"], "version: 1");
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
