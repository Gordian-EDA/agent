//! The domain-agnostic agent loop with a human apply-gate.
//!
//! [`Agent::run_turn`] drives one user turn: it repeatedly calls the
//! [`Provider`], executes each tool the model requests through the injected
//! [`ToolProvider`] (gating writes through [`Approvals`]), and feeds the
//! structured result back, until the model returns a final text (or a safety
//! iteration cap is hit).
//!
//! The agent knows nothing about KiCAD, schematics, or PCBs — it holds a
//! `Box<dyn ToolProvider>` (the domain) and a `Box<dyn Provider>` (the LLM
//! backend). All domain detail (the tools, the system prompt, what a "commit"
//! means) lives behind those two seams.
//!
//! ## Context is persistent
//!
//! The conversation lives in `Agent::history` and is carried across turns. It can
//! be unwound one turn at a time ([`Agent::pop_last_turn`]), cleared
//! ([`Agent::clear_history`]), or compacted into a summary ([`Agent::compact`]).
//!
//! ## The apply-gate (preview → approve → commit)
//!
//! A [`ToolEffect::Gated`] tool the model invokes with intent to apply
//! ([`ToolProvider::wants_apply`]) is NOT written immediately. The loop:
//!
//! 1. Runs the tool in [`RunMode::Preview`] to obtain the diff WITHOUT writing.
//! 2. If the preview is not `ready` (e.g. the input didn't compile), returns the
//!    diagnostics straight back so the model self-repairs — no approval prompt.
//! 3. Hands the preview value to [`Approvals::approve`].
//! 4. On approval, re-runs in [`RunMode::Commit`] (the real write) and marks the
//!    turn applied, emitting [`AgentEvent::Applied`].
//! 5. On rejection, feeds a "user rejected" result back to the model.
//!
//! [`AutoApprove`] is the headless test/automation implementation; an interactive
//! UI supplies its own.

use anyhow::Result;
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;

use llm_client::{Completion, ContentBlock, ImageData, Message, Provider, Role, StreamEvent, ToolCall};

use crate::tool::{ApplyInfo, RunMode, ToolEffect, ToolProvider};

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

/// A summary of one finished turn, carried in [`AgentEvent::TurnDone`].
#[derive(Clone, Debug)]
pub struct TurnOutcomeSummary {
    /// Whether an approved write actually committed this turn.
    pub applied: bool,
    /// How many tool calls the loop executed.
    pub tool_calls_made: usize,
    /// The model's final text reply.
    pub final_text: String,
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
    ToolFinished { name: String, summary: String },
    /// An approved gated write committed; `summary` is the domain's one-line
    /// post-write digest (e.g. ERC counts).
    Applied { summary: String },
    /// Provider-reported token usage for one model call. `input_tokens` is the
    /// full prompt size (system + history + tools) — i.e. the live context.
    Usage { input_tokens: u64, output_tokens: u64 },
    /// `compact` replaced the conversation history with a summary pair.
    Compacted { messages_before: usize, messages_after: usize },
    /// The turn finished.
    TurnDone(TurnOutcomeSummary),
    /// An independent review pass over the committed work completed (from
    /// [`Agent::run_turn_reviewed`]). `round` 0 is the first review.
    Reviewed { round: usize, score: f64, defects: Vec<String> },
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

/// Drain one provider [`stream`](Provider::stream) to its final [`Completion`],
/// forwarding each text delta as an [`AgentEvent::AssistantDelta`] so the UI can
/// render tokens as they arrive. The terminating `Completed` event supplies the
/// assembled tool calls, stop reason, and usage; its text falls back to the
/// concatenated deltas when the backend didn't fill it. Errors if the stream
/// ends without a `Completed` event (a malformed / truncated stream).
async fn stream_completion(
    mut events_stream: llm_client::EventStream<'_>,
    ui: Events<'_>,
) -> Result<Completion> {
    let mut text = String::new();
    while let Some(ev) = events_stream.next().await {
        match ev? {
            StreamEvent::TextDelta(t) => {
                if !t.is_empty() {
                    text.push_str(&t);
                    emit(ui, AgentEvent::AssistantDelta(t));
                }
            }
            StreamEvent::Completed(mut c) => {
                if c.text.is_empty() && !text.is_empty() {
                    c.text = text;
                }
                return Ok(c);
            }
        }
    }
    anyhow::bail!("stream ended without a Completed event")
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

/// An agent session over one project: the LLM client, the domain tool provider,
/// the system prompt, and the persistent conversation.
pub struct Agent {
    client: Box<dyn Provider>,
    tools: Box<dyn ToolProvider>,
    /// The domain's system prompt (the LLM's standing instructions).
    system: String,
    /// The whole session's conversation, carried across turns. Tool results live
    /// here too — context is everything the next request will see.
    history: Vec<Message>,
    /// `history.len()` at the start of each user turn, so [`Agent::pop_last_turn`]
    /// can unwind exactly one exchange.
    turn_starts: Vec<usize>,
}

impl Agent {
    /// Build an agent over a domain [`ToolProvider`] and an LLM [`Provider`].
    /// `system` is the domain's system prompt.
    pub fn new(
        client: Box<dyn Provider>,
        tools: Box<dyn ToolProvider>,
        system: impl Into<String>,
    ) -> Self {
        Self {
            client,
            tools,
            system: system.into(),
            history: Vec::new(),
            turn_starts: Vec::new(),
        }
    }

    /// The domain tool provider (so callers can inspect domain state after a turn).
    pub fn tools(&self) -> &dyn ToolProvider {
        self.tools.as_ref()
    }

    /// Drop the entire conversation history (a fresh start; project files
    /// untouched). Begins a NEW thread/session, so the LLM client regenerates its
    /// `thread_identifier`.
    pub fn clear_history(&mut self) {
        self.history.clear();
        self.turn_starts.clear();
        self.client.new_thread();
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
            .flat_map(|m| &m.content)
            .map(|b| match b {
                ContentBlock::Text(t) => t.len(),
                ContentBlock::ToolUse { input, .. } => input.to_string().len(),
                ContentBlock::ToolResult { content, .. } => content.len(),
                // Base64 bytes aren't text tokens; don't inflate the proxy with them.
                ContentBlock::Image(_) => 0,
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
        let defs = self.tools.defs();
        let mut messages = self.history.clone();
        messages.push(Message::user(COMPACT_PROMPT));
        let completion = self.client.complete(&self.system, &messages, &defs).await?;
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
            AgentEvent::Compacted { messages_before: before, messages_after: after },
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
        let defs = self.tools.defs();

        repair_history(&mut self.history);
        self.turn_starts.push(self.history.len());
        self.history.push(Message::user(user_msg));

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
            // (each delta forwarded as `AssistantDelta`), while accumulating the
            // same final `Completion` — text, tool calls, usage — the loop drove
            // before. A non-streaming backend's default `stream` yields one big
            // delta then the completion, so the loop is unchanged for it.
            let completion = stream_completion(
                self.client.stream(&self.system, &self.history, &defs).await?,
                events,
            )
            .await?;
            emit(
                events,
                AgentEvent::Usage {
                    input_tokens: completion.input_tokens,
                    output_tokens: completion.output_tokens,
                },
            );

            // Finalize the streamed prose so non-streaming consumers and the
            // transcript see the whole assistant text once.
            if !completion.text.is_empty() {
                emit(events, AgentEvent::AssistantText(completion.text.clone()));
            }

            // Record the assistant turn (text + any tool_use blocks) verbatim.
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
            self.history.push(Message { role: Role::Assistant, content: assistant_blocks });

            // No tool calls → the model wants to stop.
            if completion.tool_calls.is_empty() {
                final_text = completion.text;

                // Catch the premature stop: the model did authoring work but ended
                // the turn WITHOUT ever attempting a commit, so nothing ships.
                // Re-prompt it to finish + commit (bounded). Excluded: a deliberate
                // human rejection (DID attempt a commit), a pure-text stop (no
                // authoring work), and a flow that never commits.
                if did_authoring_work && !applied && !commit_attempted && nudges_left > 0 {
                    nudges_left -= 1;
                    self.history.push(Message::user(self.tools.commit_nudge()));
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
                let gated_commit = self.tools.effect(&call.name) == ToolEffect::Gated
                    && self.tools.wants_apply(call);
                if gated_commit {
                    commit_attempted = true;
                }
                if self.tools.is_authoring_for_commit(&call.name) {
                    did_authoring_work = true;
                }
                emit(events, AgentEvent::ToolStarted { name: call.name.clone() });
                let (content, images) =
                    self.run_tool_call(call, gated_commit, approvals, &mut applied, events).await;
                emit(
                    events,
                    AgentEvent::ToolFinished {
                        name: call.name.clone(),
                        summary: self.tool_summary(call, &content),
                    },
                );
                result_blocks.push(ContentBlock::ToolResult {
                    tool_use_id: call.id.clone(),
                    content,
                    images,
                });
            }
            self.history.push(Message { role: Role::User, content: result_blocks });

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

    /// Run a turn, then — only if the turn actually COMMITTED a design change —
    /// INDEPENDENTLY review the committed work (via the domain's
    /// [`ToolProvider::review_committed`]) and feed any high-confidence defects
    /// back as a fix turn, re-reviewing up to `max_fix` rounds. The reviewer is a
    /// fresh, history-free LLM call (unbiased). `intent` is the design goal. Emits
    /// [`AgentEvent::Reviewed`] per round; returns the final turn's outcome.
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
            let Some(review) = self.tools.review_committed(intent, self.client.as_ref()).await
            else {
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
            let fix = self.tools.fix_prompt(&review.defects);
            outcome = self.run_turn(&fix, approvals, events).await?;
        }
        Ok(outcome)
    }

    /// Execute one tool call, returning the JSON-stringified result and any images
    /// to feed back. A gated-commit call is routed through the apply-gate; every
    /// other call runs once in [`RunMode::Normal`].
    async fn run_tool_call(
        &self,
        call: &ToolCall,
        gated_commit: bool,
        approvals: &mut dyn Approvals,
        applied: &mut bool,
        events: Events<'_>,
    ) -> (String, Vec<ImageData>) {
        if gated_commit {
            return self.gated_apply(call, approvals, applied, events).await;
        }
        let outcome = self.tools.run(call, RunMode::Normal, self.client.as_ref()).await;
        (outcome.value.to_string(), outcome.images)
    }

    /// The apply-gate: preview to get the diff, ask for approval, and only then
    /// commit. On rejection nothing is written and the model is told.
    async fn gated_apply(
        &self,
        call: &ToolCall,
        approvals: &mut dyn Approvals,
        applied: &mut bool,
        events: Events<'_>,
    ) -> (String, Vec<ImageData>) {
        // 1. Preview (no write) to get the diff.
        let preview = self.tools.run(call, RunMode::Preview, self.client.as_ref()).await;
        let preview_apply = preview.apply.clone().unwrap_or_default();

        // If the preview isn't ready (e.g. the input didn't compile), there is
        // nothing to approve — return the diagnostics straight back so the model
        // self-repairs.
        if !preview_apply.ready {
            return (preview.value.to_string(), preview.images);
        }

        // 2. Human apply-gate on the preview value.
        if !approvals.approve(&preview.value).await {
            let rejected = json!({
                "ok": true,
                "written": false,
                "rejected": true,
                "note": "user rejected the proposed change; nothing was written",
            });
            return (rejected.to_string(), Vec::new());
        }

        // 3. Approved → commit (the real write).
        let committed = self.tools.run(call, RunMode::Commit, self.client.as_ref()).await;
        if let Some(ApplyInfo { committed: true, summary, .. }) = &committed.apply {
            *applied = true;
            emit(events, AgentEvent::Applied { summary: summary.clone() });
        }
        (committed.value.to_string(), committed.images)
    }

    /// A short one-liner for a finished tool call, for a collapsed UI card.
    fn tool_summary(&self, call: &ToolCall, result_json: &str) -> String {
        let result: Value = serde_json::from_str(result_json).unwrap_or(Value::Null);
        self.tools.summary(call, &result)
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
            history.push(Message { role: Role::User, content: results });
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

/// Newest-first prompt previews for the turns recorded in `turn_starts`.
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
/// history back to it. Returns how many were actually popped.
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
        assert_eq!(history.len(), 4, "{history:#?}");
        match &history[2].content[0] {
            ContentBlock::ToolResult { tool_use_id, content, .. } => {
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

        assert_eq!(pop_n(&mut history, &mut starts, 5), 1);
        assert!(history.is_empty() && starts.is_empty());
        assert_eq!(pop_n(&mut history, &mut starts, 1), 0, "nothing left to pop");
    }
}
