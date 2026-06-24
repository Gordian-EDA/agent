//! [`PcbTools`] — the KiCAD domain as a [`gordian_core::ToolProvider`].
//!
//! `PcbTools` owns the [`PcbToolCtx`] (KiCAD env, project paths, symbol caches,
//! the live IPC session) behind an [`Arc`]. Because `PcbToolCtx` is **not** `Send`
//! across threads in the way the loop wants (its symbol/footprint caches and the
//! IPC session are interior-mutable), THIS provider owns the `spawn_blocking`:
//! [`ToolProvider::run`] off-loads the synchronous [`crate::tools::run_tool`]
//! dispatch onto the blocking pool, so a compile / render / `kicad-cli` subprocess
//! never stalls a single-threaded UI runtime — and `gordian_core`'s trait method
//! can stay a clean `async fn`.
//!
//! ## The apply-gate, re-expressed as effect tags
//!
//! `apply_design` is the one [`ToolEffect::Gated`] tool. The loop drives it through
//! [`RunMode::Preview`] → approve → [`RunMode::Commit`]; here that maps onto the
//! existing dry-run / commit `apply_design` body byte-for-byte — the provider only
//! forces `commit:false`/`commit:true` and lifts the dry-run `ok` and the commit
//! `written` + ERC counts into [`ApplyInfo`]. `review_design` (async, needs the LLM
//! client) is handled here too, exactly as the old loop did.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use base64::Engine as _;
use gordian_core::{
    ApplyInfo, ImageData, Provider, ReviewOutcome, RunMode, ToolCall, ToolDef, ToolEffect,
    ToolOutcome, ToolProvider,
};
use serde_json::{Value, json};

use crate::tools::{IMAGE_PATH_KEY, PcbToolCtx, run_tool, tool_defs};

/// The KiCAD PCB/schematic tools, as a [`gordian_core::ToolProvider`]. Holds the
/// shared [`PcbToolCtx`] and off-loads every (synchronous) tool onto the blocking
/// pool.
pub struct PcbTools {
    ctx: Arc<PcbToolCtx>,
}

impl PcbTools {
    /// Build the provider over a project's [`PcbToolCtx`].
    pub fn new(ctx: PcbToolCtx) -> Self {
        Self { ctx: Arc::new(ctx) }
    }

    /// The tool context (project paths, KiCAD env). Exposed for callers that want
    /// to inspect the `.kicad_sch` path after a turn.
    pub fn ctx(&self) -> &PcbToolCtx {
        &self.ctx
    }

    /// Run one synchronous tool on the blocking pool. Tools can take seconds
    /// (symbol-index build, reconciled render, ERC subprocess); off-loading them
    /// keeps an interactive caller redrawing.
    async fn run_blocking(&self, name: &str, input: Value) -> Result<Value> {
        let ctx = Arc::clone(&self.ctx);
        let name = name.to_string();
        tokio::task::spawn_blocking(move || run_tool(&name, input, &ctx))
            .await
            .map_err(|e| anyhow::anyhow!("tool execution task failed: {e}"))?
    }

    /// The `review_design` tool: an INDEPENDENT electrical-correctness review of the
    /// current draft, called by the agent in-flow. Runs the FRESH diverse-lens LLM
    /// review (no conversation history) UNIONED with the deterministic exact-math
    /// ERC, and returns the score + high-confidence functional defects.
    async fn review_design(&self, input: &Value, reviewer: &dyn Provider) -> Result<Value> {
        let intent = input.get("intent").and_then(Value::as_str).unwrap_or("");
        let dv = self.run_blocking("get_design", json!({})).await?;
        let netlist = dv.get("yaml").and_then(Value::as_str).unwrap_or_default().to_string();
        if netlist.trim().is_empty() {
            return Ok(json!({
                "error": "no design to review yet — build one with create_design/edit_design (or apply_design) first",
            }));
        }
        let (score, defects) = self.review_netlist_with_erc(reviewer, intent, &netlist).await?;
        let note = if defects.is_empty() {
            "no high-confidence functional defects — the design looks electrically sound"
        } else {
            "high-confidence functional defects found (they pass ERC but are electrically wrong); \
             fix each with edit_design and re-check"
        };
        Ok(json!({ "score": score, "defects": defects, "note": note }))
    }

    /// Run the diverse-lens LLM review on `netlist` and UNION in the deterministic
    /// exact-math ERC (feedback-divider ratios, LED current, dangling/crystal/
    /// polarity) — deduped by refdes so a fault both layers find isn't doubled.
    async fn review_netlist_with_erc(
        &self,
        reviewer: &dyn Provider,
        intent: &str,
        netlist: &str,
    ) -> Result<(f64, Vec<String>)> {
        let (score, mut defects) = crate::review::review_netlist(reviewer, intent, netlist).await?;
        if let Some(design) = circuit_lang::compile(netlist, self.ctx.provider()).design {
            for d in circuit_lang::erc::erc_checks(&design) {
                if !defects.iter().any(|e| crate::review::same_defect(e, &d)) {
                    defects.push(d);
                }
            }
        }
        Ok((score, defects))
    }
}

#[async_trait]
impl ToolProvider for PcbTools {
    fn defs(&self) -> Vec<ToolDef> {
        tool_defs()
    }

    fn effect(&self, name: &str) -> ToolEffect {
        match name {
            // The one human-gated write.
            "apply_design" => ToolEffect::Gated,
            // Tools that mutate the working draft / board scratch state.
            "create_design" | "edit_design" | "derive_board" | "assign_footprint"
            | "place_board" | "route_board" | "autoroute" | "export_board" | "open_board"
            | "move_part" | "route_track" | "set_net_width" => ToolEffect::Authoring,
            // Everything else reads only.
            _ => ToolEffect::ReadOnly,
        }
    }

    fn wants_apply(&self, call: &ToolCall) -> bool {
        call.name == "apply_design" && wants_commit(&call.input)
    }

    fn is_authoring_for_commit(&self, name: &str) -> bool {
        // Schematic research/authoring work — the kind of turn whose deliverable
        // is a committed design. Scopes the "ended without a committed design"
        // nudge to schematic turns only, so the PCB board flow (which never calls
        // apply_design) is never spuriously nudged.
        matches!(
            name,
            "search_symbols" | "get_symbol_info" | "create_design" | "edit_design" | "apply_design"
        )
    }

    fn commit_nudge(&self) -> &str {
        "Your turn ended without a committed design — nothing was written. You MUST \
         finish the schematic now: call `create_design`/`edit_design` to author the \
         full design, then `apply_design(commit:true)` to commit it. Do this now \
         before ending your turn."
    }

    async fn run(&self, call: &ToolCall, mode: RunMode, reviewer: &dyn Provider) -> ToolOutcome {
        // review_design needs the LLM client + async, so it can't ride the sync
        // dispatch — handle it here like the old loop did.
        if call.name == "review_design" {
            return into_outcome(self.review_design(&call.input, reviewer).await, None);
        }

        // The gated apply: the loop drives Preview/Commit; map each onto the
        // dry-run / commit `apply_design` body, lifting the gate facts into ApplyInfo.
        if call.name == "apply_design" {
            match mode {
                RunMode::Preview => {
                    let mut input = call.input.clone();
                    input["commit"] = json!(false);
                    let dry = self.run_blocking("apply_design", input).await;
                    // `ready` = the YAML compiled (dry.ok == true); otherwise the loop
                    // returns the diagnostics straight back with no approval prompt.
                    let ready = dry
                        .as_ref()
                        .ok()
                        .and_then(|v| v.get("ok").and_then(Value::as_bool))
                        == Some(true);
                    return into_outcome(dry, Some(ApplyInfo { ready, ..Default::default() }));
                }
                RunMode::Commit => {
                    let mut input = call.input.clone();
                    input["commit"] = json!(true);
                    let committed = self.run_blocking("apply_design", input).await;
                    let apply = committed.as_ref().ok().map(|v| {
                        let written = v.get("written").and_then(Value::as_bool) == Some(true);
                        let errors =
                            v.pointer("/erc/errors").and_then(Value::as_u64).unwrap_or(0);
                        let warnings =
                            v.pointer("/erc/warnings").and_then(Value::as_u64).unwrap_or(0);
                        ApplyInfo {
                            ready: true,
                            committed: written,
                            summary: format!("ERC {errors} errors, {warnings} warnings"),
                        }
                    });
                    return into_outcome(committed, apply);
                }
                // A normal (commit-less) apply_design: dry-run, no gate.
                RunMode::Normal => {}
            }
        }

        into_outcome(self.run_blocking(&call.name, call.input.clone()).await, None)
    }

    fn summary(&self, call: &ToolCall, result: &Value) -> String {
        tool_summary(&call.name, &call.input, result)
    }

    async fn review_committed(
        &self,
        intent: &str,
        reviewer: &dyn Provider,
    ) -> Option<ReviewOutcome> {
        let sch = self.ctx.sch_path();
        if !sch.exists() {
            return None;
        }
        let netlist = sch_layout::read::lift(self.ctx.env(), sch).ok()?;
        let (score, defects) = self.review_netlist_with_erc(reviewer, intent, &netlist).await.ok()?;
        Some(ReviewOutcome { score, defects })
    }
}

/// Turn a tool's `Result<Value>` into a [`ToolOutcome`]: a tool error becomes a
/// structured `{error: …}` value (the model self-repairs), images are pulled out
/// of the value via [`take_images`], and `apply` rides along for a gated tool.
fn into_outcome(result: Result<Value>, apply: Option<ApplyInfo>) -> ToolOutcome {
    match result {
        Ok(mut value) => {
            let images = take_images(&mut value);
            ToolOutcome { value, images, apply }
        }
        Err(e) => ToolOutcome { value: json!({ "error": e.to_string() }), images: Vec::new(), apply },
    }
}

/// Pull a `_image_path` out of a tool result: load + base64 the PNG, strip the key
/// so the model's text view stays clean. An unreadable file degrades to "no image".
fn take_images(value: &mut Value) -> Vec<ImageData> {
    let Some(path) =
        value.get(IMAGE_PATH_KEY).and_then(Value::as_str).map(str::to_string)
    else {
        return Vec::new();
    };
    if let Some(obj) = value.as_object_mut() {
        obj.remove(IMAGE_PATH_KEY);
    }
    match std::fs::read(&path) {
        Ok(bytes) => vec![ImageData {
            format: "png".to_string(),
            base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        }],
        Err(e) => {
            eprintln!("render image unreadable at {path}: {e}");
            Vec::new()
        }
    }
}

/// Whether an `apply_design` input intends to write (`commit: true`).
fn wants_commit(input: &Value) -> bool {
    input.get("commit").and_then(Value::as_bool) == Some(true)
}

/// A short, human-readable one-liner for a finished tool call, used to label a
/// collapsed tool-call card in the UI. Reads the structured JSON result.
fn tool_summary(name: &str, input: &Value, result: &Value) -> String {
    if let Some(err) = result.get("error").and_then(Value::as_str) {
        return format!("error: {err}");
    }
    match name {
        "search_symbols" => {
            let q = input.get("query").and_then(Value::as_str).unwrap_or("");
            let n = result.get("hits").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
            format!("\"{q}\" → {n} hits")
        }
        "get_symbol_info" => {
            let lib = input.get("lib_id").and_then(Value::as_str).unwrap_or("");
            let n = result.get("pins").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
            format!("{lib} → {n} pins")
        }
        "get_design" => "lifted current design".to_string(),
        "validate_design" => {
            let errors = result.get("errors").and_then(Value::as_u64).unwrap_or(0);
            let warnings = result.get("warnings").and_then(Value::as_u64).unwrap_or(0);
            format!("{errors} errors, {warnings} warnings")
        }
        "apply_design" => {
            if result.get("written").and_then(Value::as_bool) == Some(true) {
                let errors = result.pointer("/erc/errors").and_then(Value::as_u64).unwrap_or(0);
                format!("written (ERC {errors} errors)")
            } else if result.get("rejected").and_then(Value::as_bool) == Some(true) {
                "rejected".to_string()
            } else if result.get("would_write").and_then(Value::as_bool) == Some(true) {
                let added =
                    result.pointer("/diff/added").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
                let removed = result
                    .pointer("/diff/removed")
                    .and_then(Value::as_array)
                    .map(Vec::len)
                    .unwrap_or(0);
                format!("preview: +{added} -{removed}")
            } else {
                "ok".to_string()
            }
        }
        "run_erc" => {
            let errors = result.get("errors").and_then(Value::as_u64).unwrap_or(0);
            let warnings = result.get("warnings").and_then(Value::as_u64).unwrap_or(0);
            format!("{errors} errors, {warnings} warnings")
        }
        "project_info" => result
            .get("sch_path")
            .and_then(Value::as_str)
            .unwrap_or("project state")
            .to_string(),
        "read_schematic" => {
            let path = input.get("path").and_then(Value::as_str).unwrap_or("?");
            format!("lifted {path}")
        }
        "render_schematic" => "rendered schematic to PNG".to_string(),
        "review_design" => {
            let score = result.get("score").and_then(Value::as_f64).unwrap_or(0.0);
            let n = result.get("defects").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
            if n == 0 {
                format!("score {score:.0}/10 — clean")
            } else {
                format!("score {score:.0}/10 — {n} defect(s) to fix")
            }
        }
        _ => "done".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_summary_reads_structured_results() {
        let s = tool_summary(
            "search_symbols",
            &json!({ "query": "STM32" }),
            &json!({ "hits": [1, 2, 3] }),
        );
        assert_eq!(s, "\"STM32\" → 3 hits");

        let s = tool_summary(
            "apply_design",
            &json!({}),
            &json!({ "written": true, "erc": { "errors": 0 } }),
        );
        assert!(s.contains("written"), "got: {s}");

        let s = tool_summary("get_design", &json!({}), &json!({ "error": "boom" }));
        assert_eq!(s, "error: boom");
    }

    #[test]
    fn wants_commit_detects_true_only() {
        assert!(wants_commit(&json!({ "yaml": "x", "commit": true })));
        assert!(!wants_commit(&json!({ "yaml": "x", "commit": false })));
        assert!(!wants_commit(&json!({ "yaml": "x" })));
    }
}
