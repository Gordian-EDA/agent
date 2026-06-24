//! Provider-agnostic LLM client.
//!
//! This crate is the **bottom** of the Gordian stack: it owns the vendor-neutral
//! conversation types ([`Role`], [`ImageData`], [`ContentBlock`], [`Message`],
//! [`ToolDef`], [`ToolCall`], [`Completion`]) and the [`Provider`] trait that
//! every backend implements. It has NO domain, UI, or KiCAD dependencies, so it
//! is publishable on its own and shared by both the CLI and a future web backend.
//!
//! A [`Provider`] runs one completion in either of two shapes — await the whole
//! [`Completion`] ([`Provider::complete`]) or consume it incrementally as
//! [`StreamEvent`]s ([`Provider::stream`]); the default `stream` wraps `complete`
//! into a single event so a request/response backend needs only `complete`.
//!
//! All three backends are served by one implementation, [`GenaiProvider`], over
//! the `genai` crate: the OpenAI-compatible gateway, the native Anthropic
//! Messages API, and AWS Bedrock (Converse). [`from_env`] picks one via
//! [`config::selected_backend`] (OpenAI when `OPENAI_API_KEY` is set, else
//! Anthropic when `ANTHROPIC_API_KEY` is set, else Bedrock), so callers stay
//! provider-agnostic.

mod backend;
pub mod config;
mod provider;
mod types;

pub use backend::{GenaiProvider, from_env};
pub use config::{AnthropicConfig, Backend, BedrockConfig, OpenAiConfig};
pub use provider::{EventStream, Provider, StreamEvent, drain_stream};
pub use types::{Completion, ContentBlock, ImageData, Message, Role, ToolCall, ToolDef};
