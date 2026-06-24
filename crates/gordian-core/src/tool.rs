//! The domain-agnostic tool seam: a [`ToolProvider`] is everything the agent loop
//! needs to expose and execute a domain's tools without knowing what they do.
//!
//! The loop drives tools by [`ToolEffect`]:
//!
//! - [`ToolEffect::ReadOnly`] / [`ToolEffect::Authoring`] — run once
//!   ([`RunMode::Normal`]) and feed the result back to the model. (The split is
//!   informational; the loop treats them identically today.)
//! - [`ToolEffect::Gated`] — a *write*. When the model invokes a gated tool with
//!   intent to apply ([`ToolProvider::wants_apply`]), the loop runs it twice:
//!   first [`RunMode::Preview`] (a dry-run that produces the diff to approve
//!   WITHOUT writing), then — only on human approval — [`RunMode::Commit`] (the
//!   real write). The provider reports what happened via [`ApplyInfo`].
//!
//! This reproduces the schematic apply-gate (`apply_design` dry-run → approve →
//! commit) generically: the provider owns the domain detail (how a call flips
//! between preview and commit, what "ready"/"committed" mean), the loop owns the
//! preview → approve → commit choreography.

use async_trait::async_trait;
use llm_client::{ImageData, Provider, ToolCall, ToolDef};
use serde_json::Value;

/// How a tool affects the world — the loop's dispatch discriminator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolEffect {
    /// Reads only; never changes project state (search, info, render, validate).
    ReadOnly,
    /// Mutates working/draft state the loop does not gate (drafting, board
    /// pipeline steps that write only into the project's scratch state).
    Authoring,
    /// A human-gated write: previewed, approved, then committed.
    Gated,
}

/// Which pass of a gated tool the loop is asking the provider to run. ReadOnly /
/// Authoring tools always run [`RunMode::Normal`] and ignore this.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunMode {
    /// A normal single run (every non-gated tool, and a gated tool the model did
    /// not intend to apply).
    Normal,
    /// A gated tool's dry-run: produce the diff to approve, WITHOUT writing.
    Preview,
    /// A gated tool's real write, after approval.
    Commit,
}

/// What a [`RunMode::Preview`] / [`RunMode::Commit`] of a gated tool produced —
/// the structured facts the loop's gate and its [`crate::AgentEvent::Applied`]
/// emission need, lifted out of the domain JSON.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ApplyInfo {
    /// Preview: there is a valid change to approve (e.g. the YAML compiled). When
    /// `false`, the loop returns the preview value straight to the model to
    /// self-repair, with NO approval prompt.
    pub ready: bool,
    /// Commit: the write actually landed.
    pub committed: bool,
    /// A short human-readable summary of the committed write (e.g. ERC counts),
    /// carried in [`crate::AgentEvent::Applied`].
    pub summary: String,
}

/// An independent post-turn review of the domain's committed work (the netlist
/// analog of a layout critic). Returned by [`ToolProvider::review_committed`]
/// and folded into the review→fix loop by [`crate::Agent::run_turn_reviewed`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReviewOutcome {
    /// Overall correctness score (0-10).
    pub score: f64,
    /// High-confidence defect lines to feed back as a fix turn (empty = clean).
    pub defects: Vec<String>,
}

/// The result of running one tool: the JSON the model reads back, any images to
/// attach to the tool result, and — for a gated tool — its [`ApplyInfo`].
#[derive(Clone, Debug, Default)]
pub struct ToolOutcome {
    /// The structured JSON result fed back to the model as text.
    pub value: Value,
    /// Images to attach to the tool result (e.g. a rendered schematic).
    pub images: Vec<ImageData>,
    /// The on-disk path of the image a render tool produced, if any. The model
    /// sees the base64 in [`Self::images`]; the *path* is kept here so a UI can
    /// display the same PNG inline (rather than re-encoding it). Set by the
    /// domain when it strips its image-path key out of `value`.
    pub image_path: Option<String>,
    /// Gated-tool apply facts; `None` for ReadOnly / Authoring tools and for a
    /// gated tool run in [`RunMode::Normal`].
    pub apply: Option<ApplyInfo>,
}

impl ToolOutcome {
    /// A plain result (no images, not a gated apply).
    pub fn plain(value: Value) -> Self {
        Self { value, images: Vec::new(), image_path: None, apply: None }
    }
}

/// Everything the agent loop needs to expose and run a domain's tools.
///
/// The loop holds a `Box<dyn ToolProvider>` and never names a concrete tool. A
/// domain implementation (e.g. the KiCAD PCB tools) owns the synchronous tool
/// bodies and any non-`Send` state, and is responsible for off-loading blocking
/// work itself (so this trait's `run` can stay a clean async method).
#[async_trait]
pub trait ToolProvider: Send + Sync {
    /// The JSON-Schema tool definitions handed to the model.
    fn defs(&self) -> Vec<ToolDef>;

    /// The effect class of the named tool (drives the loop's gate dispatch).
    /// An unknown name should return [`ToolEffect::ReadOnly`] (it will error on
    /// run anyway).
    fn effect(&self, name: &str) -> ToolEffect;

    /// Whether this call to a [`ToolEffect::Gated`] tool intends to APPLY (write).
    /// A gated tool the model previews (no write intent) returns `false` and runs
    /// once in [`RunMode::Normal`]. Non-gated tools never reach this.
    fn wants_apply(&self, call: &ToolCall) -> bool;

    /// Execute one tool. `mode` selects the gated-tool pass; `reviewer` is the
    /// LLM client a tool may need for an in-flow independent review (the same
    /// client the loop drives). Tool errors should be returned as a structured
    /// `{"error": …}` value (not propagated) so the model can self-repair.
    async fn run(&self, call: &ToolCall, mode: RunMode, reviewer: &dyn Provider) -> ToolOutcome;

    /// A short one-line digest of a finished tool call for a collapsed UI card.
    /// Default: `"done"` (domains override for richer summaries).
    fn summary(&self, _call: &ToolCall, _result: &Value) -> String {
        "done".to_string()
    }

    /// Whether a finished turn that did domain *authoring* work but never
    /// committed should be NUDGED to finish + commit. The loop scopes its
    /// "ended without a committed design" re-prompt to calls where this is true.
    /// Default: `false` (no tool is authoring-for-commit).
    fn is_authoring_for_commit(&self, _name: &str) -> bool {
        false
    }

    /// The re-prompt text the loop sends when a stalled authoring turn is nudged.
    /// Only consulted when [`Self::is_authoring_for_commit`] fired this turn.
    fn commit_nudge(&self) -> &str {
        "Your turn ended without committing the work. Finish it now and commit \
         before ending your turn."
    }

    /// An INDEPENDENT review of the domain's just-committed work, for
    /// [`crate::Agent::run_turn_reviewed`]. `intent` is the design goal; the
    /// `reviewer` is the loop's LLM client (used history-free, so it doesn't
    /// rationalise the generating model's choices). Returns `None` when there is
    /// nothing committed to review yet. Default: `None` (no review).
    async fn review_committed(
        &self,
        _intent: &str,
        _reviewer: &dyn Provider,
    ) -> Option<crate::ReviewOutcome> {
        None
    }

    /// The text fed back as a fix turn when [`Self::review_committed`] finds
    /// high-confidence defects. `{defects}` lists them. Domains override to
    /// phrase the fix instruction in their own terms.
    fn fix_prompt(&self, defects: &[String]) -> String {
        format!(
            "An INDEPENDENT review of the work you just committed found these \
             high-confidence defects:\n{}\n\nFix each one and re-commit.",
            defects.join("\n")
        )
    }
}
