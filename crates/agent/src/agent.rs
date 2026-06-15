//! The agent loop with a human apply-gate.
//!
//! [`Agent::run_turn`] drives one user turn: it builds the system prompt (the
//! circuit-YAML language spec + workflow doctrine + real-library guidance), then
//! repeatedly calls the [`LlmClient`], executing each tool the model requests and
//! feeding the structured result back, until the model returns a final text (or a
//! safety iteration cap is hit).
//!
//! ## Design state is tool-pulled
//!
//! The whole design is **never** dumped into the system prompt. The model fetches
//! it on demand via `get_design`, keeps context small, and self-repairs off the
//! structured diagnostics that the tools return.
//!
//! ## Context is persistent
//!
//! The conversation lives in [`Agent::history`] and is carried across turns, so
//! "are you done?" after a build refers to the build. The history can be
//! unwound one turn at a time ([`Agent::pop_last_turn`]), cleared
//! ([`Agent::clear_history`]), or compacted into a summary ([`Agent::compact`]).
//!
//! ## The apply-gate (dry-run → approve → commit)
//!
//! `apply_design` is the only write. When the model asks to commit
//! (`commit: true`), the loop does **not** write immediately. It:
//!
//! 1. Re-runs `apply_design` as a **dry-run** (`commit: false`) to obtain the
//!    structured diff without touching disk.
//! 2. Hands that diff to [`Approvals::approve`].
//! 3. On approval, re-runs `apply_design` with `commit: true` (the real write +
//!    snapshot + ERC) and marks the turn `applied`.
//! 4. On rejection, feeds a "user rejected" tool result back to the model so it
//!    can react, and writes nothing.
//!
//! [`AutoApprove`] is the headless test/automation implementation; the TUI
//! supplies an interactive one later.

use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;

use crate::llm::{ContentBlock, ImageData, LlmClient, Message, Role};
use crate::tools::{ToolCtx, Tools};

/// Safety cap on LLM round-trips per turn. Generous enough for
/// search → info → validate → apply self-repair, bounded so a misbehaving model
/// can't loop forever.
const MAX_ITERATIONS: usize = 12;

/// The human apply-gate. The loop calls [`Approvals::approve`] with the dry-run
/// diff before any `apply_design` write; returning `false` cancels the write.
///
/// `approve` is **async**: in the TUI the gate blocks the agent turn until the
/// user presses `a`/`r`, which is inherently a wait on another task. Headless
/// implementations ([`AutoApprove`]) return immediately.
#[async_trait]
pub trait Approvals: Send {
    /// Decide whether to commit the proposed change, given the dry-run diff
    /// (the `apply_design` dry-run JSON: `{ok, would_write, diff, rendered_len}`).
    async fn approve(&mut self, diff: &Value) -> bool;
}

/// A non-interactive [`Approvals`] that always answers the same way. Used by
/// tests and headless automation (`autopcb agent`).
pub struct AutoApprove {
    answer: bool,
}

impl AutoApprove {
    /// Always approve.
    pub fn yes() -> Self {
        Self { answer: true }
    }

    /// Always reject.
    pub fn no() -> Self {
        Self { answer: false }
    }
}

#[async_trait]
impl Approvals for AutoApprove {
    async fn approve(&mut self, _diff: &Value) -> bool {
        self.answer
    }
}

/// A summary of one finished turn, carried in [`AgentEvent::TurnDone`] so the UI
/// can update its counters without owning the loop's internals.
#[derive(Clone, Debug)]
pub struct TurnOutcomeSummary {
    /// Whether an approved write actually committed this turn.
    pub applied: bool,
    /// How many tool calls the loop executed.
    pub tool_calls_made: usize,
    /// The model's final text reply.
    pub final_text: String,
}

/// Events the agent loop emits as it runs, for a live UI. The headless paths
/// pass `None` and never see these.
#[derive(Clone, Debug)]
pub enum AgentEvent {
    /// The model produced assistant text (interleaved with tool calls or final).
    AssistantText(String),
    /// A tool call is about to run.
    ToolStarted { name: String },
    /// A tool call finished; `summary` is a short one-line digest for a card.
    ToolFinished { name: String, summary: String },
    /// An approved `apply_design` committed; carries post-write ERC counts.
    Applied { errors: usize, warnings: usize },
    /// Provider-reported token usage for one model call. `input_tokens` is the
    /// full prompt size (system + history + tools) — i.e. the live context.
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },
    /// `compact` replaced the conversation history with a summary pair.
    Compacted {
        messages_before: usize,
        messages_after: usize,
    },
    /// The turn finished.
    TurnDone(TurnOutcomeSummary),
}

/// Counters describing the live conversation context, for a `/context` view.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ContextStats {
    /// User turns currently held (and unwindable) in the history.
    pub turns: usize,
    /// Messages in the history (user, assistant, and tool-result messages).
    pub messages: usize,
    /// Total characters across all blocks — a rough proxy for tokens (~4
    /// chars/token) when the provider has not reported usage yet.
    pub approx_chars: usize,
}

/// A typed handle for the optional UI event sink. `None` is the headless case.
type Events<'a> = Option<&'a UnboundedSender<AgentEvent>>;

/// Best-effort emit: a closed receiver (UI gone) is ignored.
fn emit(events: Events<'_>, ev: AgentEvent) {
    if let Some(tx) = events {
        let _ = tx.send(ev);
    }
}

/// Why a [`Agent::run_turn`] stopped. Distinguishes a clean finish from the
/// safety-cap cutoff so the UI can tell the user the turn was *truncated* rather
/// than completed. (Interruptions and provider errors never reach here — they
/// surface on the shell's join path, not as a returned `TurnOutcome`.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// The model returned a final text with no pending tool calls — done.
    Completed,
    /// The loop hit [`MAX_ITERATIONS`] before the model finished; the turn was
    /// cut off mid-work. The conversation persists, so a follow-up "continue"
    /// resumes it.
    IterationCap,
}

/// The result of one [`Agent::run_turn`].
#[derive(Clone, Debug)]
pub struct TurnOutcome {
    /// Whether an approved `apply_design` write actually committed this turn.
    pub applied: bool,
    /// The model's final text reply.
    pub final_text: String,
    /// How many tool calls the loop executed (counts each model-requested call;
    /// the dry-run probe before an approved commit is internal and not counted).
    pub tool_calls_made: usize,
    /// Whether the loop finished cleanly or was cut off at the iteration cap.
    pub stop_reason: StopReason,
}

/// An agent session over one project. Holds the LLM client, the tool context
/// (KiCAD env + project paths + symbol provider + snapshot store), and the tool
/// registry.
///
/// The context lives in an [`Arc`] because every tool call executes on the
/// blocking thread pool (see [`Agent::run_tool_blocking`]) — a compile, render,
/// or `kicad-cli` subprocess must never stall the caller's (possibly
/// single-threaded, UI-owning) async runtime.
pub struct Agent {
    client: Box<dyn LlmClient>,
    ctx: Arc<ToolCtx>,
    tools: Tools,
    /// The whole session's conversation, carried across turns so the model
    /// remembers what it did. Tool results live here too — context is
    /// everything the next request will see.
    history: Vec<Message>,
    /// `history.len()` at the start of each user turn, so [`Agent::pop_last_turn`]
    /// can unwind exactly one exchange.
    turn_starts: Vec<usize>,
}

impl Agent {
    /// Build an agent over a project's [`ToolCtx`]. The conversation `client`
    /// drives the agent loop; `apply_design` derives its layout frame from the
    /// netlist (`sch_layout::floorplan::infer_ir`), so no separate layout client
    /// is involved.
    pub fn new(client: Box<dyn LlmClient>, ctx: ToolCtx) -> Self {
        Self {
            client,
            ctx: Arc::new(ctx),
            tools: Tools::new(),
            history: Vec::new(),
            turn_starts: Vec::new(),
        }
    }

    /// Drop the entire conversation history (a fresh start; files untouched).
    pub fn clear_history(&mut self) {
        self.history.clear();
        self.turn_starts.clear();
    }

    /// Unwind the most recent user turn: remove its user message and everything
    /// after it from the history. Returns `false` when there is nothing to pop
    /// (fresh agent, or everything before a compaction barrier).
    pub fn pop_last_turn(&mut self) -> bool {
        self.pop_turns(1) == 1
    }

    /// Unwind the `k` most recent turns at once, returning how many were actually
    /// popped (fewer than `k` once the history is exhausted or a compaction
    /// barrier is hit). The selector built from [`Agent::unwindable_turns`] uses
    /// this to roll back to an arbitrary point.
    pub fn pop_turns(&mut self, k: usize) -> usize {
        pop_n(&mut self.history, &mut self.turn_starts, k)
    }

    /// Prompt previews for every turn that can still be unwound, newest first —
    /// the rows the double-Esc unwind picker offers.
    pub fn unwindable_turns(&self) -> Vec<String> {
        turn_previews(&self.history, &self.turn_starts)
    }

    /// Counters for the live context (turns / messages / approximate size).
    pub fn context_stats(&self) -> ContextStats {
        let approx_chars = self
            .history
            .iter()
            .flat_map(|m| &m.content)
            .map(|b| match b {
                ContentBlock::Text(t) => t.len(),
                ContentBlock::ToolUse { input, .. } => input.to_string().len(),
                ContentBlock::ToolResult { content, .. } => content.len(),
            })
            .sum();
        ContextStats {
            turns: self.turn_starts.len(),
            messages: self.history.len(),
            approx_chars,
        }
    }

    /// Compact the conversation: one tool-less model call summarizes the
    /// history, which is then replaced by a `[user summary, assistant ack]`
    /// pair. Returns `(messages_before, messages_after)` and emits
    /// [`AgentEvent::Compacted`]. Compaction is a barrier: prior turns can no
    /// longer be unwound.
    pub async fn compact(&mut self, events: Events<'_>) -> Result<(usize, usize)> {
        let before = self.history.len();
        if before == 0 {
            return Ok((0, 0));
        }
        repair_history(&mut self.history);

        // Tool defs stay in the request: Converse rejects histories containing
        // toolUse/toolResult blocks unless a toolConfig is present.
        let defs = self.tools.defs();
        let mut messages = self.history.clone();
        messages.push(Message::user(COMPACT_PROMPT));
        let completion = self
            .client
            .complete(&system_prompt(), &messages, &defs)
            .await?;
        emit(
            events,
            AgentEvent::Usage {
                input_tokens: completion.input_tokens,
                output_tokens: completion.output_tokens,
            },
        );

        let summary = completion.text.trim().to_string();
        if summary.is_empty() {
            anyhow::bail!("compaction failed: the model returned no summary text");
        }
        self.history = vec![
            Message::user(format!(
                "[Conversation summary — earlier context was compacted]\n{summary}"
            )),
            Message::assistant("Understood — I'll continue from that summary."),
        ];
        self.turn_starts.clear();
        let after = self.history.len();
        emit(
            events,
            AgentEvent::Compacted {
                messages_before: before,
                messages_after: after,
            },
        );
        Ok((before, after))
    }

    /// The tool context (project paths, KiCAD env). Exposed for callers that
    /// want to inspect the `.kicad_sch` path after a turn.
    pub fn ctx(&self) -> &ToolCtx {
        &self.ctx
    }

    /// Execute one tool on the blocking pool. Tools are synchronous and can
    /// take seconds (symbol-index build, reconciled render, ERC subprocess);
    /// off-loading them keeps an interactive caller redrawing.
    async fn run_tool_blocking(&self, name: &str, input: Value) -> Result<Value> {
        let ctx = Arc::clone(&self.ctx);
        let name = name.to_string();
        tokio::task::spawn_blocking(move || Tools::new().run(&name, input, &ctx))
            .await
            .context("tool execution task failed")?
    }

    /// Drive one user turn to completion.
    ///
    /// Loops: call the model → run any requested tools (gating `apply_design`
    /// commits through `approvals`) → feed results back → repeat, until the model
    /// returns a final text with no pending tool calls, or [`MAX_ITERATIONS`] is
    /// reached.
    ///
    /// The turn appends to the agent's persistent history, so later turns see
    /// the full conversation. A turn that was cancelled mid-flight may leave a
    /// ragged tail (a dangling `tool_use`); [`repair_history`] patches that
    /// before the new turn starts.
    ///
    /// `events`, when `Some`, receives [`AgentEvent`]s as the loop runs so a UI
    /// can render assistant text and tool-call cards live. Headless callers pass
    /// `None`.
    pub async fn run_turn(
        &mut self,
        user_msg: &str,
        approvals: &mut dyn Approvals,
        events: Events<'_>,
    ) -> Result<TurnOutcome> {
        let system = system_prompt();
        let defs = self.tools.defs();

        repair_history(&mut self.history);
        self.turn_starts.push(self.history.len());
        self.history.push(Message::user(user_msg));

        let mut applied = false;
        let mut tool_calls_made = 0usize;
        let mut final_text = String::new();

        for _ in 0..MAX_ITERATIONS {
            let completion = self.client.complete(&system, &self.history, &defs).await?;
            emit(
                events,
                AgentEvent::Usage {
                    input_tokens: completion.input_tokens,
                    output_tokens: completion.output_tokens,
                },
            );

            // Surface any assistant text the moment we have it.
            if !completion.text.is_empty() {
                emit(events, AgentEvent::AssistantText(completion.text.clone()));
            }

            // Record the assistant turn (text + any tool_use blocks) verbatim so
            // the next request carries a faithful transcript.
            let mut assistant_blocks: Vec<ContentBlock> = Vec::new();
            if !completion.text.is_empty() {
                assistant_blocks.push(ContentBlock::Text(completion.text.clone()));
            }
            for call in &completion.tool_calls {
                assistant_blocks.push(ContentBlock::ToolUse {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    input: call.input.clone(),
                });
            }
            self.history.push(Message {
                role: Role::Assistant,
                content: assistant_blocks,
            });

            // No tool calls → the model is done; return its text.
            if completion.tool_calls.is_empty() {
                final_text = completion.text;
                emit(
                    events,
                    AgentEvent::TurnDone(TurnOutcomeSummary {
                        applied,
                        tool_calls_made,
                        final_text: final_text.clone(),
                    }),
                );
                return Ok(TurnOutcome {
                    applied,
                    final_text,
                    tool_calls_made,
                    stop_reason: StopReason::Completed,
                });
            }

            // Run every requested tool and collect the results into one user
            // message (Converse requires all tool results in a single turn).
            let mut result_blocks: Vec<ContentBlock> = Vec::new();
            for call in &completion.tool_calls {
                tool_calls_made += 1;
                emit(
                    events,
                    AgentEvent::ToolStarted {
                        name: call.name.clone(),
                    },
                );
                let (content, images) = self
                    .run_tool_call(call, approvals, &mut applied, events)
                    .await;
                emit(
                    events,
                    AgentEvent::ToolFinished {
                        name: call.name.clone(),
                        summary: tool_summary(&call.name, &call.input, &content),
                    },
                );
                result_blocks.push(ContentBlock::ToolResult {
                    tool_use_id: call.id.clone(),
                    content,
                    images,
                });
            }
            self.history.push(Message {
                role: Role::User,
                content: result_blocks,
            });

            // Carry any text the model emitted alongside its tool calls so a turn
            // that ends without a trailing text-only completion still has a reply.
            if !completion.text.is_empty() {
                final_text = completion.text;
            }
        }

        // Hit the iteration cap without a clean finish.
        if final_text.is_empty() {
            final_text = "(agent reached its iteration limit without a final answer)".to_string();
        }
        emit(
            events,
            AgentEvent::TurnDone(TurnOutcomeSummary {
                applied,
                tool_calls_made,
                final_text: final_text.clone(),
            }),
        );
        Ok(TurnOutcome {
            applied,
            final_text,
            tool_calls_made,
            stop_reason: StopReason::IterationCap,
        })
    }

    /// Execute one tool call, returning the JSON-stringified result and any
    /// images to feed back to the model. `apply_design` commits are routed
    /// through the apply-gate; every other tool runs directly. Tool errors are
    /// surfaced as a structured `{error: ...}` result (not propagated) so the
    /// model can self-repair.
    async fn run_tool_call(
        &self,
        call: &crate::llm::ToolCall,
        approvals: &mut dyn Approvals,
        applied: &mut bool,
        events: Events<'_>,
    ) -> (String, Vec<ImageData>) {
        let result = if call.name == "apply_design" && wants_commit(&call.input) {
            self.gated_apply(&call.input, approvals, applied, events)
                .await
        } else {
            self.run_tool_blocking(&call.name, call.input.clone()).await
        };

        match result {
            Ok(mut value) => {
                let images = take_images(&mut value);
                (value.to_string(), images)
            }
            Err(e) => (json!({ "error": e.to_string() }).to_string(), Vec::new()),
        }
    }

    /// The apply-gate: dry-run to get the diff, ask for approval, and only then
    /// commit. On rejection nothing is written and the model is told.
    async fn gated_apply(
        &self,
        input: &Value,
        approvals: &mut dyn Approvals,
        applied: &mut bool,
        events: Events<'_>,
    ) -> Result<Value> {
        // 1. Dry-run (commit:false) to get the diff WITHOUT writing.
        let mut dry_input = input.clone();
        dry_input["commit"] = json!(false);
        let dry = self.run_tool_blocking("apply_design", dry_input).await?;

        // If the YAML doesn't even compile, there is nothing to approve — return
        // the diagnostics straight back so the model self-repairs.
        if dry.get("ok").and_then(Value::as_bool) != Some(true) {
            return Ok(dry);
        }

        // 2. Human apply-gate on the dry-run diff (awaits a UI keypress / a
        //    headless answer).
        if !approvals.approve(&dry).await {
            return Ok(json!({
                "ok": true,
                "written": false,
                "rejected": true,
                "note": "user rejected the proposed change; nothing was written",
            }));
        }

        // 3. Approved → commit (writes + snapshots + ERC).
        let mut commit_input = input.clone();
        commit_input["commit"] = json!(true);
        let committed = self.run_tool_blocking("apply_design", commit_input).await?;
        if committed.get("written").and_then(Value::as_bool) == Some(true) {
            *applied = true;
            let errors = committed
                .pointer("/erc/errors")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize;
            let warnings = committed
                .pointer("/erc/warnings")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize;
            emit(events, AgentEvent::Applied { errors, warnings });
        }
        Ok(committed)
    }
}

/// Pull a `_image_path` out of a tool result: load + base64 the PNG, strip the
/// key so the model's text view stays clean. An unreadable file degrades to
/// "no image" rather than failing the tool call.
fn take_images(value: &mut Value) -> Vec<ImageData> {
    use base64::Engine as _;
    let Some(path) = value
        .get(crate::tools::IMAGE_PATH_KEY)
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return Vec::new();
    };
    if let Some(obj) = value.as_object_mut() {
        obj.remove(crate::tools::IMAGE_PATH_KEY);
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

/// A short, human-readable one-liner for a finished tool call, used to label a
/// collapsed tool-call card in the UI. Reads the structured JSON result.
fn tool_summary(name: &str, input: &Value, result_json: &str) -> String {
    let result: Value = serde_json::from_str(result_json).unwrap_or(Value::Null);
    if let Some(err) = result.get("error").and_then(Value::as_str) {
        return format!("error: {err}");
    }
    match name {
        "search_symbols" => {
            let q = input.get("query").and_then(Value::as_str).unwrap_or("");
            let n = result
                .get("hits")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            format!("\"{q}\" → {n} hits")
        }
        "get_symbol_info" => {
            let lib = input.get("lib_id").and_then(Value::as_str).unwrap_or("");
            let n = result
                .get("pins")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
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
                let errors = result
                    .pointer("/erc/errors")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                format!("written (ERC {errors} errors)")
            } else if result.get("rejected").and_then(Value::as_bool) == Some(true) {
                "rejected".to_string()
            } else if result.get("would_write").and_then(Value::as_bool) == Some(true) {
                let added = result
                    .pointer("/diff/added")
                    .and_then(Value::as_array)
                    .map(Vec::len)
                    .unwrap_or(0);
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
        _ => "done".to_string(),
    }
}

/// Whether an `apply_design` input intends to write (`commit: true`).
fn wants_commit(input: &Value) -> bool {
    input.get("commit").and_then(Value::as_bool) == Some(true)
}

/// The instruction `compact` sends as the final user message.
const COMPACT_PROMPT: &str = "Summarize this conversation so far for your own \
future reference: the user's goals, every design decision made, the current \
state of the schematic (components, nets, anything applied), and any open \
issues. Reply with ONLY the summary text — no tool calls.";

/// Patch a ragged history tail left by a cancelled or failed turn so the next
/// Converse request is valid:
///
/// - a trailing assistant message with `tool_use` blocks that never got their
///   results is given synthetic "cancelled" `tool_result`s;
/// - a trailing user message (e.g. tool results whose follow-up completion
///   never ran) is closed with a synthetic assistant note, keeping the
///   user/assistant alternation Converse requires once the next user turn is
///   appended.
fn repair_history(history: &mut Vec<Message>) {
    let Some(last) = history.last() else {
        return;
    };

    if last.role == Role::Assistant {
        let dangling: Vec<String> = last
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        if !dangling.is_empty() {
            let results = dangling
                .into_iter()
                .map(|id| ContentBlock::ToolResult {
                    tool_use_id: id,
                    content: json!({
                        "cancelled": true,
                        "note": "the turn was cancelled before this tool ran",
                    })
                    .to_string(),
                    images: Vec::new(),
                })
                .collect();
            history.push(Message {
                role: Role::User,
                content: results,
            });
        }
    }

    if history.last().map(|m| m.role) == Some(Role::User) {
        history.push(Message::assistant("(turn interrupted)"));
    }
}

/// The first text block of a message, if any (a user turn's prompt lives here).
fn first_text(m: &Message) -> Option<&str> {
    m.content.iter().find_map(|b| match b {
        ContentBlock::Text(t) => Some(t.as_str()),
        _ => None,
    })
}

/// Collapse a prompt to a single trimmed line, truncated for a picker row.
fn preview(text: &str) -> String {
    const MAX: usize = 60;
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() > MAX {
        let mut s: String = one_line.chars().take(MAX - 1).collect();
        s.push('…');
        s
    } else {
        one_line
    }
}

/// Newest-first prompt previews for the turns recorded in `turn_starts`. Each is
/// the first text block of that turn's user message (at `history[start]`),
/// single-lined and truncated. Turns behind a compaction barrier aren't listed —
/// `turn_starts` no longer carries them.
fn turn_previews(history: &[Message], turn_starts: &[usize]) -> Vec<String> {
    turn_starts
        .iter()
        .rev()
        .map(|&start| {
            history
                .get(start)
                .and_then(first_text)
                .map(preview)
                .unwrap_or_else(|| "(prompt)".to_string())
        })
        .collect()
}

/// Pop the `k` most recent turns: drop each turn's start index and truncate the
/// history back to it. Returns how many were actually popped (fewer than `k`
/// once `turn_starts` is exhausted — e.g. at a compaction barrier).
fn pop_n(history: &mut Vec<Message>, turn_starts: &mut Vec<usize>, k: usize) -> usize {
    let mut popped = 0;
    while popped < k {
        match turn_starts.pop() {
            Some(start) => {
                history.truncate(start);
                popped += 1;
            }
            None => break,
        }
    }
    popped
}

/// The system prompt: the circuit-YAML language spec (kernel + sugar), the
/// workflow doctrine, and the real-library guidance from validation.
///
/// Kept as a single embedded string (no design state) — the model pulls the
/// design on demand via `get_design`.
fn system_prompt() -> String {
    SYSTEM_PROMPT.to_string()
}

const SYSTEM_PROMPT: &str = r#"You are an expert KiCAD schematic copilot. You design and edit electronic
schematics by emitting a small declarative YAML language ("circuit-YAML") and
driving a fixed set of tools. You never hand-edit the .kicad_sch directly; the
tools compile your YAML to a real KiCAD schematic.

# circuit-YAML language

A design is ONE YAML document with this shape:

  version: 1                 # required, always 1
  name: my_board             # optional design name
  rails: [3V3, GND, VBUS]    # optional: declare power/ground nets (see Sugar)
  layout:                    # optional 2D placement grid (see Layout)
    - [usb, mcu, headers]    #   row 0, left -> right
    - [~,   power]           #   row 1; ~ is an empty cell
  blocks:                    # required: a partition of all components
    main:                    # block name, lower_snake_case
      components:
        R1: { ... }          # refdes -> component
  nets:                      # optional: per-net class hints (rarely needed)
    I2C1_SDA: { class: signal }

## Components (the kernel)

Each component is keyed by its refdes and has:

  U1:
    part: MCU_ST_STM32H7:STM32H743VITx   # REQUIRED: full KiCAD lib_id "Lib:Name"
    value: 10k                           # optional component value
    footprint: Package_QFP:LQFP-100      # optional
    dnp: true                            # optional do-not-populate flag
    pins:                                # map pin -> net name (or `nc`)
      VDD: 3V3
      VSS: GND
      PA0: USB_DM
      "48": VCAP1                        # pin NUMBER as a quoted key (see below)

- `part:` MUST be a real, fully-qualified lib_id like `Device:R` or
  `MCU_ST_STM32H7:STM32H743VITx`. Find it with `search_symbols` first — never
  guess or invent a lib_id. The five short aliases `R`, `C`, `L`, `D`, `LED`
  expand to `Device:R`/`Device:C`/`Device:L`/`Device:D`/`Device:LED`; everything
  else must be a real `Lib:Name`.
- `pins:` maps a pin KEY to a net name. The key may be the pin's NAME (e.g. `VDD`,
  `PA0`) or, when names are ambiguous or stacked (multiple pins share a name like
  the STM32 `VCAP`/`VSS`), the pin NUMBER as a quoted string (e.g. `"48"`).
  Prefer numbers when a name is not unique. Use `get_symbol_info` to read the
  real pin names/numbers/types for a part.
- The reserved net `nc` (case-insensitive) places a no-connect on a pin. You do
  NOT need to list every pin: any unmentioned pin is auto-no-connected — EXCEPT
  power-INPUT pins, which MUST be connected to a net or compilation fails loudly.
  So always wire VDD/VSS/VDDA/etc.

## Naming rules (hard unless noted)

- refdes: strictly `[A-Z]+[0-9]+` — uppercase letters then digits, e.g. `R1`,
  `U2`, `J1`. Use PLAIN refdes; do NOT use descriptive names like `C_VCAP1` or
  `R_PULLUP` (they are rejected). Just `C1`, `R3`, etc.
- net names: UPPER_SNAKE, no spaces, `/` reserved. (A lowercase letter is only a
  warning, but prefer UPPER_SNAKE.)
- block names: lower_snake_case.
- Every refdes is globally unique across all blocks. Blocks are grouping only —
  no electrical meaning.

## Layout (optional placement grid)

The engine places parts automatically from connectivity — you usually need NO
layout. For a board with a real floorplan, the top-level `layout:` is a 2D grid:

  layout:
    - [usb, mcu, headers]   # row 0: usb left, mcu centre, headers right
    - [~,   power]          # row 1: power below mcu; ~ is an empty cell

- Each cell names a BLOCK or a single refdes; column = left→right, row =
  top→bottom (ordinal — spacing is computed for you). Rows may be ragged.
- Only place the structural anchors (ICs, connectors, modules). Leave caps,
  resistors, crystals OUT — the engine places them next to the part they wire to.
- Omit `layout:` entirely and blocks flow left→right in declaration order.

## Sugar (shorthands the compiler expands)

- `rails: [3V3, GND]` — declares these nets as power/ground rails. Use it so the
  schematic gets proper power symbols.
- `between: [NET_A, NET_B]` — for a 2-pin part, wires its two pins to these nets
  in pin-number order. Replaces an explicit `pins:` map:
      R1: { part: R, value: 10k, between: [VBUS, GND] }
  Note: on POLARIZED parts (D/LED/CP) `between` warns about orientation.
- `decouple: { 100nF: 10, 4.7uF: 2 }` — on an IC, synthesizes that many
  decoupling caps of each value across the IC's power/ground. The caps are
  generated for you; never list them individually.

# Tools and workflow (follow this order)

1. `get_design()` — lift the CURRENT schematic back to circuit-YAML. ALWAYS call
   this first when editing an existing design so you build on it (don't recreate
   from scratch and don't clobber the user's work).
2. `search_symbols(query)` — find the real `Lib:Name` lib_id for any part BEFORE
   you reference it. KiCAD 10 renamed many symbols (e.g.
   `USB_C_Receptacle_USB2.0` is now `USB_C_Receptacle_USB2.0_16P`), so do not
   trust remembered names — search.
3. `get_symbol_info(lib_id)` — read a part's real pin table (number, name,
   electrical type, unit) so you wire the right pins, especially for stacked
   power pins where you must key by number.
4. `validate_design(yaml)` — compile your YAML WITHOUT writing. Read the
   diagnostics and self-repair until it reports `ok: true` and 0 errors.
5. `apply_design(yaml, commit:false)` — preview: returns the structured diff
   (added/removed/changed refdes, net delta) WITHOUT writing. Inspect it.
6. `apply_design(yaml, commit:true)` — propose the WRITE. A human must approve
   the diff before it lands; on approval it writes the .kicad_sch, snapshots the
   prior, and runs ERC, returning the ERC counts. On rejection nothing is written
   — explain or revise.
7. `run_erc()` — re-run KiCAD's Electrical Rules Check on the current schematic.

Two more tools answer questions rather than edit:

- `project_info()` — the project directory, the schematic path your writes go
  to, whether it exists yet, and the undo-snapshot count. Use it when the user
  asks where the file is or whether you can see their project.
- `read_schematic(path)` — lift ANY .kicad_sch on disk (absolute, ~, or
  project-relative path) to circuit-YAML, read-only. Use it when the user
  points you at a schematic by path.

Doctrine: search before you reference a part; read pins with get_symbol_info;
validate before you apply; preview (commit:false) before you commit (commit:true).
Aim for ERC-clean designs. When you are finished, reply with a short plain-text
summary of what you did — no tool call.
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn auto_approve_yes_and_no() {
        let diff = json!({ "diff": { "added": ["R1"] } });
        assert!(AutoApprove::yes().approve(&diff).await);
        assert!(!AutoApprove::no().approve(&diff).await);
    }

    #[test]
    fn tool_summary_reads_structured_results() {
        let s = tool_summary(
            "search_symbols",
            &json!({ "query": "STM32" }),
            &json!({ "hits": [1, 2, 3] }).to_string(),
        );
        assert_eq!(s, "\"STM32\" → 3 hits");

        let s = tool_summary(
            "apply_design",
            &json!({}),
            &json!({ "written": true, "erc": { "errors": 0 } }).to_string(),
        );
        assert!(s.contains("written"), "got: {s}");

        let s = tool_summary(
            "get_design",
            &json!({}),
            &json!({ "error": "boom" }).to_string(),
        );
        assert_eq!(s, "error: boom");
    }

    #[test]
    fn repair_history_closes_a_dangling_tool_use() {
        let mut history = vec![
            Message::user("add a resistor"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text("searching".into()),
                    ContentBlock::ToolUse {
                        id: "tu_9".into(),
                        name: "search_symbols".into(),
                        input: json!({ "query": "R" }),
                    },
                ],
            },
        ];
        repair_history(&mut history);
        // Synthetic result for tu_9, then a closing assistant note.
        assert_eq!(history.len(), 4, "{history:#?}");
        match &history[2].content[0] {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } => {
                assert_eq!(tool_use_id, "tu_9");
                assert!(content.contains("cancelled"));
            }
            other => panic!("expected a tool result, got {other:?}"),
        }
        assert_eq!(history[3].role, Role::Assistant);
    }

    #[test]
    fn repair_history_closes_a_trailing_user_message() {
        let mut history = vec![Message::user("hello")];
        repair_history(&mut history);
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].role, Role::Assistant);
    }

    #[test]
    fn repair_history_leaves_clean_histories_alone() {
        let mut empty: Vec<Message> = Vec::new();
        repair_history(&mut empty);
        assert!(empty.is_empty());

        let mut clean = vec![Message::user("hi"), Message::assistant("done")];
        repair_history(&mut clean);
        assert_eq!(clean.len(), 2, "a finished exchange needs no repair");
    }

    #[test]
    fn turn_previews_are_newest_first_and_single_lined() {
        // Two turns; turn_starts points at each user message's index.
        let history = vec![
            Message::user("  first   prompt  "),
            Message::assistant("ok"),
            Message::user("second prompt"),
            Message::assistant("done"),
        ];
        let starts = vec![0, 2];
        let p = turn_previews(&history, &starts);
        assert_eq!(p, vec!["second prompt".to_string(), "first prompt".to_string()]);
    }

    #[test]
    fn preview_truncates_long_prompts_with_an_ellipsis() {
        let p = preview(&"x".repeat(100));
        assert_eq!(p.chars().count(), 60, "capped at MAX");
        assert!(p.ends_with('…'));
        assert_eq!(preview("short"), "short");
    }

    #[test]
    fn pop_n_truncates_history_and_reports_the_real_count() {
        let mut history = vec![
            Message::user("t1"),
            Message::assistant("a1"),
            Message::user("t2"),
            Message::assistant("a2"),
            Message::user("t3"),
            Message::assistant("a3"),
        ];
        let mut starts = vec![0, 2, 4];

        assert_eq!(pop_n(&mut history, &mut starts, 2), 2, "popped two");
        assert_eq!(starts, vec![0], "only the oldest turn remains");
        assert_eq!(history.len(), 2, "history truncated to t1's exchange");

        // Asking for more than is left pops the rest and reports the true count.
        assert_eq!(pop_n(&mut history, &mut starts, 5), 1);
        assert!(history.is_empty() && starts.is_empty());
        assert_eq!(pop_n(&mut history, &mut starts, 1), 0, "nothing left to pop");
    }

    #[test]
    fn wants_commit_detects_true_only() {
        assert!(wants_commit(&json!({ "yaml": "x", "commit": true })));
        assert!(!wants_commit(&json!({ "yaml": "x", "commit": false })));
        assert!(!wants_commit(&json!({ "yaml": "x" })));
    }

    #[test]
    fn system_prompt_covers_kernel_sugar_and_workflow() {
        let p = system_prompt();
        // Kernel + naming rules.
        assert!(p.contains("part:"));
        assert!(p.contains("[A-Z]+[0-9]+"));
        assert!(p.contains("power-INPUT"));
        // Sugar forms.
        assert!(p.contains("rails:"));
        assert!(p.contains("between:"));
        assert!(p.contains("decouple:"));
        // Workflow doctrine + real-lib guidance.
        assert!(p.contains("search_symbols"));
        assert!(p.contains("validate_design"));
        assert!(p.contains("apply_design"));
        assert!(p.contains("commit:false"));
        assert!(p.contains("C_VCAP1")); // plain-refdes guidance
    }
}
