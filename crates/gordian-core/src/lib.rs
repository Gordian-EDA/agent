//! Gordian's KiCAD schematic + PCB design agent.
//!
//! This crate IS the agent: the [`Agent`] turn loop with a human apply-gate, the
//! conversation history (unwind / clear / [`Agent::compact`]), the schematic/PCB
//! tool registry ([`tools`] + [`tools_pcb`]), the composed-sheet emit
//! ([`multisheet`]), the SVG→PNG [`render`], the netlist + vision [`review`]
//! mechanics, the [`prompts`] system prompt, and the design [`retrieval`] corpus.
//!
//! The loop is built around ONE external seam — the [`Provider`] trait, whose one
//! production impl is [`GenaiProvider`], the genai-backed LLM (see the [`llm`]
//! module) — plus the KiCAD tools it drives directly. The only decoupling that
//! remains is this library vs. the [`gordian`](../gordian/index.html) CLI binary,
//! so a future web frontend reuses the lib. To build a working agent:
//!
//! ```ignore
//! let config = gordian_core::GordianConfig::default();
//! let ctx = gordian_core::AgentRuntime::for_project_with_config(
//!     env,
//!     project_dir,
//!     config.clone(),
//! )?;
//! let agent = gordian_core::Agent::new(
//!     gordian_core::GenaiProvider::from_config(&config.llm)?,
//!     ctx,
//!     gordian_core::prompts::system_prompt(),
//! );
//! ```
//!
//! The [`testing`] module's [`testing::ScriptedClient`] drives provider behavior
//! without a network; tools always run against a real [`AgentRuntime`].

mod agent;
pub mod config;
pub mod llm;
pub mod multisheet;
pub mod prompts;
pub mod render;
pub mod retrieval;
pub mod review;
pub mod review_kicad;
mod runtime;
pub mod session;
pub mod testing;
mod tool;
pub mod tools;
pub mod tools_pcb;
pub mod workspace;

pub use agent::{Agent, AgentEvent, Approvals, AutoApprove, ContextStats, StopReason, TurnOutcome};
pub use config::{
    AgentConfig, CONFIG_SCHEMA_VERSION, ConfigError, DEFAULT_MAX_TOKENS, DEFAULT_REFERENCE_COUNT,
    DEFAULT_RENDER_MAX_PX, DEFAULT_SCHEMATIC_FILENAME, DEFAULT_SEARCH_LIMIT, GordianConfig,
    KicadConfig, LlmConfig, ProjectConfig, RetrievalConfig, ReviewConfig, ToolConfig,
};
pub use review::{review, review_image};
pub use runtime::AgentRuntime;
pub use tool::{ApplyInfo, ReviewOutcome, RunMode, ToolEffect, ToolOutcome};

// Re-export the LLM (the production `GenaiProvider`, the `Provider` seam, and
// the genai conversation types they speak) so callers can build on
// `gordian_core::*` alone.
pub use llm::{
    Binary, ChatMessage, ChatRole, ChatStreamEvent, ContentPart, EventStream, GenaiProvider,
    MessageContent, Provider, StreamChunk, StreamEnd, Tool, ToolCall, ToolResponse, Usage,
    completed_text, drain_stream, token_usage,
};
