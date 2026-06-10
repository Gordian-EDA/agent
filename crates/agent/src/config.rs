//! Configuration loading for the LLM client.
//!
//! Credentials are read from the process environment, with a local `.env`
//! file (if present in the current working directory or an ancestor) layered
//! in first via [`dotenvy`]. The `.env` file is gitignored and holds the AWS
//! Bedrock bearer token.

use anyhow::{Context, Result};

/// Default model used when `AGENT_MODEL` is unset.
pub const DEFAULT_MODEL: &str = "us.anthropic.claude-opus-4-5-20251101-v1:0";
/// Default region used when `AWS_REGION` is unset.
pub const DEFAULT_REGION: &str = "us-east-1";

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
