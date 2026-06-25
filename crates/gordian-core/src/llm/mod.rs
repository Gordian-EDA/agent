//! Provider-agnostic LLM client.
//!
//! This module owns the vendor-neutral conversation types ([`Role`],
//! [`ImageData`], [`ContentBlock`], [`Message`], [`ToolDef`], [`ToolCall`],
//! [`Completion`]) and the [`Provider`] trait every backend implements, with no
//! domain/UI/KiCAD coupling.
//!
//! A [`Provider`] runs one completion in either shape — await the whole
//! [`Completion`] ([`Provider::complete`]) or consume it incrementally as
//! [`StreamEvent`]s ([`Provider::stream`]).
//!
//! The one real backend, [`GenaiProvider`], is provider-agnostic over the
//! `genai` crate: bring any key + an `AGENT_MODEL`, and genai routes by the
//! model name. [`from_env`] only special-cases a private OpenAI-compatible base
//! URL (`OPENAI_BASE_URL`) — the one thing genai can't infer.

mod backend;
mod provider;
mod types;

pub use backend::{GenaiProvider, from_env, provider_status};
pub use provider::{EventStream, Provider, StreamEvent};
pub use types::{Completion, ContentBlock, ImageData, Message, Role, ToolCall, ToolDef};
