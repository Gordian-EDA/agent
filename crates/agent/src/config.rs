//! Transitional re-export of the LLM-backend configuration, now living in the
//! `llm-client` crate. The backend selector enum keeps its old in-crate name
//! `Provider` (it is `llm_client::Backend`) so callers compile unchanged during
//! the split.

pub use llm_client::config::{
    Backend as Provider, Config, DEFAULT_MODEL, DEFAULT_OPENAI_MODEL, DEFAULT_REGION, OpenAiConfig,
    provider_status, selected_backend as selected_provider,
};
