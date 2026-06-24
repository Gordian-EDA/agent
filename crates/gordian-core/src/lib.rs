//! Gordian's domain-agnostic agent core.
//!
//! This crate owns the generic half of the agent: the [`Agent`] turn loop, the
//! human apply-gate, conversation history (unwind / clear / [`Agent::compact`]),
//! and the diverse-lens [`review`] mechanics — all over two injected seams and
//! NOTHING domain-specific (no KiCAD, UI, or rendering dependencies).
//!
//! The two seams the loop is built around:
//!
//! - [`Provider`](llm_client::Provider) (from `llm-client`) — the LLM backend.
//! - [`ToolProvider`] — the domain's tools, tagged by [`ToolEffect`]
//!   (`ReadOnly` | `Authoring` | `Gated`). The loop drives a `Gated` write
//!   through preview → approve → commit ([`RunMode`]), reporting via
//!   [`ApplyInfo`]; an independent post-turn review comes back as a
//!   [`ReviewOutcome`].
//!
//! [`Agent::new`] takes a `Box<dyn Provider>`, a `Box<dyn ToolProvider>`, and the
//! domain's system prompt — so the same loop serves any domain, any backend, a
//! CLI, or a web frontend. The [`testing`] module's [`testing::ScriptedClient`]
//! drives the loop without a network.

mod agent;
pub mod review;
pub mod session;
pub mod testing;
mod tool;

pub use agent::{
    Agent, AgentEvent, Approvals, AutoApprove, ContextStats, StopReason, TurnOutcome,
    TurnOutcomeSummary,
};
pub use review::review;
pub use tool::{
    ApplyInfo, ReviewOutcome, RunMode, ToolEffect, ToolOutcome, ToolProvider,
};

// Re-export the LLM seam so domain crates can build on `gordian_core::*` alone.
pub use llm_client::{
    Completion, ContentBlock, ImageData, Message, Provider, Role, ToolCall, ToolDef,
};
