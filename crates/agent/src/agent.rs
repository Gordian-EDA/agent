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
//! The conversation lives in `Agent::history` and is carried across turns, so
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

/// Safety cap on LLM round-trips per turn. Generous enough for the longest legitimate
/// flow, bounded so a misbehaving model can't loop forever. 12 was too low; 24 carried
/// ~50-part boards but BOTH engines' densest cases overran it — independently discovered
/// from each side and raised to 40:
///   - SCHEMATIC: the biggest boards (STM32H7+DDR+Ethernet; an industrial I/O module with
///     8 optos + 4 relays + RS485/CAN — 60-80 parts) exceed the model's max OUTPUT tokens
///     on a single create_design, so it falls back to incremental edit_design per block
///     (6-9 blocks + per-part search + validate) and hit 24 with NO schematic written
///     (observed: 55 tool calls, stop=IterationCap).
///   - PCB: a dense board (100-ball BGA + decoupling + connectors, ~29 parts) spends ~9
///     round-trips on footprint search/info, several on build, then multiple
///     place/route/triage cycles, and hit 24 mid-placement-refinement with NO board exported.
///
/// 40 lets the largest boards commit/export incrementally; simple boards still finish in a
/// handful of round-trips, so the extra ceiling only costs tokens on boards that need it.
const MAX_ITERATIONS: usize = 40;

/// How many times a turn that ends WITHOUT a committed design (the model
/// researched or drafted but never called `apply_design(commit:true)`) is
/// re-prompted to finish and commit before we give up. Bounded so a model that
/// genuinely can't finish doesn't loop forever.
const MAX_COMMIT_NUDGES: usize = 2;

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
    /// An independent design-review pass over the committed netlist completed (from
    /// [`Agent::run_turn_reviewed`]). `round` 0 is the first review; later rounds follow fix turns.
    Reviewed {
        round: usize,
        score: f64,
        defects: Vec<String>,
    },
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
    /// The loop hit `MAX_ITERATIONS` before the model finished; the turn was
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
/// blocking thread pool (see `Agent::run_tool_blocking`) — a compile, render,
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
    /// returns a final text with no pending tool calls, or `MAX_ITERATIONS` is
    /// reached.
    ///
    /// The turn appends to the agent's persistent history, so later turns see
    /// the full conversation. A turn that was cancelled mid-flight may leave a
    /// ragged tail (a dangling `tool_use`); `repair_history` patches that
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
        // Whether the model ever DISPATCHED an `apply_design(commit:true)` this
        // turn. Distinguishes the genuine stall ("researched/drafted but never
        // tried to commit") from a deliberate human rejection (which DID attempt
        // a commit) — we only nudge the former.
        let mut commit_attempted = false;
        // Whether the model did SCHEMATIC-authoring work this turn (researched
        // symbols or drafted a design). The same loop also drives the PCB board
        // flow (`create_board`/`place_board`/`route_board`/`export_board`), which
        // legitimately never calls `apply_design` — so the "didn't commit" nudge
        // must only fire on a stalled SCHEMATIC turn, not a board turn.
        let mut did_schematic_work = false;
        // Bounded re-prompts that push a stalled model past a premature stop.
        let mut nudges_left = MAX_COMMIT_NUDGES;
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

            // No tool calls → the model wants to stop.
            if completion.tool_calls.is_empty() {
                final_text = completion.text;

                // Catch the premature stop: the model did schematic-authoring
                // work (researched with `search_symbols`, or drafted with
                // `create_design`) but ended the turn WITHOUT ever attempting
                // `apply_design(commit:true)`, so nothing ships. Re-prompt it to
                // finish and commit (bounded), rather than silently returning an
                // empty design. Excluded by design: a deliberate human rejection
                // (DID attempt a commit), a pure-text stop / plain answer (no
                // schematic work), and a PCB board-flow turn (never uses
                // `apply_design`).
                if did_schematic_work && !applied && !commit_attempted && nudges_left > 0 {
                    nudges_left -= 1;
                    self.history.push(Message::user(
                        "Your turn ended without a committed design — nothing was \
                         written. You MUST finish the schematic now: call \
                         `create_design`/`edit_design` to author the full design, \
                         then `apply_design(commit:true)` to commit it. Do this now \
                         before ending your turn.",
                    ));
                    continue;
                }

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
                if call.name == "apply_design" && wants_commit(&call.input) {
                    commit_attempted = true;
                }
                if is_schematic_authoring_tool(&call.name) {
                    did_schematic_work = true;
                }
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

    /// Run a turn, then INDEPENDENTLY review the committed netlist for electrical-correctness
    /// faults (pin-function mis-wires, voltage-domain part errors, missing-essential parts,
    /// topology errors — the class ERC and the layout critic both miss) and feed any
    /// HIGH-confidence critical/major defects back as a fix turn, re-reviewing up to `max_fix`
    /// rounds. The reviewer is a fresh, history-free [`crate::review::review_netlist`] call
    /// (unbiased — the generating model rationalises its own slips). `intent` is the design goal,
    /// for the reviewer's context. Emits [`AgentEvent::Reviewed`] per round; returns the final
    /// turn's outcome. A lift/review failure ends the loop gracefully (the design stands).
    pub async fn run_turn_reviewed(
        &mut self,
        user_msg: &str,
        intent: &str,
        approvals: &mut dyn Approvals,
        events: Events<'_>,
        max_fix: usize,
    ) -> Result<TurnOutcome> {
        let mut outcome = self.run_turn(user_msg, approvals, events).await?;
        for round in 0..=max_fix {
            let sch = self.ctx.sch_path();
            if !sch.exists() {
                break;
            }
            let Ok(netlist) = sch_layout::lift::lift(self.ctx.env(), sch) else {
                break;
            };
            let (score, mut defects) =
                match crate::review::review_netlist(self.client.as_ref(), intent, &netlist).await {
                    Ok(r) => r,
                    Err(_) => break,
                };
            // Deterministic quantitative ERC (feedback-divider ratios, LED current) — the exact-math
            // layer UNDER the LLM ensemble, where the netlist makes the numbers unambiguous and the
            // reviewer is weakest. Unioned by refdes so a fault both layers find isn't reported twice.
            if let Some(design) = circuit_lang::compile(&netlist, self.ctx.provider()).design {
                for d in circuit_lang::erc::erc_checks(&design) {
                    if !defects.iter().any(|e| crate::review::same_defect(e, &d)) {
                        defects.push(d);
                    }
                }
            }
            emit(events, AgentEvent::Reviewed { round, score, defects: defects.clone() });
            if defects.is_empty() || round == max_fix {
                break;
            }
            let fix = format!(
                "An INDEPENDENT design review of the netlist you just committed found these \
                 high-confidence functional defects (they pass ERC but are electrically wrong):\n{}\n\n\
                 Fix each one — search for a correct part or value if needed (e.g. a 3.3V-capable \
                 transceiver, the right MCU function pin) — and re-commit the corrected design.",
                defects.join("\n")
            );
            outcome = self.run_turn(&fix, approvals, events).await?;
        }
        Ok(outcome)
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
        } else if call.name == "review_design" {
            // Independent review needs the LlmClient + async, so it can't ride the sync tool
            // dispatch — handle it here like the apply-gate.
            self.review_design(&call.input).await
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

    /// The `review_design` tool: an INDEPENDENT electrical-correctness review of the current design,
    /// called by the agent in-flow (transparent — it shows up as a normal tool card). Takes the
    /// current design YAML (the same draft-or-lifted source as `get_design`, so the agent can review
    /// BEFORE committing — one apply-gate), runs a FRESH diverse-lens LLM review
    /// ([`crate::review::review_netlist`] — no conversation history, so it doesn't rationalise the
    /// agent's own choices) UNIONED with the deterministic exact-math ERC, and returns the score +
    /// high-confidence functional defects for the agent to fix and re-check. Async + needs the
    /// LlmClient, so it's handled here rather than in the sync tool dispatch.
    async fn review_design(&self, input: &Value) -> Result<Value> {
        let intent = input.get("intent").and_then(Value::as_str).unwrap_or("");
        let dv = self.run_tool_blocking("get_design", json!({})).await?;
        let netlist = dv
            .get("yaml")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if netlist.trim().is_empty() {
            return Ok(json!({
                "error": "no design to review yet — build one with create_design/edit_design (or apply_design) first",
            }));
        }
        let (score, mut defects) =
            crate::review::review_netlist(self.client.as_ref(), intent, &netlist).await?;
        // Deterministic exact-math ERC under the LLM ensemble (feedback-divider ratios, LED current,
        // dangling/crystal/polarity) — unioned by refdes so a fault both layers find isn't doubled.
        if let Some(design) = circuit_lang::compile(&netlist, self.ctx.provider()).design {
            for d in circuit_lang::erc::erc_checks(&design) {
                if !defects.iter().any(|e| crate::review::same_defect(e, &d)) {
                    defects.push(d);
                }
            }
        }
        let note = if defects.is_empty() {
            "no high-confidence functional defects — the design looks electrically sound"
        } else {
            "high-confidence functional defects found (they pass ERC but are electrically wrong); \
             fix each with edit_design and re-check"
        };
        Ok(json!({ "score": score, "defects": defects, "note": note }))
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
        "review_design" => {
            let score = result.get("score").and_then(Value::as_f64).unwrap_or(0.0);
            let n = result
                .get("defects")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            if n == 0 {
                format!("score {score:.0}/10 — clean")
            } else {
                format!("score {score:.0}/10 — {n} defect(s) to fix")
            }
        }
        _ => "done".to_string(),
    }
}

/// Whether an `apply_design` input intends to write (`commit: true`).
fn wants_commit(input: &Value) -> bool {
    input.get("commit").and_then(Value::as_bool) == Some(true)
}

/// Whether a tool call is SCHEMATIC-authoring/research work — the kind of turn
/// whose deliverable is a committed design. Used to scope the "ended without a
/// committed design" nudge to schematic turns only, so the PCB board flow
/// (`create_board`/`place_board`/`route_board`/`export_board`, which never calls
/// `apply_design`) is never spuriously nudged.
fn is_schematic_authoring_tool(name: &str) -> bool {
    matches!(
        name,
        "search_symbols" | "get_symbol_info" | "create_design" | "edit_design" | "apply_design"
    )
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
- Every refdes is globally unique across all blocks. Blocks carry no electrical
  meaning, but they ARE the floorplan: the engine lays each block out as one MODULE
  and flows the blocks left→right in declaration order. So PARTITION the design into a
  FEW COARSE functional modules — typically just: power-entry (input connector +
  regulator + their bulk/bypass caps), the MAIN IC + ALL its local support, and one or
  two I/O groups. Declare them in signal-flow order (input/power first, processing next,
  outputs/peripherals last).
  SIZE each block at roughly 6-10 parts, and SCALE THE BLOCK COUNT with the design's size:
  a ~15-part board → 2-3 blocks; a ~30-part board → 4-5 blocks; a dense ~50-part board →
  6-8 blocks. Each block becomes its OWN sheet, so a block much over ~12 parts SPRAWLS on
  its sheet (the #1 dense-board defect) — split it further. And a block under ~5 parts is a
  SPARSE little sheet that reads WORSE (label-on-body clutter, no signal flow) — merge it.
  So neither extreme: not one flat `main` block (grid-packs everything, sprawls), nor a
  swarm of 2-4-part fragments. Aim for the 6-10-part sweet spot and add blocks as the design grows.
- SET A CONCISE `value` ON EVERY COMPONENT (≤ ~12 chars: e.g. `USB-C`, `BOOT`, `SWD`, `STM32F103`,
  `24LC256`). With no value the engine renders the full part NAME (`USB_C_Receptacle_USB2.0_16P`,
  `Conn_01x03`) as the label — a long string that OVERLAPS the symbol's pins/body and reads as
  clutter (a recurring per-sheet readability defect). Passives already use values (`10k`, `100nF`);
  give connectors, jumpers, headers, sockets, and ICs a short value too.
  HOW to keep blocks ~6-10 parts:
  • A small MCU's crystal + decoupling + reset + boot all fold INTO the MCU block. But on a
    DENSE board where that would exceed ~12 parts, split support out (e.g. a `clock_reset`
    block, or keep decoupling with the MCU and put the crystal/reset elsewhere).
  • Group small peripherals into I/O blocks — but on a dense board with many peripherals,
    use a FEW I/O blocks (e.g. one per bus or per 2-3 peripherals), not one giant `io` block.
  • A lone crystal/jumper/LED, or a 2-pin power/signal connector, folds into a neighbour.
  • BUT a MULTI-SIGNAL BREAKOUT HEADER (SWD, JTAG, GPIO, debug — a connector breaking out many
    distinct signals) gets its OWN block. A connector's pinout reads cleanly alone, but two breakout
    headers (or a header + status LEDs) crammed on one sheet collide — overlapping port labels, the #1
    io-sheet defect. One breakout header per block; don't lump SWD + GPIO + LEDs into a single `io`.

## Layout (placement is automatic — blocks are your floorplan)

The engine places parts automatically from connectivity, and AUTO-RECOGNIZES common
idioms from your pin connections — a crystal with its two load caps next to the
oscillator pins, a decoupling-cap bank along the IC's power rail. You do nothing
special: wire the netlist normally (crystal between two osc nets, caps between V+ and
GND). `apply_design` returns `detected_idioms` so you can confirm what was recognized.

Your main floorplan control is the BLOCK partition itself: the engine lays each block
out as a module and flows the blocks LEFT→RIGHT in declaration order. So declaring your
blocks in signal-flow order (power/input → processing → outputs) IS the floorplan — no
explicit grid needed. Keep TIGHTLY-COUPLED blocks adjacent in the declaration order so
their interconnect stays short (e.g. put an MCU between the sensor it reads and the LED
it drives, not with another block in between).

For fine control WITHIN a block, that block may carry its own 2-D `layout:` grid (a
`layout:` key INSIDE the block, NOT at the top level — top-level `layout:` is rejected):

  blocks:
    mcu:
      layout:               # rows of cells; each cell is a refdes or ~ (empty)
        - [U1, J1]
        - [U1, C1]
      components: { ... }

- Cells name a refdes; column = left→right, row = top→bottom (ordinal). Place only the
  structural anchors (ICs, connectors); leave caps/resistors/crystals OUT — the engine
  clusters them next to the part they wire to. Most blocks need no grid at all.

## Sugar (shorthands the compiler expands)

- Power & ground symbols are ordinary COMPONENTS — give a `power:Lib` part a
  single pin tied to the net it drives, and every net touched by such a symbol
  becomes a power/ground rail (the engine draws the symbols and rail wiring):
      GND1: { part: power:GND, pins: { 1: GND } }
      VCC1: { part: power:VCC, pins: { 1: 3V3 } }
  ONE symbol for a net draws a single shared rail. TWO OR MORE symbols for the SAME
  net (GND1, GND2, GND3 …) tell the engine to DISTRIBUTE that net as LOCAL ground/
  supply symbols — one little triangle dropped right at each pin — instead of one
  sheet-spanning rail. This is how professionals draw a dense board: a long GND rail
  with a dozen risers across the page reads as a tangle, so on any board with many
  ground/supply pins (an MCU, an FPGA, a multi-IC board) declare SEVERAL GND (and V+)
  symbols so the grounds stay local and the sheet stays legible. Small boards (a
  divider, a single regulator) want just one symbol per rail. KiCAD's `power:` library
  is rich — `power:GND`, `power:VCC`, `power:+3V3`, `power:+5V`, `power:VBUS`, etc. A
  shared rail is implied by fan-out from a single symbol; several symbols distribute it.
- `between: [NET_A, NET_B]` — for a SYMMETRIC 2-pin part (R, C, L, fuse), wires
  its two pins to these nets in pin-number order. Replaces an explicit `pins:` map:
      R1: { part: R, value: 10k, between: [VBUS, GND] }
- `positive: NET` / `negative: NET` — for a POLARIZED 2-pin part (D, LED, CP),
  wires the anode and cathode. The compiler maps them to the right pins for you:
      D1: { part: LED, positive: VBUS, negative: STATUS }   # anode VBUS, cathode STATUS
  Using `between` on a polarized part (or `positive`/`negative` on a symmetric
  one) is a hard error — pick the right one. Multi-pin parts use `pins:`.
- `decouple: { 100nF: 10, 4.7uF: 2 }` — on an IC, synthesizes that many
  decoupling caps of each value across the IC's power/ground. The caps are
  generated for you; never list them individually.
- Exposing a signal as an I/O PORT: mark the net with a `label:global` component — a
  single-pin label whose pin ties to the net, exactly like a `power:GND` symbol marks
  a ground:
      VOUT_PORT: { part: label:global, pins: { 1: VOUT } }
  The engine draws that net with a global-label port pennant at the sheet edge. Use
  this for any board I/O — especially an output that ALSO connects internally (e.g. a
  gain stage's `VOUT`, a logic `OUT`), which a bare name can't auto-detect. (A signal
  net that taps only to the edge is auto-labelled, so a simple VIN/VOUT often needs no
  marker.) Do NOT add a single-pin test-point or `Conn_01x01` connector just to "bring
  a net out" — that clutters the sheet. Reserve connectors for REAL physical headers.

# Tools and workflow (follow this order)

0. DECIDE the path. For a NEW design on an empty project: author your FULL
   circuit-YAML and call `create_design(yaml)` to write the working draft (then
   refine with `edit_design`). For EDITING an existing schematic: call
   `get_design()` first to lift it so you build on it (don't clobber the user's
   work). Researching parts is NOT the deliverable — you are NOT done until you have
   authored a complete design and committed it with `apply_design(commit:true)`. Do
   not stop after only searching/reading symbols.
1. `get_design()` — lift the CURRENT schematic back to circuit-YAML (EDIT path only;
   on an empty project it returns nothing — go straight to `create_design`).
2. `search_symbols(query)` — find the real `Lib:Name` lib_id for any part BEFORE
   you reference it. KiCAD 10 renamed many symbols (e.g.
   `USB_C_Receptacle_USB2.0` is now `USB_C_Receptacle_USB2.0_16P`), so do not
   trust remembered names — search.
3. `get_symbol_info(lib_id)` — read a part's real pin table (number, name,
   electrical type, unit) so you wire the right pins, especially for stacked
   power pins where you must key by number.
4. `validate_design(yaml)` — compile your YAML WITHOUT writing. Read the
   diagnostics and self-repair until it reports `ok: true` and 0 errors.
5. `review_design(intent)` — once the design is COMPLETE, get an INDEPENDENT
   electrical-correctness review: a FRESH reviewer (no memory of your work, so it
   won't rationalise your choices) plus a deterministic exact-math ERC flag
   FUNCTIONAL faults that pass ERC but are electrically wrong (pin-function
   mis-wires, a part on the wrong voltage rail, a feedback divider set for the wrong
   output, reversed polarity, missing essentials). Fix any high-confidence defects
   with edit_design, then re-review. Do this BEFORE you commit.
6. `apply_design(yaml, commit:false)` — preview: returns the structured diff
   (added/removed/changed refdes, net delta) WITHOUT writing. Inspect it.
7. `apply_design(yaml, commit:true)` — propose the WRITE. A human must approve
   the diff before it lands; on approval it writes the .kicad_sch, snapshots the
   prior, and runs ERC, returning the ERC counts. On rejection nothing is written
   — explain or revise.
8. `run_erc()` — re-run KiCAD's Electrical Rules Check on the current schematic.

Two more tools answer questions rather than edit:

- `project_info()` — the project directory, the schematic path your writes go
  to, whether it exists yet, and the undo-snapshot count. Use it when the user
  asks where the file is or whether you can see their project.
- `read_schematic(path)` — lift ANY .kicad_sch on disk (absolute, ~, or
  project-relative path) to circuit-YAML, read-only. Use it when the user
  points you at a schematic by path.

Doctrine: search before you reference a part; read pins with get_symbol_info;
validate before you apply; review_design before you commit (it catches FUNCTIONAL
faults ERC can't see); preview (commit:false) before you commit (commit:true).
Aim for designs that are ERC-clean AND electrically correct. You are only FINISHED
once `apply_design(commit:true)` has COMMITTED the design (it returns the ERC counts);
a turn that ends after only searching/validating with nothing committed is a FAILURE.
Once committed, reply with a short plain-text summary of what you did — no tool call.

# PCB layout & routing (the board side)

When the user wants a physical board — placement, routing, a `.kicad_pcb` — you
drive a SEPARATE set of tools over a board "draft" (the PCB analog of the
schematic). The routing/placement engine is deterministic geometry; YOUR job is
the floorplan, the constraints, and triaging failures. You NEVER emit trace
coordinates — copper comes only from the engine.

**The board is DERIVED from the schematic — you never re-type parts or nets.** Once a
schematic is committed (you drew it with `create_design` → `apply_design`, or the user
supplied a `.kicad_sch`), `derive_board` reuses its parts and netlist for the board. So if
the user asks for a board and no schematic exists yet, draw and commit one FIRST
(`create_design` → `apply_design`), THEN run the board flow below. Your board-side job is
just choosing footprints and driving the floorplan/routing — the parts and pad-nets come
from the schematic.

## Board flow (follow this order)

1. `search_footprints(query)` — find the real footprint `Lib:Name` for each part
   (e.g. `Resistor_SMD:R_0603_1608Metric`). NEVER guess a footprint lib_id —
   search for it, exactly like symbols.
2. `get_footprint_info(lib_id)` — read the pad numbers / courtyard / bbox to confirm
   the footprint fits, then `assign_footprints({assignments: {refdes: lib_id}})` to
   record each part's footprint (board-side; the schematic stays footprint-agnostic).
   An unknown lib_id comes back with suggestions — fix it and re-assign.
3. `derive_board({bounds, rules?, outline?})` — build the board draft from the committed
   schematic + the footprint map. The parts and pad-nets come from the netlist; you supply
   ONLY the outline bounds (mm) and optional rules — never re-type parts. A part with no
   footprint yet comes back as `needs_footprints` (assign it, then retry); a footprint
   missing a pad the schematic nets comes back as `wrong_footprints` (wrong footprint for
   that part — pick one that fits, re-assign, retry).
   START WITH GENEROUS BOUNDS (roughly 2× the summed part area, square-ish). The
   export tightens the final outline to the copper + 1mm, so a roomy routing area is
   FREE in the finished board but gives the placer/router the slack they need — a
   hand-packed tight board is the #1 cause of an illegal placement you then waste the
   turn fighting. You can always shrink later; starting tight only hurts.
   USE THE ENGINE'S FEATURES — they are deterministic and DRC-checked, so reach for
   them instead of hand-workarounds or telling the user to finish in KiCAD:
   - `rules.pours: [{net, layer}]` — a copper POUR the engine fills + anti-pads for
     you (a 2-layer ground plane, an RF/HF return, shielding). When the user asks for
     a ground plane/pour, DECLARE IT HERE; never route top-only and punt the zone to
     the user.
   - `rules.layers: 4|6|8` — adds the two CENTRED inner GND/VCC PLANES automatically
     (dense power pins), leaving the other inner layers as signal (8-layer → 6 signal).
     NOTE: more layers add capacity but the greedy router does not yet aggressively
     exploit inner SIGNAL layers, so going 4→6→8 may not route strictly more on a given
     board — pick the layer count your fab/impedance needs, not as a routing-density dial.
   - `rules.net_widths: {net: mm}` — fat power / thin signal.
   - `rules.via_diameter`/`rules.via_drill` — for a dense BGA/QFP that leaves balls
     unrouted, a SMALLER standard via (`0.5`/`0.3`, vs the `0.6`/`0.3` default) is the
     reliable lever: it drops between fine-pitch balls a 0.6 via can't, routing more.
     Do NOT instead reach for a finer `rules.clearance` — verified non-monotonic, it
     often routes FEWER (finer grid → worse greedy contention); reserve sub-0.15mm
     clearance for genuinely sub-0.5mm pitch where a trace can't otherwise fit at all.
   - `outline: [[x,y],...]` — a custom board shape (circle/hex/any); bounds still
     bounds it. Placement, routing, and pours all respect the polygon.
4. `set_placement_hints({groups})` — ENCOURAGED before placing: translate circuit
   intent into floorplan groups (`{name, members, region?, edge?}`) — decoupling
   caps hugging their IC, connectors on an `edge`, a sub-circuit in a `region`.
   Hints only IMPROVE placement; they never gate it. Good placement dominates
   routing success, so spend effort here.
5. `place_board()` — the deterministic legalizer snaps parts to a legal, in-bounds
   floorplan honoring your hints and any locks. Returns each part's position and
   whether the placement is `legal`. For an illegal (too-tight) placement, the
   FIRST and cheapest fix is to ENLARGE BOUNDS (`resize_board` to a bigger
   `bounds`, keeping the parts) — the export auto-tightens the outline to the copper + 1mm anyway, so
   roomy bounds cost nothing in the finished board and give the legalizer slack.
   Do NOT try to resolve overlaps by hand-`move_part`ing parts around: each
   move_part LOCKS that part, and a pile of locks over-constrains the legalizer so
   it can't separate them (you'll fight your own locks forever). Trust the
   legalizer — give it room + good hints and let it place. In particular do NOT lock
   connectors/headers: they AUTO-seek their nearest board edge, and locking one (e.g.
   via move_part) pins it wherever you put it — usually the interior — DEFEATING the
   edge-seek and stranding it mid-board. Reserve move_part/locks for the rare part
   whose exact interior spot truly matters, not for connectors or routine placement.
6. `render_board()` — LOOK at the board. This is your eyes: call it after
   place_board to see the floorplan and after route_board to see the copper
   (top = red, bottom = blue, failed nets = orange crosses). Critique it against a
   manufacturability bar before and after routing.
7. `route_board()` — the engine routes. Returns the router used, per-net `failed`
   list with a `reason`, `metrics`, and a `lint_summary`. Needs a placement first.
8. Triage loop — if `failed` is non-empty, read the reasons and the congestion
   hotspots, apply ONE lever (below), then re-place (if placement was cleared)
   and re-route. BOUND IT: give a stubborn net about 3 triage attempts, and prefer
   a `set_placement_hints` re-floorplan over many one-at-a-time `move_part` nudges
   (re-clustering beats hand-walking a part across the board). If a few nets stay
   walled-in on a genuinely tight/enclosed board after that, STOP — an honestly
   unrouted net is an ACCEPTABLE result, not something to keep grinding. Do NOT
   spend the whole turn (or your iteration budget) chasing the last net.
9. `export_board({path?})` — once the board is placed and routed AS FAR AS IT GOES.
   A board with a FEW honest unrouted nets (listed in `failed`) is a useful,
   shippable deliverable: EXPORT it and report those nets to the user — an
   UNEXPORTED board helps no one, so never let perfectionism on one net cost you the
   whole board. The only hard requirement is never export a STALE route (re-route
   after any change). Writes the `.kicad_pcb` and runs DRC (KiCAD ≥ 8).

## Failure-provenance cheat sheet (read every `reason`)

Each failed net carries a `reason` prefixed by the stage that gave up:

- `global:` — no mesh path, or capacity is infeasible. The cells can't carry the
  demand: a PLACEMENT or KEEPOUT problem. Spread parts out or remove a keepout.
- `assign:` — a boundary slot overflowed (more crossings than a shared cell edge
  can take). Local crowding at one boundary — relieve it by nudging a part or
  widening that channel.
- `cell N:` — local congestion inside one mesh cell N (a dense pin field). Move a
  part out of that cell or open spacing there.
- `finisher: no path` (or a "no grid path … congestion or enclosure" message) —
  the board is genuinely tight or a net is walled in. Move the connected parts
  apart, widen spacing, or relax/remove the blocking rule or keepout.
- A "naive fallback" note means the detailed router could not improve on the
  always-correct grid router, so the simpler result was kept — not itself a fault.

**Fine-pitch escape limit (a FAB reality, not a bug to grind):** when the failing
pins sit on a ≤0.8mm-pitch part (a fine QFN/BGA) and one or two placement/spacing
triage attempts don't clear them, STOP. Those dense/inner SIGNAL pins genuinely
cannot escape with standard through-vias — they need HDI microvias / via-in-pad, a
fab capability the engine does not emit. (Power/ground pins on such parts already
auto-fan-out to the inner planes; this caveat is about signals.) Accept the leftover
nets as honestly unrouted and report them, or tell the user a coarser-pitch part
would route fully — do NOT re-place repeatedly chasing a physically unroutable net.

`route_board` also returns `congestion.hotspots` (hot mesh edges, with usage vs
capacity and the loads) on a failed route — use them to pick WHICH part to move or
WHICH channel to open.

## Triage levers (in preference order)

0. **Wide power nets failing / signals crowding one layer → go 4-layer FIRST.**
   When `route_board` fails a high-fanout power net (VCC/VIN/VOUT) or its output
   says signals are crowding a single layer (e.g. a bottom GND pour leaves only the
   top for signals), the decisive lever is `derive_board(..., rules:{layers:4})` and
   re-place — the high-fanout power nets become inner PLANES (connected by vias),
   freeing both outer layers for signals. ACT on this immediately; do NOT spend
   several re-place/re-hint rounds fighting wide traces on 2 layers first. If
   route_board itself recommends more layers, that recommendation is the move.
1. `set_placement_hints` — re-floorplan via groups/regions/edges (the biggest
   lever; placement dominates).
2. `move_part({reference, x, y, rotation?})` — nudge ONE part to a position you
   reasoned from the render and the place_board positions. The engine legalizes
   around the lock on the next place.
3. `unlock_part({reference})` — release a lock you no longer want pinned.
4. `set_constraints({rules?, keepouts?})` — relax a rule (clearance, trace width)
   or replace a blocking keepout with a gapped one. `net_classes` are reserved but
   NOT yet honored — they are rejected.
5. Re-`place_board` (whenever a change cleared the placement), then re-`route_board`.

## Hard rules (non-negotiable)

- NEVER guess a footprint lib_id — `search_footprints` for it, every time.
- NEVER invent trace coordinates. Copper comes only from `route_board`.
- `move_part` positions must be REASONED from `render_board` + the `place_board`
  positions — never a blind guess. It is a deliberate nudge, not a coordinate dump.
- After ANY constraint or part change, re-run `place_board` (when the placement was
  cleared) and then `route_board` — a stale route is invalid and must not be
  exported.
- If `route_board` returns `lint_summary` non-zero or `engine_bug: true`, that is
  an ENGINE bug that escaped the oracles — NOT a board you can triage. Report it to
  the user VERBATIM and do not try to work around it. (Honest `failed` nets with an
  empty `lint_summary` are normal — triage those.)

Doctrine (board side): search footprints before you reference them; floorplan with
hints; place, then LOOK (render); route; read every failure `reason` and triage
the cheapest lever; re-place/re-route after each change; export only a clean,
routed board. When finished, reply with a short plain-text summary — no tool call.
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
    fn schematic_authoring_tools_scope_the_commit_nudge() {
        // Schematic research/authoring → these turns' deliverable is a commit.
        for t in [
            "search_symbols",
            "get_symbol_info",
            "create_design",
            "edit_design",
            "apply_design",
        ] {
            assert!(is_schematic_authoring_tool(t), "{t} is schematic work");
        }
        // PCB board-flow tools never call `apply_design`, so they must NOT arm
        // the "didn't commit a design" nudge (else a clean board turn re-prompts
        // forever — the pcb_gate regression).
        for t in [
            "create_board",
            "place_board",
            "route_board",
            "export_board",
            "set_constraints",
            "move_part",
            "render_board",
        ] {
            assert!(!is_schematic_authoring_tool(t), "{t} is board work");
        }
    }

    #[test]
    fn system_prompt_covers_kernel_sugar_and_workflow() {
        let p = system_prompt();
        // Kernel + naming rules.
        assert!(p.contains("part:"));
        assert!(p.contains("[A-Z]+[0-9]+"));
        assert!(p.contains("power-INPUT"));
        // Sugar forms.
        assert!(p.contains("power:"));
        assert!(p.contains("between:"));
        assert!(p.contains("decouple:"));
        // Workflow doctrine + real-lib guidance.
        assert!(p.contains("search_symbols"));
        assert!(p.contains("validate_design"));
        assert!(p.contains("apply_design"));
        assert!(p.contains("commit:false"));
        assert!(p.contains("C_VCAP1")); // plain-refdes guidance
    }

    #[test]
    fn system_prompt_covers_the_pcb_workflow_and_triage() {
        let p = system_prompt();
        // Board tool flow.
        assert!(p.contains("search_footprints"));
        assert!(p.contains("assign_footprints"));
        assert!(p.contains("derive_board"));
        assert!(p.contains("set_placement_hints"));
        assert!(p.contains("place_board"));
        assert!(p.contains("render_board"));
        assert!(p.contains("route_board"));
        assert!(p.contains("export_board"));
        // Failure-provenance cheat sheet (the four stage prefixes).
        assert!(p.contains("global:"));
        assert!(p.contains("assign:"));
        assert!(p.contains("cell N:"));
        assert!(p.contains("finisher:"));
        // Triage levers.
        assert!(p.contains("move_part"));
        assert!(p.contains("unlock_part"));
        assert!(p.contains("set_constraints"));
        // Hard rules.
        assert!(p.contains("NEVER guess a footprint lib_id"));
        assert!(p.contains("NEVER invent trace coordinates"));
        assert!(p.contains("engine_bug"));
    }
}
