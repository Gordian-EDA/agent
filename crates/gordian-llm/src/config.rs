//! Typed request configuration for the production provider.

use std::fmt;
use std::str::FromStr;

use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize, Serializer};

/// Default completion budget when the config does not set one.
pub const DEFAULT_MAX_TOKENS: u32 = 16_384;

/// A failed [`LlmConfig::validate`]: the offending field path and the reason.
#[derive(Debug)]
pub struct LlmConfigError {
    pub field: String,
    pub message: String,
}

impl LlmConfigError {
    fn new(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            message: message.into(),
        }
    }
}

/// LLM request behavior.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct LlmConfig {
    /// Optional explicit adapter namespace, such as `openai`, `anthropic`,
    /// `gemini`, `open_router`, `bedrock_api`, or `ollama`.
    ///
    /// When absent, the production provider lets genai infer the adapter from
    /// the model name, preserving old configs.
    pub adapter: Option<String>,
    /// Provider-routed model id, such as `gpt-4o`, `claude-sonnet-4-6`, or an
    /// adapter-namespaced id like `open_router::anthropic/claude-sonnet-4-5`.
    pub model: Option<String>,
    /// API key for the configured model provider, when the provider requires
    /// one. This is a local client setting; gordian-core does not decide where
    /// or how it is persisted.
    pub api_key: Option<String>,
    /// Optional provider endpoint override, for OpenAI-compatible gateways,
    /// local model servers, and test doubles.
    pub endpoint: Option<String>,
    /// Maximum completion tokens requested per agent call.
    pub max_tokens: u32,
    /// Whether the provider should ask for an ephemeral cache breakpoint over
    /// the static system/tool prefix when supported.
    pub ephemeral_cache: bool,
    /// Optional provider-routed reasoning/thinking effort hint.
    pub reasoning_effort: Option<LlmReasoningEffort>,
    /// Whether to request provider reasoning summaries/content when supported.
    pub capture_reasoning: bool,
    /// Whether the model accepts image input. Text-only models (most open
    /// models on OpenAI-compatible gateways) reject image parts outright, so
    /// the agent keeps rendered-image tool results on disk and out of the
    /// conversation when this is false.
    pub vision_capable: bool,
}

impl fmt::Debug for LlmConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LlmConfig")
            .field("adapter", &self.adapter)
            .field("model", &self.model)
            .field("api_key", &self.api_key.as_ref().map(|_| "[REDACTED]"))
            .field("endpoint", &self.endpoint)
            .field("max_tokens", &self.max_tokens)
            .field("ephemeral_cache", &self.ephemeral_cache)
            .field("reasoning_effort", &self.reasoning_effort)
            .field("capture_reasoning", &self.capture_reasoning)
            .field("vision_capable", &self.vision_capable)
            .finish()
    }
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            adapter: None,
            model: None,
            api_key: None,
            endpoint: None,
            max_tokens: DEFAULT_MAX_TOKENS,
            ephemeral_cache: true,
            reasoning_effort: None,
            capture_reasoning: false,
            vision_capable: true,
        }
    }
}

impl LlmConfig {
    pub fn validate(&self, path: &'static str) -> Result<(), LlmConfigError> {
        if let Some(adapter) = &self.adapter {
            let adapter = adapter.trim();
            if adapter.is_empty() {
                return Err(LlmConfigError::new(
                    format!("{path}.adapter"),
                    "adapter must not be empty when set",
                ));
            }
            if adapter.contains("::") || adapter.chars().any(char::is_whitespace) {
                return Err(LlmConfigError::new(
                    format!("{path}.adapter"),
                    "adapter must be a provider namespace like openai, anthropic, or open_router",
                ));
            }
        }
        if let Some(model) = &self.model
            && model.trim().is_empty()
        {
            return Err(LlmConfigError::new(
                format!("{path}.model"),
                "model must not be empty when set",
            ));
        }
        if let Some(api_key) = &self.api_key
            && api_key.trim().is_empty()
        {
            return Err(LlmConfigError::new(
                format!("{path}.apiKey"),
                "API key must not be empty when set",
            ));
        }
        if let Some(endpoint) = &self.endpoint {
            let endpoint = endpoint.trim();
            if endpoint.is_empty() {
                return Err(LlmConfigError::new(
                    format!("{path}.endpoint"),
                    "endpoint must not be empty when set",
                ));
            }
            if !(endpoint.starts_with("http://") || endpoint.starts_with("https://")) {
                return Err(LlmConfigError::new(
                    format!("{path}.endpoint"),
                    "endpoint must start with http:// or https://",
                ));
            }
        }
        if self.max_tokens == 0 {
            return Err(LlmConfigError::new(
                format!("{path}.maxTokens"),
                "max tokens must be greater than zero",
            ));
        }
        Ok(())
    }
}

/// Provider-neutral reasoning/thinking effort hint for LLM requests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LlmReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
    Budget(u32),
}

impl fmt::Display for LlmReasoningEffort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => write!(f, "none"),
            Self::Minimal => write!(f, "minimal"),
            Self::Low => write!(f, "low"),
            Self::Medium => write!(f, "medium"),
            Self::High => write!(f, "high"),
            Self::XHigh => write!(f, "xhigh"),
            Self::Max => write!(f, "max"),
            Self::Budget(tokens) => write!(f, "{tokens}"),
        }
    }
}

impl FromStr for LlmReasoningEffort {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        if value.is_empty() {
            return Err("reasoning effort must not be empty".to_string());
        }
        match value.to_ascii_lowercase().as_str() {
            "none" => Ok(Self::None),
            "minimal" => Ok(Self::Minimal),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" | "x-high" => Ok(Self::XHigh),
            "max" => Ok(Self::Max),
            _ => value
                .parse::<u32>()
                .ok()
                .filter(|budget| *budget > 0)
                .map(Self::Budget)
                .ok_or_else(|| {
                    "reasoning effort must be none, minimal, low, medium, high, xhigh, max, or a positive token budget"
                        .to_string()
                }),
        }
    }
}

impl Serialize for LlmReasoningEffort {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for LlmReasoningEffort {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct EffortVisitor;

        impl<'de> Visitor<'de> for EffortVisitor {
            type Value = LlmReasoningEffort;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(
                    "none, minimal, low, medium, high, xhigh, max, or a positive token budget",
                )
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                LlmReasoningEffort::from_str(value).map_err(E::custom)
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                let value = u32::try_from(value).map_err(E::custom)?;
                if value == 0 {
                    return Err(E::custom(
                        "reasoning effort budget must be greater than zero",
                    ));
                }
                Ok(LlmReasoningEffort::Budget(value))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                let value = u32::try_from(value)
                    .map_err(|_| E::custom("reasoning effort budget must be greater than zero"))?;
                if value == 0 {
                    return Err(E::custom(
                        "reasoning effort budget must be greater than zero",
                    ));
                }
                Ok(LlmReasoningEffort::Budget(value))
            }
        }

        deserializer.deserialize_any(EffortVisitor)
    }
}
