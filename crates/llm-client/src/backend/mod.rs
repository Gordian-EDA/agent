//! The concrete [`crate::Provider`] backends and the env-driven selector.

mod bedrock;
mod openai;

pub use bedrock::BedrockClient;
pub use openai::OpenAiClient;

use anyhow::Result;

use crate::config::{Backend, selected_backend};
use crate::provider::Provider;

/// Build the configured client from the environment / local `.env`, selecting
/// the backend via [`crate::config::selected_backend`] (OpenAI-compatible when
/// `OPENAI_API_KEY` is set, else AWS Bedrock). Returned boxed behind the
/// [`Provider`] trait so callers are provider-agnostic.
pub fn from_env() -> Result<Box<dyn Provider>> {
    match selected_backend() {
        Backend::OpenAi => Ok(Box::new(OpenAiClient::from_env()?)),
        Backend::Bedrock => Ok(Box::new(BedrockClient::from_env()?)),
    }
}
