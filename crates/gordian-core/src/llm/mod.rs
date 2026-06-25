//! LLM client, spoken directly in genai's conversation types.
//!
//! There are no neutral wrapper types: messages are genai [`ChatMessage`]s, tools
//! are genai [`Tool`]s, a tool call is a genai [`ToolCall`], an image is a genai
//! [`Binary`]. The one production LLM is [`GenaiProvider`], provider-agnostic over
//! the `genai` crate: bring any key + an `AGENT_MODEL`, and genai routes by the
//! model name — even a private OpenAI-compatible endpoint via a `genai_N::` model
//! namespace. [`GenaiProvider::from_env`] is just `Client::default()` + the model
//! id.
//!
//! The agent loop talks to its LLM through the small [`Provider`] seam (a backend
//! runs one completion as a whole [`StreamEnd`] or as a genai [`ChatStreamEvent`]
//! stream); [`GenaiProvider`] is its one production impl, and the `testing`
//! doubles are the others, so the loop's single external dependency stays
//! swappable for the no-network tests.

mod provider;
mod seam;

pub use provider::{GenaiProvider, completed_text, token_usage};
pub use seam::{EventStream, Provider, drain_stream};

// The genai types the rest of the crate speaks. Re-exported so callers build on
// `gordian_core::*` without taking a direct genai dependency.
pub use genai::chat::{
    Binary, ChatMessage, ChatRole, ChatStreamEvent, ContentPart, MessageContent, StreamChunk,
    StreamEnd, Tool, ToolCall, ToolResponse, Usage,
};
