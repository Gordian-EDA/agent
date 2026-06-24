//! The minimal gating vocabulary the turn loop drives KiCAD tools through.
//!
//! There is exactly one domain (KiCAD, forever), so there is no tool-provider
//! trait — the loop calls [`crate::tools::run_tool`] / [`crate::tools::tool_defs`]
//! directly (off-loaded onto a blocking pool at the call site). These types are
//! just the structured facts the gate's choreography needs:
//!
//! - [`ToolEffect`] classifies a tool name (`ReadOnly` | `Authoring` | `Gated`).
//! - A [`ToolEffect::Gated`] write (`apply_design` with `commit:true`) is driven
//!   through preview → approve → commit ([`RunMode`]), reporting via [`ApplyInfo`].
//! - An independent post-turn review comes back as a [`ReviewOutcome`].
//!
//! The KiCAD-concrete classifiers and the commit-forcing / `ApplyInfo`-lifting body
//! live in [`crate::agent`] alongside the loop they serve.

use llm_client::ImageData;
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

/// Which pass of a gated tool the loop is asking for. ReadOnly / Authoring tools
/// always run [`RunMode::Normal`] and ignore this.
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

/// An independent post-turn review of the committed work (the netlist + vision
/// layout critic), folded into the review→fix loop by
/// [`crate::Agent::run_turn_reviewed`].
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
    /// display the same PNG inline (rather than re-encoding it).
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
