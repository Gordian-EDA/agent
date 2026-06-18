//! The `agent` crate: a provider-agnostic LLM client for auto-pcb.
//!
//! Two backends sit behind the [`llm::LlmClient`] trait: AWS Bedrock's Converse
//! API ([`llm::BedrockClient`]) and any OpenAI-compatible chat endpoint
//! ([`llm::OpenAiClient`]). [`llm::from_env`] picks one via
//! [`config::selected_provider`] (OpenAI when `OPENAI_API_KEY` is set, else
//! Bedrock), so callers stay provider-agnostic.

pub mod agent;
pub mod config;
pub mod llm;
pub mod render;
pub mod tools;
pub mod tools_pcb;
pub mod workspace;

pub use agent::{
    Agent, AgentEvent, Approvals, AutoApprove, StopReason, TurnOutcome, TurnOutcomeSummary,
};
pub use config::{Config, OpenAiConfig, Provider};
pub use llm::{
    BedrockClient, Completion, ContentBlock, LlmClient, Message, OpenAiClient, Role, ToolCall,
    ToolDef,
};
