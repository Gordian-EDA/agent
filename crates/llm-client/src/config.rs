//! Configuration loading for the LLM backends.
//!
//! Credentials are read from the process environment, with a local `.env`
//! file (if present in the current working directory or an ancestor) layered
//! in first via [`dotenvy`]. The `.env` file is gitignored and holds the
//! provider credentials (AWS Bedrock bearer token, OpenAI-compatible API key +
//! base URL, or an Anthropic API key).
//!
//! Three backends sit behind the [`crate::Provider`] trait, all served by the
//! single [`crate::GenaiProvider`] over the `genai` crate: AWS Bedrock
//! ([`BedrockConfig`]), any OpenAI-compatible chat endpoint ([`OpenAiConfig`]),
//! and the native Anthropic Messages API ([`AnthropicConfig`]).
//! [`selected_backend`] decides which to build.

use anyhow::{Context, Result};

/// Default model used by the Bedrock backend when `AGENT_MODEL` is unset.
pub const DEFAULT_MODEL: &str = "us.anthropic.claude-opus-4-5-20251101-v1:0";
/// Default region used when `AWS_REGION` is unset.
pub const DEFAULT_REGION: &str = "us-east-1";
/// Default model for the OpenAI-compatible backend when `AGENT_MODEL` is unset —
/// the model the e2e tests (`bluepill_agent`, `pcb_gate`'s real-model run) and the
/// `agent_design`/`board_agent` examples drive via `from_env`. The gateway
/// namespaces models as `provider/model`, so Sonnet 4.6 is requested as
/// `anthropic/claude-sonnet-4-6`. (Override per-run with `AGENT_MODEL`.)
pub const DEFAULT_OPENAI_MODEL: &str = "anthropic/claude-sonnet-4-6";
/// Default model for the native Anthropic backend when `AGENT_MODEL` is unset.
pub const DEFAULT_ANTHROPIC_MODEL: &str = "claude-opus-4-5";

/// Which LLM backend to talk to. All three are served by [`crate::GenaiProvider`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Bedrock,
    OpenAi,
    Anthropic,
}

impl Backend {
    /// Lowercase label for status display.
    pub fn label(self) -> &'static str {
        match self {
            Backend::Bedrock => "bedrock",
            Backend::OpenAi => "openai",
            Backend::Anthropic => "anthropic",
        }
    }
}

/// Decide the backend: an explicit `AGENT_PROVIDER` (`openai` | `bedrock` |
/// `anthropic`) wins, otherwise OpenAI when `OPENAI_API_KEY` is present, then
/// Anthropic when `ANTHROPIC_API_KEY` is present, else Bedrock. (`.env` is
/// layered in first, best-effort.)
pub fn selected_backend() -> Backend {
    let _ = dotenvy::dotenv();
    match std::env::var("AGENT_PROVIDER").ok().as_deref() {
        Some(p) if p.eq_ignore_ascii_case("openai") => Backend::OpenAi,
        Some(p) if p.eq_ignore_ascii_case("bedrock") => Backend::Bedrock,
        Some(p) if p.eq_ignore_ascii_case("anthropic") => Backend::Anthropic,
        _ if std::env::var("OPENAI_API_KEY").is_ok() => Backend::OpenAi,
        _ if std::env::var("ANTHROPIC_API_KEY").is_ok() => Backend::Anthropic,
        _ => Backend::Bedrock,
    }
}

/// Backend + model label for status display, without requiring the selected
/// backend's credentials to be valid (falls back to the default model).
pub fn provider_status() -> (String, String) {
    let backend = selected_backend();
    let model = match backend {
        Backend::Bedrock => {
            BedrockConfig::from_env().map(|c| c.model).unwrap_or_else(|_| DEFAULT_MODEL.to_string())
        }
        Backend::OpenAi => OpenAiConfig::from_env()
            .map(|c| c.model)
            .unwrap_or_else(|_| DEFAULT_OPENAI_MODEL.to_string()),
        Backend::Anthropic => AnthropicConfig::from_env()
            .map(|c| c.model)
            .unwrap_or_else(|_| DEFAULT_ANTHROPIC_MODEL.to_string()),
    };
    (backend.label().to_string(), model)
}

/// Resolved configuration for an OpenAI-compatible chat backend.
#[derive(Clone)]
pub struct OpenAiConfig {
    /// API key (`OPENAI_API_KEY`). Sent as a bearer token; never logged.
    pub api_key: String,
    /// Base URL (`OPENAI_BASE_URL`), e.g. `https://api.example.com/api`. The
    /// `/chat/completions` path is appended when the request is sent.
    pub base_url: String,
    /// Model id (`AGENT_MODEL`, default [`DEFAULT_OPENAI_MODEL`]).
    pub model: String,
}

impl std::fmt::Debug for OpenAiConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiConfig")
            .field("api_key", &"<redacted>")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .finish()
    }
}

impl OpenAiConfig {
    /// Load from the environment / local `.env`. Errors if the API key or base
    /// URL is missing; the model falls back to [`DEFAULT_OPENAI_MODEL`].
    pub fn from_env() -> Result<Self> {
        let _ = dotenvy::dotenv();
        let api_key = std::env::var("OPENAI_API_KEY")
            .context("OPENAI_API_KEY not set (expected in environment or a local .env file)")?;
        let base_url = std::env::var("OPENAI_BASE_URL")
            .context("OPENAI_BASE_URL not set (expected in environment or a local .env file)")?;
        let model = std::env::var("AGENT_MODEL").unwrap_or_else(|_| DEFAULT_OPENAI_MODEL.to_string());
        Ok(Self { api_key, base_url, model })
    }
}

/// Resolved configuration for the native Anthropic Messages API backend.
#[derive(Clone)]
pub struct AnthropicConfig {
    /// Anthropic API key (`ANTHROPIC_API_KEY`). Never logged.
    pub api_key: String,
    /// Model id (`AGENT_MODEL`, default [`DEFAULT_ANTHROPIC_MODEL`]).
    pub model: String,
}

impl std::fmt::Debug for AnthropicConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicConfig")
            .field("api_key", &"<redacted>")
            .field("model", &self.model)
            .finish()
    }
}

impl AnthropicConfig {
    /// Load from the environment / local `.env`. Errors if the API key is
    /// missing; the model falls back to [`DEFAULT_ANTHROPIC_MODEL`].
    pub fn from_env() -> Result<Self> {
        let _ = dotenvy::dotenv();
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .context("ANTHROPIC_API_KEY not set (expected in environment or a local .env file)")?;
        let model =
            std::env::var("AGENT_MODEL").unwrap_or_else(|_| DEFAULT_ANTHROPIC_MODEL.to_string());
        Ok(Self { api_key, model })
    }
}

/// Resolved configuration for the AWS Bedrock Converse backend, authenticated
/// with a bearer token (not SigV4 — genai's `BedrockApiAdapter` pulls no AWS SDK).
#[derive(Clone)]
pub struct BedrockConfig {
    /// AWS Bedrock bearer token (`AWS_BEARER_TOKEN_BEDROCK`). Never logged.
    pub bearer_token: String,
    /// AWS region (`AWS_REGION`, default `us-east-1`).
    pub region: String,
    /// Model id (`AGENT_MODEL`, default Claude Opus).
    pub model: String,
}

impl std::fmt::Debug for BedrockConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Deliberately redact the bearer token so it never lands in logs.
        f.debug_struct("BedrockConfig")
            .field("bearer_token", &"<redacted>")
            .field("region", &self.region)
            .field("model", &self.model)
            .finish()
    }
}

impl BedrockConfig {
    /// Load configuration from the environment, layering in a local `.env`
    /// file first if one is present. Errors only if the bearer token is
    /// missing — region and model fall back to defaults.
    pub fn from_env() -> Result<Self> {
        // Best-effort: a missing .env is fine (CI / prod use real env vars).
        let _ = dotenvy::dotenv();

        let bearer_token = std::env::var("AWS_BEARER_TOKEN_BEDROCK").context(
            "AWS_BEARER_TOKEN_BEDROCK not set (expected in environment or a local .env file)",
        )?;
        let region = std::env::var("AWS_REGION").unwrap_or_else(|_| DEFAULT_REGION.to_string());
        let model = std::env::var("AGENT_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());

        Ok(Self { bearer_token, region, model })
    }

    /// The Bedrock Converse runtime endpoint for this config's region. genai's
    /// `BedrockApiAdapter` appends the per-model `/model/{id}/converse` path.
    pub fn endpoint(&self) -> String {
        format!("https://bedrock-runtime.{}.amazonaws.com/", self.region)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bedrock_endpoint_is_region_scoped() {
        let cfg = BedrockConfig {
            bearer_token: "secret".to_string(),
            region: "us-west-2".to_string(),
            model: DEFAULT_MODEL.to_string(),
        };
        assert_eq!(cfg.endpoint(), "https://bedrock-runtime.us-west-2.amazonaws.com/");
    }

    #[test]
    fn debug_redacts_bearer_token() {
        let cfg = BedrockConfig {
            bearer_token: "super-secret-token".to_string(),
            region: "us-east-1".to_string(),
            model: DEFAULT_MODEL.to_string(),
        };
        let rendered = format!("{cfg:?}");
        assert!(!rendered.contains("super-secret-token"));
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn debug_redacts_openai_and_anthropic_keys() {
        let oai = OpenAiConfig {
            api_key: "sk-openai-secret".to_string(),
            base_url: "https://api.example.com/api".to_string(),
            model: DEFAULT_OPENAI_MODEL.to_string(),
        };
        let ant = AnthropicConfig {
            api_key: "sk-ant-secret".to_string(),
            model: DEFAULT_ANTHROPIC_MODEL.to_string(),
        };
        assert!(!format!("{oai:?}").contains("sk-openai-secret"));
        assert!(format!("{oai:?}").contains("<redacted>"));
        assert!(!format!("{ant:?}").contains("sk-ant-secret"));
        assert!(format!("{ant:?}").contains("<redacted>"));
    }
}
