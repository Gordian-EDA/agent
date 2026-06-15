//! Configuration loading for the LLM client.
//!
//! Credentials are read from the process environment, with a local `.env`
//! file (if present in the current working directory or an ancestor) layered
//! in first via [`dotenvy`]. The `.env` file is gitignored and holds the
//! provider credentials (AWS Bedrock bearer token, or OpenAI-compatible API
//! key + base URL).
//!
//! Two backends are supported behind the [`crate::llm::LlmClient`] trait:
//! AWS Bedrock ([`Config`]) and any OpenAI-compatible chat endpoint
//! ([`OpenAiConfig`]). [`selected_provider`] decides which to build.

use anyhow::{Context, Result};

/// Default model used when `AGENT_MODEL` is unset.
pub const DEFAULT_MODEL: &str = "us.anthropic.claude-opus-4-5-20251101-v1:0";
/// Default region used when `AWS_REGION` is unset.
pub const DEFAULT_REGION: &str = "us-east-1";
/// Default model for the OpenAI-compatible backend when `AGENT_MODEL` is unset.
/// The gateway namespaces models as `provider/model`, so Opus 4.8 is requested
/// as `anthropic/claude-opus-4-8`.
pub const DEFAULT_OPENAI_MODEL: &str = "anthropic/claude-opus-4-8";

/// Which LLM backend to talk to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    Bedrock,
    OpenAi,
}

impl Provider {
    /// Lowercase label for status display.
    pub fn label(self) -> &'static str {
        match self {
            Provider::Bedrock => "bedrock",
            Provider::OpenAi => "openai",
        }
    }
}

/// Decide the backend: an explicit `AGENT_PROVIDER` (`openai` | `bedrock`) wins,
/// otherwise OpenAI when `OPENAI_API_KEY` is present, else Bedrock. (`.env` is
/// layered in first, best-effort.)
pub fn selected_provider() -> Provider {
    let _ = dotenvy::dotenv();
    match std::env::var("AGENT_PROVIDER").ok().as_deref() {
        Some(p) if p.eq_ignore_ascii_case("openai") => Provider::OpenAi,
        Some(p) if p.eq_ignore_ascii_case("bedrock") => Provider::Bedrock,
        _ if std::env::var("OPENAI_API_KEY").is_ok() => Provider::OpenAi,
        _ => Provider::Bedrock,
    }
}

/// Provider + model label for status display, without requiring the selected
/// provider's credentials to be valid (falls back to the default model).
pub fn provider_status() -> (String, String) {
    let provider = selected_provider();
    let model = match provider {
        Provider::Bedrock => Config::from_env().map(|c| c.model).unwrap_or_else(|_| DEFAULT_MODEL.to_string()),
        Provider::OpenAi => {
            OpenAiConfig::from_env().map(|c| c.model).unwrap_or_else(|_| DEFAULT_OPENAI_MODEL.to_string())
        }
    };
    (provider.label().to_string(), model)
}

/// Resolved configuration for an OpenAI-compatible chat backend.
#[derive(Clone)]
pub struct OpenAiConfig {
    /// API key (`OPENAI_API_KEY`). Sent as a bearer token; never logged.
    pub api_key: String,
    /// Base URL (`OPENAI_BASE_URL`), e.g. `https://api.example.com/api`. The
    /// `/chat/completions` path is appended by [`OpenAiConfig::chat_url`].
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

    /// The chat-completions endpoint for this base URL (trailing slash trimmed).
    pub fn chat_url(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }
}

/// Resolved configuration for talking to AWS Bedrock.
#[derive(Clone)]
pub struct Config {
    /// AWS Bedrock bearer token (`AWS_BEARER_TOKEN_BEDROCK`). Never logged.
    pub bearer_token: String,
    /// AWS region (`AWS_REGION`, default `us-east-1`).
    pub region: String,
    /// Model id (`AGENT_MODEL`, default Claude Opus).
    pub model: String,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Deliberately redact the bearer token so it never lands in logs.
        f.debug_struct("Config")
            .field("bearer_token", &"<redacted>")
            .field("region", &self.region)
            .field("model", &self.model)
            .finish()
    }
}

impl Config {
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

        Ok(Self {
            bearer_token,
            region,
            model,
        })
    }

    /// The Bedrock Converse endpoint URL for this config's region and model.
    /// The model id is percent-encoded for the URL path segment.
    pub fn converse_url(&self) -> String {
        format!(
            "https://bedrock-runtime.{}.amazonaws.com/model/{}/converse",
            self.region,
            encode_path_segment(&self.model),
        )
    }
}

/// Minimal percent-encoding for a single URL path segment. Bedrock model ids
/// contain `:` and `.`; only the colon needs escaping for a clean path.
fn encode_path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_encodes_model_id() {
        let cfg = Config {
            bearer_token: "secret".to_string(),
            region: "us-east-1".to_string(),
            model: "us.anthropic.claude-opus-4-5-20251101-v1:0".to_string(),
        };
        let url = cfg.converse_url();
        assert!(url.starts_with("https://bedrock-runtime.us-east-1.amazonaws.com/model/"));
        assert!(url.ends_with("/converse"));
        // The colon in the model id must be percent-encoded.
        assert!(url.contains("v1%3A0"), "url was: {url}");
    }

    #[test]
    fn debug_redacts_bearer_token() {
        let cfg = Config {
            bearer_token: "super-secret-token".to_string(),
            region: "us-east-1".to_string(),
            model: DEFAULT_MODEL.to_string(),
        };
        let rendered = format!("{cfg:?}");
        assert!(!rendered.contains("super-secret-token"));
        assert!(rendered.contains("<redacted>"));
    }
}
