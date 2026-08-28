//! Gordian's KiCAD schematic + PCB design agent.
//!
//! This crate IS the agent: the [`Agent`] turn loop with a human apply-gate, the
//! conversation history (unwind / clear / [`Agent::compact`]), the schematic/PCB
//! tool registry ([`tools`] + [`tools_pcb`]), the composed-sheet emit
//! ([`multisheet`]), the SVG→PNG [`render`], the netlist + vision [`review`]
//! mechanics, and the [`prompts`] system prompt.
//!
//! The loop is built around ONE external seam — the [`Provider`] trait, whose one
//! production impl is [`GenaiProvider`], the genai-backed LLM (see the [`gordian_llm`]
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
pub mod multisheet;
pub mod prompts;
pub mod review;
pub mod review_kicad;
pub mod session;
pub mod testing;
pub mod tools;

pub use agent::{Agent, AgentEvent, Approvals, AutoApprove, ContextStats, StopReason, TurnOutcome};
pub use gordian_runtime::AgentRuntime;
pub use gordian_runtime::config::{
    AgentConfig, CONFIG_SCHEMA_VERSION, ConfigError, DEFAULT_MAX_TOKENS, DEFAULT_RENDER_MAX_PX,
    DEFAULT_SCHEMATIC_FILENAME, DEFAULT_SEARCH_LIMIT, EngineConfig, GordianConfig, KicadConfig,
    LlmConfig, LlmReasoningEffort, PcbRouterEngine, ProjectConfig, ReviewConfig,
    SchematicPlacementEngine, ToolConfig,
};
pub use gordian_runtime::tool::{ApplyInfo, ReviewOutcome, RunMode, ToolEffect, ToolOutcome};
pub use review::{review, review_image};

// Re-export the LLM (the production `GenaiProvider`, the `Provider` seam, and
// the genai conversation types they speak) so callers can build on
// `gordian_core::*` alone.
pub use gordian_llm::{
    Binary, ChatMessage, ChatRole, ChatStreamEvent, ContentPart, EventStream, GenaiProvider,
    MessageContent, Provider, StreamChunk, StreamEnd, Tool, ToolCall, ToolResponse, Usage,
    completed_text, drain_stream, token_usage,
};
