//! The `agent` crate: a provider-agnostic LLM client for auto-pcb.
//!
//! The current backend is AWS Bedrock's Converse API (see [`llm::BedrockClient`]),
//! authenticated with a bearer token. Everything is kept behind the
//! [`llm::LlmClient`] trait so the provider can be swapped later.

pub mod config;
pub mod llm;

pub use config::Config;
pub use llm::{
    BedrockClient, Completion, ContentBlock, LlmClient, Message, Role, ToolCall, ToolDef,
};
