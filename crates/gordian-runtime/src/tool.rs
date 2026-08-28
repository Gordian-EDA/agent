//! The minimal gating vocabulary the turn loop drives KiCAD tools through.
//!
//! There is exactly one domain (KiCAD, forever), so there is no tool-provider
//! trait — the loop calls the tool registry (`tools::run_tool` / `tools::tool_defs` in
//! `gordian-core`) directly (off-loaded onto a blocking pool at the call site). These types are
//! just the structured facts the gate's choreography needs:
//!
//! - [`ToolEffect`] distinguishes reads, draft authoring, pre-execution approval,
//!   and previewed approval.
//! - A [`ToolEffect::Gated`] write (`apply_design`) is driven
//!   through preview → approve → commit ([`RunMode`]), reporting via [`ApplyInfo`].
//! - A [`ToolEffect::ApprovalRequired`] mutation that has no dry-run is approved
//!   from its operation name and arguments before it executes once.
//! - An independent post-turn review comes back as a [`ReviewOutcome`].
//!
//! The KiCAD-concrete classifiers and the commit-forcing / `ApplyInfo`-lifting body
//! live in `gordian-core`'s agent module alongside the loop they serve.

use anyhow::{Result, anyhow, bail};
use gordian_llm::Binary;
use kicad_footprint::FootprintId;
use serde_json::{Value, json};

use crate::AgentRuntime;

/// How a tool affects the world — the loop's dispatch discriminator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolEffect {
    /// Reads only; never changes project state (search, info, render, validate).
    ReadOnly,
    /// Mutates only project-local draft authoring state; a later gated apply is
    /// required before this affects the schematic.
    Authoring,
    /// Mutates project files or a live KiCAD board and must be approved before
    /// its first and only execution because it has no dry-run implementation.
    ApprovalRequired,
    /// A human-gated write: previewed, approved, then committed.
    Gated,
}

/// Which pass of a preview-capable gated tool the loop is asking for. Other
/// tools always run [`RunMode::Normal`] and ignore this.
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
    pub images: Vec<Binary>,
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
        Self {
            value,
            images: Vec::new(),
            image_path: None,
            apply: None,
        }
    }
}

/// Pull a required string field out of the input, with a clear error.
pub fn require_str(input: &Value, key: &str) -> Result<String> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("missing required string field `{key}`"))
}

pub fn require_search_query(input: &Value) -> Result<String> {
    let query = require_str(input, "query")?;
    if query.trim().is_empty() {
        bail!("search query must contain non-whitespace text");
    }
    Ok(query)
}

/// Build the `{ok, diagnostics, errors, warnings}` report a compile yields.
pub fn compile_report(diags: &circuit_lang::Diagnostics) -> Value {
    use circuit_lang::Severity;
    use std::collections::{BTreeMap, HashMap, HashSet};

    const MAX_DIAGNOSTICS: usize = 40;
    const MAX_REPRESENTATIVES_PER_CODE: usize = 6;

    let mut code_counts = BTreeMap::<&str, (usize, usize)>::new();
    for d in &diags.0 {
        let counts = code_counts.entry(d.code).or_default();
        match d.severity {
            Severity::Error => counts.0 += 1,
            Severity::Warning => counts.1 += 1,
        }
    }

    // Reserve one representative for every diagnostic class before allowing a
    // repetitive class to consume the remaining context budget. This keeps a
    // large syntax-error family from hiding later pin or electrical errors.
    let mut selected = vec![false; diags.0.len()];
    let mut represented_codes = HashSet::new();
    let mut selected_count = 0usize;
    for (index, d) in diags.0.iter().enumerate() {
        if selected_count == MAX_DIAGNOSTICS {
            break;
        }
        if represented_codes.insert(d.code) {
            selected[index] = true;
            selected_count += 1;
        }
    }
    let mut representatives_per_code = represented_codes
        .into_iter()
        .map(|code| (code, 1usize))
        .collect::<HashMap<_, _>>();
    for (index, d) in diags.0.iter().enumerate() {
        if selected_count == MAX_DIAGNOSTICS {
            break;
        }
        let count = representatives_per_code.entry(d.code).or_default();
        if !selected[index] && *count < MAX_REPRESENTATIVES_PER_CODE {
            selected[index] = true;
            selected_count += 1;
            *count += 1;
        }
    }
    let strings = diags
        .0
        .iter()
        .zip(selected)
        .filter(|(_, selected)| *selected)
        .map(|(d, _)| d.to_string())
        .collect::<Vec<_>>();
    let omitted = diags.0.len() - strings.len();
    let errors = diags
        .0
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .count();
    let warnings = diags
        .0
        .iter()
        .filter(|d| d.severity == Severity::Warning)
        .count();
    let mut report = json!({
        "ok": errors == 0,
        "diagnostics": strings,
        "diagnostic_code_counts": code_counts.into_iter().map(|(code, (errors, warnings))| {
            (code.to_owned(), json!({ "errors": errors, "warnings": warnings }))
        }).collect::<serde_json::Map<_, _>>(),
        "errors": errors,
        "warnings": warnings,
    });
    if omitted > 0 {
        report["diagnostics_omitted"] = json!(omitted);
        report["note"] = json!(
            "diagnostics are representative and truncated by code for context efficiency; diagnostic_code_counts preserves exact totals"
        );
    }
    report
}

/// `"; suggestions: a, b"` for a non-empty candidate list, empty otherwise —
/// a diagnostic never ends in a dangling `suggestions:`.
pub fn footprint_suggestion_clause(suggestions: &[FootprintId]) -> String {
    if suggestions.is_empty() {
        return String::new();
    }
    let list = suggestions
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    format!("; suggestions: {list}")
}

pub fn current_sch_text(ctx: &AgentRuntime) -> Option<String> {
    std::fs::read_to_string(ctx.sch_path()).ok()
}

pub const IMAGE_PATH_KEY: &str = "_image_path";
