//! Transitional re-export of the provider-agnostic LLM client, now living in the
//! `llm-client` crate. The trait keeps its old in-crate name `LlmClient` (it is
//! `llm_client::Provider`) so the agent loop compiles unchanged during the split;
//! the domain crate will consume `llm-client` directly once `agent` is retired.

pub use llm_client::{
    BedrockClient, Completion, ContentBlock, ImageData, Message, OpenAiClient, Provider as LlmClient,
    Role, ToolCall, ToolDef, from_env,
};
