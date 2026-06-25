//! Gordian's KiCAD schematic + PCB design agent.
//!
//! This crate IS the agent: the [`Agent`] turn loop with a human apply-gate, the
//! conversation history (unwind / clear / [`Agent::compact`]), the schematic/PCB
//! tool registry ([`tools`] + [`tools_pcb`]), the composed-sheet emit
//! ([`multisheet`]), the SVG→PNG [`render`], the netlist + vision [`review`]
//! mechanics, the [`prompts`] system prompt, and the design [`retrieval`] corpus.
//!
//! The loop is built around ONE external seam — [`Provider`], the LLM backend
//! (see the [`llm`] module) — plus the KiCAD tools it drives directly. The only
//! decoupling that remains is this library vs. the
//! [`gordian`](../gordian/index.html) CLI binary, so a future web frontend
//! reuses the lib. To build a working agent:
//!
//! ```ignore
//! let ctx = gordian_core::tools::PcbToolCtx::for_project(env, project_dir)?;
//! let agent = gordian_core::Agent::new(
//!     gordian_core::from_env()?,
//!     ctx,
//!     gordian_core::prompts::system_prompt(),
//! );
//! ```
//!
//! The [`testing`] module's [`testing::ScriptedClient`] drives the loop without a
//! network; the gating tests inject a [`TestBackend`] instead of a real
//! [`PcbToolCtx`].

mod agent;
pub mod llm;
pub mod multisheet;
pub mod prompts;
pub mod render;
pub mod retrieval;
pub mod review;
pub mod review_kicad;
pub mod session;
pub mod testing;
mod tool;
pub mod tools;
pub mod tools_pcb;
pub mod workspace;

pub use agent::{
    Agent, AgentEvent, Approvals, AutoApprove, ContextStats, StopReason, TestBackend, TurnOutcome,
};
pub use review::{review, review_image};
pub use tool::{ApplyInfo, ReviewOutcome, RunMode, ToolEffect, ToolOutcome};

// Re-export the LLM seam so callers can build on `gordian_core::*` alone.
pub use llm::{
    Completion, ContentBlock, ImageData, Message, Provider, Role, ToolCall, ToolDef, from_env,
    provider_status,
};
