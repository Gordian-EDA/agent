//! The single genai-backed [`crate::Provider`] and the env-driven selector.

mod genai;

pub use genai::GenaiProvider;

use anyhow::Result;

use crate::config::{
    AnthropicConfig, Backend, BedrockConfig, OpenAiConfig, selected_backend,
};
use crate::provider::Provider;

/// Build the configured [`GenaiProvider`] from the environment / local `.env`,
/// selecting the backend via [`crate::config::selected_backend`] (OpenAI-compatible
/// when `OPENAI_API_KEY` is set, else native Anthropic when `ANTHROPIC_API_KEY` is
/// set, else AWS Bedrock). Returned boxed behind the [`Provider`] trait so callers
/// stay provider-agnostic.
pub fn from_env() -> Result<Box<dyn Provider>> {
    match selected_backend() {
        Backend::OpenAi => Ok(Box::new(GenaiProvider::gateway(OpenAiConfig::from_env()?))),
        Backend::Anthropic => Ok(Box::new(GenaiProvider::anthropic(AnthropicConfig::from_env()?))),
        Backend::Bedrock => Ok(Box::new(GenaiProvider::bedrock(BedrockConfig::from_env()?))),
    }
}
