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
//! [`ToolCallAssembler`] folds streaming tool-call deltas (OpenAI index-keyed
//! arg strings, Anthropic `input_json_delta`, or whole-object gateway args) into
//! finished [`ToolCall`]s — the seam a real SSE backend plugs into.
//!
//! Two backends ship today: AWS Bedrock's Converse API ([`BedrockClient`]) and
//! any OpenAI-compatible chat endpoint ([`OpenAiClient`]). [`from_env`] picks one
//! via [`config::selected_backend`] (OpenAI when `OPENAI_API_KEY` is set, else
//! Bedrock), so callers stay provider-agnostic.

mod assemble;
mod backend;
pub mod config;
mod provider;
mod types;

pub use assemble::ToolCallAssembler;
pub use backend::{BedrockClient, OpenAiClient, from_env};
pub use config::{Backend, Config, OpenAiConfig};
pub use provider::{EventStream, Provider, StreamEvent, drain_stream};
pub use types::{Completion, ContentBlock, ImageData, Message, Role, ToolCall, ToolDef};
