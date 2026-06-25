//! The agent turn loop with a human apply-gate — wired directly to the KiCAD tools.
//!
//! [`Agent::run_turn`] drives one user turn: it repeatedly calls the
//! [`Provider`], executes each tool the model requests (gating writes through
//! [`Approvals`]), and feeds the structured result back, until the model returns a
//! final text (or a safety iteration cap is hit). There is one domain (KiCAD,
//! forever), so the loop dispatches [`crate::tools::run_tool`] /
//! [`crate::tools::tool_defs`] DIRECTLY — off-loading the synchronous, non-`Send`
//! [`PcbToolCtx`] work onto the blocking pool at the call site.
//!
//! ## Context is persistent
//!
//! The conversation lives in `Agent::history` and is carried across turns. It can
//! be unwound one turn at a time ([`Agent::pop_last_turn`]), cleared
//! ([`Agent::clear_history`]), or compacted into a summary ([`Agent::compact`]).
//!
//! ## The apply-gate (preview → approve → commit)
//!
//! `apply_design` with `commit:true` is the one [`ToolEffect::Gated`] write; it is
//! NOT written immediately. The loop:
//!
//! 1. Runs `apply_design(commit:false)` in [`RunMode::Preview`] to obtain the diff.
//! 2. If the preview is not `ready` (e.g. the input didn't compile), returns the
//!    diagnostics straight back so the model self-repairs — no approval prompt.
//! 3. Hands the preview value to [`Approvals::approve`].
//! 4. On approval, re-runs `apply_design(commit:true)` in [`RunMode::Commit`] and
//!    marks the turn applied, emitting [`AgentEvent::Applied`].
//! 5. On rejection, feeds a "user rejected" result back to the model.
//!
//! [`AutoApprove`] is the headless test/automation implementation; an interactive
//! UI supplies its own.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use base64::Engine as _;
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;

use crate::llm::{
    Binary, ChatMessage, ChatRole, ChatStreamEvent, ContentPart, EventStream, GenaiProvider,
    MessageContent, Provider, StreamEnd, ToolCall, ToolResponse, completed_text, token_usage,
};

use crate::tool::{ApplyInfo, ReviewOutcome, RunMode, ToolEffect, ToolOutcome};
use crate::tools::{IMAGE_PATH_KEY, PcbToolCtx, run_tool, tool_defs};

/// Safety cap on LLM round-trips per turn. Generous enough for the longest
/// legitimate flow, bounded so a misbehaving model can't loop forever.
const MAX_ITERATIONS: usize = 90;

/// How many times a turn that ended WITHOUT committing (the model researched or
/// drafted but never applied) is re-prompted to finish + commit before we give
/// up. Bounded so a model that genuinely can't finish doesn't loop forever.
const MAX_COMMIT_NUDGES: usize = 2;

/// The human apply-gate. The loop calls [`Approvals::approve`] with the preview
/// value before any gated commit; returning `false` cancels the write.
///
/// `approve` is **async**: in a UI the gate blocks the turn until the user
/// answers, which is inherently a wait on another task. Headless implementations
/// ([`AutoApprove`]) return immediately.
#[async_trait]
pub trait Approvals: Send {
    /// Decide whether to commit the proposed change, given the gated tool's
    /// preview value.
    async fn approve(&mut self, preview: &Value) -> bool;
}

/// A non-interactive [`Approvals`] that always answers the same way. Used by
/// tests and headless automation.
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
    async fn approve(&mut self, _preview: &Value) -> bool {
        self.answer
    }
}

/// Events the agent loop emits as it runs, for a live UI. Headless paths pass
/// `None` and never see these.
#[derive(Clone, Debug)]
pub enum AgentEvent {
    /// An incremental chunk of assistant text, streamed live as the model
    /// produces it. Concatenating a turn's `AssistantDelta`s reconstructs the
    /// text later finalized in [`AgentEvent::AssistantText`]. A UI renders these
    /// into an in-progress entry for token-by-token feel; non-streaming
    /// consumers can ignore them and use `AssistantText` alone.
    AssistantDelta(String),
    /// The model produced assistant text (interleaved with tool calls or final).
    AssistantText(String),
    /// A tool call is about to run.
    ToolStarted { name: String },
    /// A tool call finished; `summary` is a short one-line digest for a card.
    /// `image_path` carries the on-disk PNG a render tool produced (if any), so a
    /// UI can display it inline; it is `None` for every non-render tool.
    ToolFinished {
        name: String,
        summary: String,
        image_path: Option<String>,
    },
    /// An approved gated write committed; `summary` is the domain's one-line
    /// post-write digest (e.g. ERC counts).
    Applied { summary: String },
    /// Provider-reported token usage for one model call. `input_tokens` is the
    /// full prompt size (system + history + tools) — i.e. the live context — and
    /// *includes* `cache_write_tokens` and `cache_read_tokens`. The cache counts
    /// let a UI bill the cached prefix at the cheaper rate and surface caching.
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cache_write_tokens: u64,
        cache_read_tokens: u64,
    },
    /// `compact` replaced the conversation history with a summary pair.
    Compacted {
        messages_before: usize,
        messages_after: usize,
    },
    /// The turn finished.
    TurnDone,
    /// An independent review pass over the committed work completed (from
    /// [`Agent::run_turn_reviewed`]). `round` 0 is the first review.
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

/// Drain one provider [`stream`](Provider::stream) to its terminal
/// [`ChatStreamEvent::End`], forwarding each text chunk as an
/// [`AgentEvent::AssistantDelta`] so the UI can render tokens as they arrive.
/// Returns `(reply text, the End's StreamEnd)`: the text is the chunks
/// concatenated (the live reply); the [`StreamEnd`] carries the captured tool
/// calls + usage. Errors if the stream ends without an `End` event (a malformed /
/// truncated stream).
async fn stream_completion(
    mut events_stream: EventStream<'_>,
    ui: Events<'_>,
) -> Result<(String, StreamEnd)> {
    let mut text = String::new();
    while let Some(ev) = events_stream.next().await {
        match ev? {
            ChatStreamEvent::Chunk(chunk) => {
                if !chunk.content.is_empty() {
                    text.push_str(&chunk.content);
                    emit(ui, AgentEvent::AssistantDelta(chunk.content));
                }
            }
            ChatStreamEvent::End(end) => return Ok((text, end)),
            // Start markers, reasoning, thought-signature, and tool-call chunks:
            // the tool calls surface assembled on the End event.
            _ => {}
        }
    }
    anyhow::bail!("stream ended without an End event")
}

/// Why a [`Agent::run_turn`] stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// The model returned a final text with no pending tool calls — done.
    Completed,
    /// The loop hit `MAX_ITERATIONS` before the model finished; the turn was cut
    /// off mid-work. The conversation persists, so a follow-up "continue"
    /// resumes it.
    IterationCap,
}

/// The result of one [`Agent::run_turn`].
#[derive(Clone, Debug)]
pub struct TurnOutcome {
    /// Whether an approved gated write actually committed this turn.
    pub applied: bool,
    /// The model's final text reply.
    pub final_text: String,
    /// How many tool calls the loop executed (the preview probe before an
    /// approved commit is internal and not counted).
    pub tool_calls_made: usize,
    /// Whether the loop finished cleanly or was cut off at the iteration cap.
    pub stop_reason: StopReason,
}

/// What the loop executes its tools against. Production is [`Backend::Kicad`] (the
/// real [`PcbToolCtx`], run on the blocking pool); [`Backend::Stub`] lets the
/// gating tests drive the loop's choreography without KiCAD. This is the loop's
/// ONLY seam beyond the [`Provider`] — deliberately a two-method tool runner, not
/// a pluggable domain.
enum Backend {
    Kicad(Arc<PcbToolCtx>),
    Stub(Box<dyn TestBackend>),
}

/// A minimal, test-only stand-in for the KiCAD tool execution: just the two async
/// operations the loop's gate and review→fix choreography drive. Tests implement
/// it to exercise the gate (preview→approve→commit, self-repair, rejection,
/// stall-nudge, review-fix) deterministically without KiCAD or a network.
#[async_trait]
pub trait TestBackend: Send + Sync {
    /// Execute one tool in the given pass, returning the structured outcome.
    async fn run(&self, call: &ToolCall, mode: RunMode, reviewer: &dyn Provider) -> ToolOutcome;
    /// The independent post-turn review; `None` = nothing to review.
    async fn review_committed(
        &self,
        _intent: &str,
        _reviewer: &dyn Provider,
    ) -> Option<ReviewOutcome> {
        None
    }
}

impl Backend {
    /// Run one tool. The KiCAD backend forces `apply_design`'s `commit` flag to
    /// match the gate's pass and lifts the dry-run `ok` / commit `written` + ERC
    /// counts into [`ApplyInfo`]; everything else runs the synchronous tool on the
    /// blocking pool. `review_design` (async, needs the LLM client) is handled
    /// here too. The stub backend simply delegates.
    async fn run(&self, call: &ToolCall, mode: RunMode, reviewer: &dyn Provider) -> ToolOutcome {
        match self {
            Backend::Stub(b) => b.run(call, mode, reviewer).await,
            Backend::Kicad(ctx) => run_kicad_tool(ctx, call, mode, reviewer).await,
        }
    }

    /// The independent post-turn review of the committed work.
    async fn review_committed(
        &self,
        intent: &str,
        reviewer: &dyn Provider,
    ) -> Option<ReviewOutcome> {
        match self {
            Backend::Stub(b) => b.review_committed(intent, reviewer).await,
            Backend::Kicad(ctx) => review_committed_kicad(ctx, intent, reviewer).await,
        }
    }
}

/// An agent session over one project: the LLM client, the tool backend, the
/// system prompt, and the persistent conversation.
///
/// Generic over the [`Provider`] seam (defaulting to the one production
/// [`GenaiProvider`]) only so the deterministic, no-network tests can drive the
/// loop with a scripted client; production is always `Agent<GenaiProvider>`.
pub struct Agent<P: Provider = GenaiProvider> {
    client: P,
    backend: Backend,
    /// The KiCAD system prompt (the LLM's standing instructions).
    system: String,
    /// The whole session's conversation, carried across turns. Tool results live
    /// here too — context is everything the next request will see.
    history: Vec<ChatMessage>,
    /// `history.len()` at the start of each user turn, so [`Agent::pop_last_turn`]
    /// can unwind exactly one exchange.
    turn_starts: Vec<usize>,
}

impl<P: Provider> Agent<P> {
    /// Build an agent over a project's [`PcbToolCtx`] and a [`Provider`] client.
    /// `system` is the KiCAD system prompt.
    pub fn new(client: P, ctx: PcbToolCtx, system: impl Into<String>) -> Self {
        Self::with_backend(client, Backend::Kicad(Arc::new(ctx)), system)
    }

    /// Build an agent over a test tool backend instead of a real [`PcbToolCtx`] —
    /// the minimal seam the gating tests bind to. Production uses [`Agent::new`].
    pub fn with_test_backend(
        client: P,
        backend: Box<dyn TestBackend>,
        system: impl Into<String>,
    ) -> Self {
        Self::with_backend(client, Backend::Stub(backend), system)
    }

    fn with_backend(client: P, backend: Backend, system: impl Into<String>) -> Self {
        Self {
            client,
            backend,
            system: system.into(),
            history: Vec::new(),
            turn_starts: Vec::new(),
        }
    }

    /// The project's tool context (so callers can inspect the `.kicad_sch` path
    /// after a turn). Panics on a test backend — production paths always hold one.
    pub fn ctx(&self) -> &PcbToolCtx {
        match &self.backend {
            Backend::Kicad(ctx) => ctx,
            Backend::Stub(_) => panic!("ctx() is not available on a test backend"),
        }
    }

    /// Drop the entire conversation history (a fresh start; project files
    /// untouched).
    pub fn clear_history(&mut self) {
        self.history.clear();
        self.turn_starts.clear();
    }

    /// Unwind the most recent user turn. Returns `false` when there is nothing to
    /// pop (fresh agent, or everything before a compaction barrier).
    pub fn pop_last_turn(&mut self) -> bool {
        self.pop_turns(1) == 1
    }

    /// Unwind the `k` most recent turns, returning how many were actually popped.
    pub fn pop_turns(&mut self, k: usize) -> usize {
        pop_n(&mut self.history, &mut self.turn_starts, k)
    }

    /// Prompt previews for every turn that can still be unwound, newest first.
    pub fn unwindable_turns(&self) -> Vec<String> {
        turn_previews(&self.history, &self.turn_starts)
    }

    /// Counters for the live context (turns / messages / approximate size).
    pub fn context_stats(&self) -> ContextStats {
        let approx_chars = self
            .history
            .iter()
            .flat_map(|m| m.content.iter())
            .map(|p| match p {
                ContentPart::Text(t) => t.len(),
                ContentPart::ToolCall(tc) => tc.fn_arguments.to_string().len(),
                ContentPart::ToolResponse(tr) => tr.content.len(),
                // Base64 bytes / signatures aren't text tokens; don't inflate the proxy.
                _ => 0,
            })
            .sum();
        ContextStats {
            turns: self.turn_starts.len(),
            messages: self.history.len(),
            approx_chars,
        }
    }

    /// Compact the conversation: one tool-less model call summarizes the history,
    /// which is then replaced by a `[user summary, assistant ack]` pair. Returns
    /// `(messages_before, messages_after)` and emits [`AgentEvent::Compacted`].
    /// Compaction is a barrier: prior turns can no longer be unwound.
    pub async fn compact(&mut self, events: Events<'_>) -> Result<(usize, usize)> {
        let before = self.history.len();
        if before == 0 {
            return Ok((0, 0));
        }
        repair_history(&mut self.history);

        // Tool defs stay in the request: Converse rejects histories containing
        // toolUse/toolResult blocks unless a toolConfig is present.
        let defs = tool_defs();
        let mut messages = self.history.clone();
        messages.push(ChatMessage::user(COMPACT_PROMPT));
        let end = self.client.complete(&self.system, &messages, &defs).await?;
        let (input_tokens, output_tokens, cache_write_tokens, cache_read_tokens) =
            token_usage(&end);
        emit(
            events,
            AgentEvent::Usage {
                input_tokens,
                output_tokens,
                cache_write_tokens,
                cache_read_tokens,
            },
        );

        let summary = completed_text(&end).trim().to_string();
        if summary.is_empty() {
            anyhow::bail!("compaction failed: the model returned no summary text");
        }
        self.history = vec![
            ChatMessage::user(format!(
                "[Conversation summary — earlier context was compacted]\n{summary}"
            )),
            ChatMessage::assistant("Understood — I'll continue from that summary."),
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

    /// Drive one user turn to completion.
    ///
    /// Loops: call the model → run any requested tools (gating gated-commit calls
    /// through `approvals`) → feed results back → repeat, until the model returns
    /// a final text with no pending tool calls, or `MAX_ITERATIONS` is reached.
    pub async fn run_turn(
        &mut self,
        user_msg: &str,
        approvals: &mut dyn Approvals,
        events: Events<'_>,
    ) -> Result<TurnOutcome> {
        let defs = tool_defs();

        repair_history(&mut self.history);
        self.turn_starts.push(self.history.len());
        self.history.push(ChatMessage::user(user_msg));

        let mut applied = false;
        let mut tool_calls_made = 0usize;
        // Whether the model ever DISPATCHED a gated-commit this turn. Distinguishes
        // the genuine stall ("researched/drafted but never tried to commit") from a
        // deliberate human rejection (which DID attempt a commit) — we only nudge
        // the former.
        let mut commit_attempted = false;
        // Whether the model did authoring-for-commit work this turn. The same loop
        // also drives flows that legitimately never commit (a PCB board flow), so
        // the "didn't commit" nudge must only fire on a stalled authoring turn.
        let mut did_authoring_work = false;
        // Bounded re-prompts that push a stalled model past a premature stop.
        let mut nudges_left = MAX_COMMIT_NUDGES;
        let mut final_text = String::new();

        for _ in 0..MAX_ITERATIONS {
            // Drive the provider's stream so assistant prose renders token-by-token
            // (each chunk forwarded as `AssistantDelta`), while the terminal End
            // event carries the assembled tool calls + usage. A non-streaming
            // backend's default `stream` yields one chunk then the End, so the loop
            // is unchanged for it.
            let (text, end) = stream_completion(
                self.client
                    .stream(&self.system, &self.history, &defs)
                    .await?,
                events,
            )
            .await?;
            let (input_tokens, output_tokens, cache_write_tokens, cache_read_tokens) =
                token_usage(&end);
            let tool_calls = end.captured_into_tool_calls().unwrap_or_default();
            emit(
                events,
                AgentEvent::Usage {
                    input_tokens,
                    output_tokens,
                    cache_write_tokens,
                    cache_read_tokens,
                },
            );

            // Finalize the streamed prose so non-streaming consumers and the
            // transcript see the whole assistant text once.
            if !text.is_empty() {
                emit(events, AgentEvent::AssistantText(text.clone()));
            }

            // Record the assistant turn (text + any tool-call parts) verbatim.
            let mut assistant_parts: Vec<ContentPart> = Vec::new();
            if !text.is_empty() {
                assistant_parts.push(ContentPart::from_text(text.clone()));
            }
            for call in &tool_calls {
                assistant_parts.push(ContentPart::ToolCall(call.clone()));
            }
            self.history
                .push(ChatMessage::assistant(MessageContent::from_parts(
                    assistant_parts,
                )));

            // No tool calls → the model wants to stop.
            if tool_calls.is_empty() {
                final_text = text;

                // Catch the premature stop: the model did authoring work but ended
                // the turn WITHOUT ever attempting a commit, so nothing ships.
                // Re-prompt it to finish + commit (bounded). Excluded: a deliberate
                // human rejection (DID attempt a commit), a pure-text stop (no
                // authoring work), and a flow that never commits.
                if did_authoring_work && !applied && !commit_attempted && nudges_left > 0 {
                    nudges_left -= 1;
                    self.history.push(ChatMessage::user(COMMIT_NUDGE));
                    continue;
                }

                emit(events, AgentEvent::TurnDone);
                return Ok(TurnOutcome {
                    applied,
                    final_text,
                    tool_calls_made,
                    stop_reason: StopReason::Completed,
                });
            }

            // Run every requested tool, collecting the responses into one `tool`
            // message; any images those results attached ride a trailing `user`
            // message (genai's ToolResponse is text-only).
            let mut tool_responses: Vec<ToolResponse> = Vec::new();
            let mut result_images: Vec<ContentPart> = Vec::new();
            for call in &tool_calls {
                tool_calls_made += 1;
                let gated_commit =
                    tool_effect(&call.fn_name) == ToolEffect::Gated && wants_apply(call);
                if gated_commit {
                    commit_attempted = true;
                }
                if is_authoring_for_commit(&call.fn_name) {
                    did_authoring_work = true;
                }
                emit(
                    events,
                    AgentEvent::ToolStarted {
                        name: call.fn_name.clone(),
                    },
                );
                let (content, images, image_path) = self
                    .run_tool_call(call, gated_commit, approvals, &mut applied, events)
                    .await;
                emit(
                    events,
                    AgentEvent::ToolFinished {
                        name: call.fn_name.clone(),
                        summary: tool_summary(
                            &call.fn_name,
                            &call.fn_arguments,
                            &parse_or_null(&content),
                        ),
                        image_path,
                    },
                );
                tool_responses.push(ToolResponse::new(call.call_id.clone(), content));
                result_images.extend(images.into_iter().map(ContentPart::Binary));
            }
            self.history
                .push(ChatMessage::tool(MessageContent::from_tool_responses(
                    tool_responses,
                )));
            if !result_images.is_empty() {
                self.history
                    .push(ChatMessage::user(MessageContent::from_parts(result_images)));
            }

            // Carry any text the model emitted alongside its tool calls so a turn
            // that ends without a trailing text-only completion still has a reply.
            if !text.is_empty() {
                final_text = text;
            }
        }

        // Hit the iteration cap without a clean finish.
        if final_text.is_empty() {
            final_text = "(agent reached its iteration limit without a final answer)".to_string();
        }
        emit(events, AgentEvent::TurnDone);
        Ok(TurnOutcome {
            applied,
            final_text,
            tool_calls_made,
            stop_reason: StopReason::IterationCap,
        })
    }

    /// Run a turn, then — only if the turn actually COMMITTED a design change —
    /// INDEPENDENTLY review the committed work and feed any high-confidence
    /// defects back as a fix turn, re-reviewing up to `max_fix` rounds. The
    /// reviewer is a fresh, history-free LLM call (unbiased). `intent` is the
    /// design goal. Emits [`AgentEvent::Reviewed`] per round; returns the final
    /// turn's outcome.
    ///
    /// A read-only / conversational turn (nothing applied) skips review entirely,
    /// so the extra reviewer LLM call is paid only on authoring turns. A review
    /// that finds nothing to review ends the loop gracefully.
    pub async fn run_turn_reviewed(
        &mut self,
        user_msg: &str,
        intent: &str,
        approvals: &mut dyn Approvals,
        events: Events<'_>,
        max_fix: usize,
    ) -> Result<TurnOutcome> {
        let mut outcome = self.run_turn(user_msg, approvals, events).await?;
        // Gate: review only authoring/commit turns. Conversational and read-only
        // turns commit nothing, so there is nothing to independently review.
        if !outcome.applied {
            return Ok(outcome);
        }
        for round in 0..=max_fix {
            let Some(review) = self.backend.review_committed(intent, &self.client).await else {
                break;
            };
            emit(
                events,
                AgentEvent::Reviewed {
                    round,
                    score: review.score,
                    defects: review.defects.clone(),
                },
            );
            if review.defects.is_empty() || round == max_fix {
                break;
            }
            let fix = fix_prompt(&review.defects);
            outcome = self.run_turn(&fix, approvals, events).await?;
        }
        Ok(outcome)
    }

    /// Execute one tool call, returning the JSON-stringified result, any images to
    /// feed back, and the render PNG's on-disk path (for inline UI display). A
    /// gated-commit call is routed through the apply-gate; every other call runs
    /// once in [`RunMode::Normal`].
    async fn run_tool_call(
        &self,
        call: &ToolCall,
        gated_commit: bool,
        approvals: &mut dyn Approvals,
        applied: &mut bool,
        events: Events<'_>,
    ) -> (String, Vec<Binary>, Option<String>) {
        if gated_commit {
            return self.gated_apply(call, approvals, applied, events).await;
        }
        let outcome = self.backend.run(call, RunMode::Normal, &self.client).await;
        (
            outcome.value.to_string(),
            outcome.images,
            outcome.image_path,
        )
    }

    /// The apply-gate: preview to get the diff, ask for approval, and only then
    /// commit. On rejection nothing is written and the model is told.
    async fn gated_apply(
        &self,
        call: &ToolCall,
        approvals: &mut dyn Approvals,
        applied: &mut bool,
        events: Events<'_>,
    ) -> (String, Vec<Binary>, Option<String>) {
        // 1. Preview (no write) to get the diff.
        let preview = self.backend.run(call, RunMode::Preview, &self.client).await;
        let preview_apply = preview.apply.clone().unwrap_or_default();

        // If the preview isn't ready (e.g. the input didn't compile), there is
        // nothing to approve — return the diagnostics straight back so the model
        // self-repairs.
        if !preview_apply.ready {
            return (
                preview.value.to_string(),
                preview.images,
                preview.image_path,
            );
        }

        // 2. Human apply-gate on the preview value.
        if !approvals.approve(&preview.value).await {
            let rejected = json!({
                "ok": true,
                "written": false,
                "rejected": true,
                "note": "user rejected the proposed change; nothing was written",
            });
            return (rejected.to_string(), Vec::new(), None);
        }

        // 3. Approved → commit (the real write).
        let committed = self.backend.run(call, RunMode::Commit, &self.client).await;
        if let Some(ApplyInfo {
            committed: true,
            summary,
            ..
        }) = &committed.apply
        {
            *applied = true;
            emit(
                events,
                AgentEvent::Applied {
                    summary: summary.clone(),
                },
            );
        }
        (
            committed.value.to_string(),
            committed.images,
            committed.image_path,
        )
    }
}

/// Parse a tool result back into JSON (Null on a malformed result), for the UI
/// one-liner.
fn parse_or_null(result_json: &str) -> Value {
    serde_json::from_str(result_json).unwrap_or(Value::Null)
}

// ── KiCAD-concrete tool dispatch ───────────────────────────────────────────────

/// The effect class of a KiCAD tool name (drives the loop's gate dispatch).
fn tool_effect(name: &str) -> ToolEffect {
    match name {
        // The one human-gated write.
        "apply_design" => ToolEffect::Gated,
        // Tools that mutate draft schematic text or the live IPC board.
        "create_design" | "edit_design" | "derive_board" | "place_board" | "route_board"
        | "autoroute" | "open_board" | "move_part" | "route_track" | "set_net_width" => {
            ToolEffect::Authoring
        }
        // Everything else reads only.
        _ => ToolEffect::ReadOnly,
    }
}

/// Whether this `apply_design` call intends to APPLY (write) — i.e. `commit:true`.
fn wants_apply(call: &ToolCall) -> bool {
    call.fn_name == "apply_design"
        && call.fn_arguments.get("commit").and_then(Value::as_bool) == Some(true)
}

/// Whether a tool is schematic research/authoring whose deliverable is a committed
/// design — scopes the "ended without a committed design" nudge to schematic turns
/// only, so the PCB board flow (which never calls `apply_design`) is never nudged.
fn is_authoring_for_commit(name: &str) -> bool {
    matches!(
        name,
        "search_symbols" | "get_symbol_info" | "create_design" | "edit_design" | "apply_design"
    )
}

/// The re-prompt sent when a stalled authoring turn never committed.
const COMMIT_NUDGE: &str = "Your turn ended without a committed design — nothing was written. You MUST \
     finish the schematic now: call `create_design`/`edit_design` to author the \
     full design, then `apply_design(commit:true)` to commit it. Do this now \
     before ending your turn.";

/// The text fed back as a fix turn when the post-turn review finds defects.
fn fix_prompt(defects: &[String]) -> String {
    format!(
        "An INDEPENDENT review of the work you just committed found these \
         high-confidence defects:\n{}\n\nFix each one and re-commit.",
        defects.join("\n")
    )
}

/// Run one KiCAD tool, off-loading the synchronous, non-`Send` dispatch onto the
/// blocking pool so a compile / render / `kicad-cli` subprocess never stalls a
/// single-threaded UI runtime. `apply_design`'s Preview/Commit passes force the
/// `commit` flag and lift the gate facts into [`ApplyInfo`]; `review_design` rides
/// the async LLM client.
async fn run_kicad_tool(
    ctx: &Arc<PcbToolCtx>,
    call: &ToolCall,
    mode: RunMode,
    reviewer: &dyn Provider,
) -> ToolOutcome {
    // review_design needs the LLM client + async, so it can't ride the sync dispatch.
    if call.fn_name == "review_design" {
        return into_outcome(review_design(ctx, &call.fn_arguments, reviewer).await, None);
    }

    // The gated apply: the loop drives Preview/Commit; map each onto the dry-run /
    // commit `apply_design` body, lifting the gate facts into ApplyInfo.
    if call.fn_name == "apply_design" {
        match mode {
            RunMode::Preview => {
                let mut input = call.fn_arguments.clone();
                input["commit"] = json!(false);
                let dry = run_blocking(ctx, "apply_design", input).await;
                // `ready` = the YAML compiled (dry.ok == true); otherwise the loop
                // returns the diagnostics straight back with no approval prompt.
                let ready = dry
                    .as_ref()
                    .ok()
                    .and_then(|v| v.get("ok").and_then(Value::as_bool))
                    == Some(true);
                return into_outcome(
                    dry,
                    Some(ApplyInfo {
                        ready,
                        ..Default::default()
                    }),
                );
            }
            RunMode::Commit => {
                let mut input = call.fn_arguments.clone();
                input["commit"] = json!(true);
                let committed = run_blocking(ctx, "apply_design", input).await;
                let apply = committed.as_ref().ok().map(|v| {
                    let written = v.get("written").and_then(Value::as_bool) == Some(true);
                    let errors = v
                        .pointer("/erc/errors")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let warnings = v
                        .pointer("/erc/warnings")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
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

    into_outcome(
        run_blocking(ctx, &call.fn_name, call.fn_arguments.clone()).await,
        None,
    )
}

/// Run one synchronous tool on the blocking pool. Tools can take seconds (symbol-
/// index build, reconciled render, ERC subprocess); off-loading them keeps an
/// interactive caller redrawing.
async fn run_blocking(ctx: &Arc<PcbToolCtx>, name: &str, input: Value) -> Result<Value> {
    let ctx = Arc::clone(ctx);
    let name = name.to_string();
    tokio::task::spawn_blocking(move || run_tool(&name, input, &ctx))
        .await
        .map_err(|e| anyhow::anyhow!("tool execution task failed: {e}"))?
}

/// The `review_design` tool: an INDEPENDENT electrical-correctness review of the
/// current draft. Runs the FRESH diverse-lens LLM review (no conversation history)
/// UNIONED with the deterministic exact-math ERC.
async fn review_design(
    ctx: &Arc<PcbToolCtx>,
    input: &Value,
    reviewer: &dyn Provider,
) -> Result<Value> {
    let intent = input.get("intent").and_then(Value::as_str).unwrap_or("");
    let dv = run_blocking(ctx, "get_design", json!({})).await?;
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
    let (score, defects) = review_netlist_with_erc(ctx, reviewer, intent, &netlist).await?;
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
    ctx: &Arc<PcbToolCtx>,
    reviewer: &dyn Provider,
    intent: &str,
    netlist: &str,
) -> Result<(f64, Vec<String>)> {
    let (score, mut defects) =
        crate::review_kicad::review_netlist(reviewer, intent, netlist).await?;
    if let Some(design) = circuit_lang::compile(netlist, ctx.provider()).design {
        for d in circuit_lang::erc::erc_checks(&design) {
            if !defects.iter().any(|e| crate::review::same_defect(e, &d)) {
                defects.push(d);
            }
        }
    }
    Ok((score, defects))
}

/// The post-turn review of the committed schematic, in TWO complementary planes
/// UNIONED into one [`ReviewOutcome`]: the NETLIST plane (electrical correctness +
/// exact-math ERC), and the LAYOUT plane — an in-loop VISION critic that renders
/// the committed `.kicad_sch` to PNG and judges READABILITY.
///
/// The two defect lists are deduped by their shared `- <target>:` prefix and the
/// score is the lower of the two. The layout pass is BEST-EFFORT: a render or
/// vision failure contributes nothing and the review degrades to netlist-only.
async fn review_committed_kicad(
    ctx: &Arc<PcbToolCtx>,
    intent: &str,
    reviewer: &dyn Provider,
) -> Option<ReviewOutcome> {
    let sch = ctx.sch_path();
    if !sch.exists() {
        return None;
    }
    let netlist = sch_io::read::lift(ctx.env(), sch).ok()?;
    let (mut score, mut defects) = review_netlist_with_erc(ctx, reviewer, intent, &netlist)
        .await
        .ok()?;

    // Layout (vision) plane — best-effort, unioned in. A `(0.0, [])` result means
    // the vision pass produced no parseable verdict (no signal), so it must NOT
    // drag the score to zero — skip it.
    if let Some((layout_score, layout_defects)) =
        review_layout_schematic(ctx, reviewer, intent).await
        && !(layout_score == 0.0 && layout_defects.is_empty())
    {
        score = score.min(layout_score);
        for d in layout_defects {
            if !defects.iter().any(|e| crate::review::same_defect(e, &d)) {
                defects.push(d);
            }
        }
    }
    Some(ReviewOutcome { score, defects })
}

/// Render the committed schematic to PNG (on the blocking pool — it shells out to
/// `kicad-cli`), then run the in-loop VISION layout critic over it. `None` on ANY
/// failure (no schematic, render error, vision call error) so the caller degrades
/// to netlist-only — the layout pass must never crash a turn.
async fn review_layout_schematic(
    ctx: &Arc<PcbToolCtx>,
    reviewer: &dyn Provider,
    intent: &str,
) -> Option<(f64, Vec<String>)> {
    let ctx = Arc::clone(ctx);
    let png = tokio::task::spawn_blocking(move || {
        crate::render::schematic_png(ctx.env(), ctx.sch_path())
    })
    .await
    .ok()?
    .ok()?;
    let image = Binary::from_base64(
        "image/png",
        base64::engine::general_purpose::STANDARD.encode(png),
        None,
    );
    crate::review_kicad::review_layout(
        reviewer,
        intent,
        image,
        crate::review_kicad::LayoutKind::Schematic,
    )
    .await
    .ok()
}

/// Turn a tool's `Result<Value>` into a [`ToolOutcome`]: a tool error becomes a
/// structured `{error: …}` value (the model self-repairs), images are pulled out
/// of the value via [`take_images`] (which also surfaces the render PNG's path for
/// inline UI display), and `apply` rides along for a gated tool.
fn into_outcome(result: Result<Value>, apply: Option<ApplyInfo>) -> ToolOutcome {
    match result {
        Ok(mut value) => {
            let (images, image_path) = take_images(&mut value);
            ToolOutcome {
                value,
                images,
                image_path,
                apply,
            }
        }
        Err(e) => ToolOutcome {
            value: json!({ "error": e.to_string() }),
            images: Vec::new(),
            image_path: None,
            apply,
        },
    }
}

/// Pull a `_image_path` out of a tool result: load + base64 the PNG for the model,
/// strip the key so the model's text view stays clean, and return the path so a UI
/// can show the same PNG inline. An unreadable file yields no [`Binary`] but the
/// path is still returned (the UI falls back to a text label).
fn take_images(value: &mut Value) -> (Vec<Binary>, Option<String>) {
    let Some(path) = value
        .get(IMAGE_PATH_KEY)
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return (Vec::new(), None);
    };
    if let Some(obj) = value.as_object_mut() {
        obj.remove(IMAGE_PATH_KEY);
    }
    let images = match std::fs::read(&path) {
        Ok(bytes) => vec![Binary::from_base64(
            "image/png",
            base64::engine::general_purpose::STANDARD.encode(bytes),
            None,
        )],
        Err(e) => {
            eprintln!("render image unreadable at {path}: {e}");
            Vec::new()
        }
    };
    (images, Some(path))
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

/// The instruction `compact` sends as the final user message.
const COMPACT_PROMPT: &str = "Summarize this conversation so far for your own \
future reference: the user's goals, every design decision made, the current \
state of the schematic (components, nets, anything applied), and any open \
issues. Reply with ONLY the summary text — no tool calls.";

/// Patch a ragged history tail left by a cancelled or failed turn so the next
/// request is valid:
///
/// - a trailing assistant message with `tool_use` blocks that never got their
///   results is given synthetic "cancelled" `tool_result`s;
/// - a trailing user message (e.g. tool results whose follow-up completion never
///   ran) is closed with a synthetic assistant note, keeping the user/assistant
///   alternation valid once the next user turn is appended.
fn repair_history(history: &mut Vec<ChatMessage>) {
    let Some(last) = history.last() else {
        return;
    };

    if last.role == ChatRole::Assistant {
        let dangling: Vec<String> = last
            .content
            .iter()
            .filter_map(|p| match p {
                ContentPart::ToolCall(tc) => Some(tc.call_id.clone()),
                _ => None,
            })
            .collect();
        if !dangling.is_empty() {
            let responses: Vec<ToolResponse> = dangling
                .into_iter()
                .map(|id| {
                    ToolResponse::new(
                        id,
                        json!({
                            "cancelled": true,
                            "note": "the turn was cancelled before this tool ran",
                        })
                        .to_string(),
                    )
                })
                .collect();
            history.push(ChatMessage::tool(MessageContent::from_tool_responses(
                responses,
            )));
        }
    }

    if matches!(
        history.last().map(|m| &m.role),
        Some(ChatRole::Tool | ChatRole::User)
    ) {
        history.push(ChatMessage::assistant("(turn interrupted)"));
    }
}

/// The first text part of a message, if any (a user turn's prompt lives here).
fn first_text(m: &ChatMessage) -> Option<&str> {
    m.content.iter().find_map(|p| match p {
        ContentPart::Text(t) => Some(t.as_str()),
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

/// Newest-first prompt previews for the turns recorded in `turn_starts`.
fn turn_previews(history: &[ChatMessage], turn_starts: &[usize]) -> Vec<String> {
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
/// history back to it. Returns how many were actually popped.
fn pop_n(history: &mut Vec<ChatMessage>, turn_starts: &mut Vec<usize>, k: usize) -> usize {
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
    fn tool_effect_and_wants_apply_classify_the_gate() {
        assert_eq!(tool_effect("apply_design"), ToolEffect::Gated);
        assert_eq!(tool_effect("edit_design"), ToolEffect::Authoring);
        assert_eq!(tool_effect("search_symbols"), ToolEffect::ReadOnly);
        let call = |commit: bool| ToolCall {
            call_id: "1".into(),
            fn_name: "apply_design".into(),
            fn_arguments: json!({ "commit": commit }),
            thought_signatures: None,
        };
        assert!(wants_apply(&call(true)));
        assert!(!wants_apply(&call(false)));
    }

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
    fn repair_history_closes_a_dangling_tool_use() {
        let mut history = vec![
            ChatMessage::user("add a resistor"),
            ChatMessage::assistant(MessageContent::from_parts(vec![
                ContentPart::from_text("searching"),
                ContentPart::ToolCall(ToolCall {
                    call_id: "tu_9".into(),
                    fn_name: "search_symbols".into(),
                    fn_arguments: json!({ "query": "R" }),
                    thought_signatures: None,
                }),
            ])),
        ];
        repair_history(&mut history);
        assert_eq!(history.len(), 4, "{history:#?}");
        match &history[2].content.parts()[0] {
            ContentPart::ToolResponse(tr) => {
                assert_eq!(tr.call_id, "tu_9");
                assert!(tr.content.contains("cancelled"));
            }
            other => panic!("expected a tool response, got {other:?}"),
        }
        assert_eq!(history[3].role, ChatRole::Assistant);
    }

    #[test]
    fn repair_history_closes_a_trailing_user_message() {
        let mut history = vec![ChatMessage::user("hello")];
        repair_history(&mut history);
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].role, ChatRole::Assistant);
    }

    #[test]
    fn repair_history_leaves_clean_histories_alone() {
        let mut empty: Vec<ChatMessage> = Vec::new();
        repair_history(&mut empty);
        assert!(empty.is_empty());

        let mut clean = vec![ChatMessage::user("hi"), ChatMessage::assistant("done")];
        repair_history(&mut clean);
        assert_eq!(clean.len(), 2, "a finished exchange needs no repair");
    }

    #[test]
    fn turn_previews_are_newest_first_and_single_lined() {
        let history = vec![
            ChatMessage::user("  first   prompt  "),
            ChatMessage::assistant("ok"),
            ChatMessage::user("second prompt"),
            ChatMessage::assistant("done"),
        ];
        let starts = vec![0, 2];
        let p = turn_previews(&history, &starts);
        assert_eq!(
            p,
            vec!["second prompt".to_string(), "first prompt".to_string()]
        );
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
            ChatMessage::user("t1"),
            ChatMessage::assistant("a1"),
            ChatMessage::user("t2"),
            ChatMessage::assistant("a2"),
            ChatMessage::user("t3"),
            ChatMessage::assistant("a3"),
        ];
        let mut starts = vec![0, 2, 4];

        assert_eq!(pop_n(&mut history, &mut starts, 2), 2, "popped two");
        assert_eq!(starts, vec![0], "only the oldest turn remains");
        assert_eq!(history.len(), 2, "history truncated to t1's exchange");

        assert_eq!(pop_n(&mut history, &mut starts, 5), 1);
        assert!(history.is_empty() && starts.is_empty());
        assert_eq!(
            pop_n(&mut history, &mut starts, 1),
            0,
            "nothing left to pop"
        );
    }
}
