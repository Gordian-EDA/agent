//! The transcript model — what the chat pane is made of — plus the two writers
//! that grow and shrink it: agent-event ingestion ([`App::on_agent_event`]) and
//! the double-Esc context unwind ([`App::open_unwind`] / [`App::apply_unwind_to`]
//! and the [`UnwindPicker`] it drives).

use gordian_core::AgentEvent;
use serde_json::Value;

use super::{App, ImageCell};

/// The role of a transcript line, used by the renderer to style it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Speaker {
    /// A message the user typed.
    User,
    /// Assistant prose.
    Assistant,
    /// A collapsed tool-call card (`▸ name(args) → summary`).
    Tool,
    /// A system/status note (errors, undo confirmations, help).
    System,
}

/// The severity tint of a [`Speaker::System`] line. The renderer maps it to a
/// color; defined here (not as a ratatui `Color`) so this state machine stays
/// free of the rendering crate. Only system lines vary — every other speaker
/// ignores it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NoticeLevel {
    /// The default dim/gray note (paths, undo confirmations, hints).
    #[default]
    Plain,
    /// A clean success (green) — a turn that finished.
    Success,
    /// A caution (yellow) for non-fatal notices.
    #[allow(dead_code)]
    Warn,
    /// A failure (red) — a turn that errored out.
    Error,
}

/// One line in the chat transcript.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub speaker: Speaker,
    pub text: String,
    /// Severity tint for system lines; ignored for other speakers.
    pub level: NoticeLevel,
}

impl Entry {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            speaker: Speaker::User,
            text: text.into(),
            level: NoticeLevel::Plain,
        }
    }
    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            speaker: Speaker::Assistant,
            text: text.into(),
            level: NoticeLevel::Plain,
        }
    }
    pub fn tool(text: impl Into<String>) -> Self {
        Self {
            speaker: Speaker::Tool,
            text: text.into(),
            level: NoticeLevel::Plain,
        }
    }
    pub fn system(text: impl Into<String>) -> Self {
        Self {
            speaker: Speaker::System,
            text: text.into(),
            level: NoticeLevel::Plain,
        }
    }
    /// A system line with a severity tint (success/warn/error).
    pub fn notice(level: NoticeLevel, text: impl Into<String>) -> Self {
        Self {
            speaker: Speaker::System,
            text: text.into(),
            level,
        }
    }
}

/// A proposed mutation awaiting the user's decision. Schematic applies carry a
/// real dry-run diff; immediate PCB/project operations carry their exact name
/// and model-supplied arguments because they cannot be previewed safely.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PendingApproval {
    Schematic {
        added: Vec<String>,
        removed: Vec<String>,
        changed: Vec<String>,
        nets_before: usize,
        nets_after: usize,
    },
    Operation {
        operation: String,
        arguments: Value,
    },
}

impl PendingApproval {
    /// Parse either an immediate-operation proposal or the `apply_design`
    /// dry-run JSON. Missing schematic diff fields default to empty.
    pub fn from_payload(v: &Value) -> Self {
        if v.get("approval_kind").and_then(Value::as_str) == Some("operation") {
            return Self::Operation {
                operation: v
                    .get("operation")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown operation")
                    .to_string(),
                arguments: v.get("arguments").cloned().unwrap_or(Value::Null),
            };
        }

        let diff = v.get("diff").cloned().unwrap_or(Value::Null);
        let strings = |key: &str| -> Vec<String> {
            diff.get(key)
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        let num = |key: &str| diff.get(key).and_then(Value::as_u64).unwrap_or(0) as usize;
        Self::Schematic {
            added: strings("added"),
            removed: strings("removed"),
            changed: strings("changed"),
            nets_before: num("nets_before"),
            nets_after: num("nets_after"),
        }
    }
}

/// The double-Esc unwind picker: a floating list of recent prompts the user can
/// roll the conversation back to. `prompts` is newest-first; `selected` is a
/// 0-based index into it. Selecting row *i* unwinds `i + 1` turns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnwindPicker {
    pub prompts: Vec<String>,
    pub selected: usize,
}

impl App {
    /// Fold an agent event into the transcript / status.
    pub(super) fn on_agent_event(&mut self, ev: AgentEvent) {
        match ev {
            // A streamed chunk: grow the live in-progress assistant entry (created
            // on the first delta of a streamed run) so prose renders token-by-token.
            AgentEvent::AssistantDelta(t) => match self.live_assistant {
                Some(i) => self.transcript[i].text.push_str(&t),
                None => {
                    self.live_assistant = Some(self.transcript.len());
                    self.transcript.push(Entry::assistant(t));
                }
            },
            // The turn's final text: finalize the streamed entry in place (no
            // double-render). With no live entry — a non-streamed path — push it.
            AgentEvent::AssistantText(t) => {
                if let Some(i) = self.live_assistant.take() {
                    self.transcript[i].text = t;
                } else if !t.trim().is_empty() {
                    self.transcript.push(Entry::assistant(t));
                }
            }
            AgentEvent::ToolStarted { name } => {
                self.turn_tool_calls += 1;
                self.active_work = Some(name.clone());
            }
            AgentEvent::ToolFinished {
                name,
                summary,
                image_path,
            } => {
                // Replace the most recent "running…" card for this tool, if any,
                // so the card collapses into its result in place. The leading
                // marker glyph is the renderer's job — the text carries none, or
                // the card would show a double arrow.
                let placeholder = format!("{name}(…) running…");
                if let Some(slot) = self
                    .transcript
                    .iter_mut()
                    .rev()
                    .find(|e| e.speaker == Speaker::Tool && e.text == placeholder)
                {
                    slot.text = format!("{name} → {summary}");
                } else {
                    self.transcript
                        .push(Entry::tool(format!("{name} → {summary}")));
                }
                self.active_work = None;
                // A render tool returned a PNG: post an inline preview right after
                // the collapsed card.
                if let Some(path) = image_path {
                    self.push_image(path, name);
                }
            }
            AgentEvent::Applied { summary } => {
                self.status.applied_count += 1;
                self.transcript.push(Entry::notice(
                    NoticeLevel::Success,
                    format!("applied — {summary}"),
                ));
            }
            AgentEvent::Usage {
                provider_requests,
                input_tokens,
                output_tokens,
                cache_write_tokens,
                cache_read_tokens,
            } => {
                // What the next request will roughly resend is this whole call.
                // A failed invocation has no token report; count its request
                // without erasing the last known live context size.
                if input_tokens > 0 || output_tokens > 0 {
                    self.status.ctx_tokens = input_tokens + output_tokens;
                }
                self.status.ledger.record(
                    provider_requests,
                    input_tokens,
                    output_tokens,
                    cache_write_tokens,
                    cache_read_tokens,
                );
            }
            AgentEvent::Compacted {
                messages_before,
                messages_after,
            } => {
                self.transcript.push(Entry::system(format!(
                    "context compacted: {messages_before} → {messages_after} messages"
                )));
            }
            AgentEvent::ReviewStarted { round } => {
                self.active_work = Some(format!("design review round {round}"));
            }
            AgentEvent::Reviewed {
                round,
                score,
                defects,
            } => {
                self.active_work = None;
                let msg = if defects.is_empty() {
                    format!(
                        "design review (round {round}): score {score}/10 — no functional defects"
                    )
                } else {
                    format!(
                        "design review (round {round}): score {score}/10 — {} defect(s) to fix:\n  {}",
                        defects.len(),
                        defects.join("\n  ")
                    )
                };
                self.transcript.push(Entry::system(msg));
            }
            AgentEvent::TurnDone => {
                // Stop the spinner promptly, but leave `turn_started` for the
                // upcoming `TurnEnded` to read the elapsed time from. `TurnEnded`
                // owns the rest of teardown and is the sole indicator source, so
                // the two signals can arrive in either order without double-
                // printing or losing the clock. Any unfinalized streamed entry is
                // closed so the next turn's deltas can't append to it.
                self.live_assistant = None;
                self.running = false;
                self.active_work = None;
            }
        }
    }

    /// Post an inline image preview pinned just after the current transcript
    /// tail, so the renderer interleaves it in scroll order.
    pub(super) fn push_image(&mut self, path: impl Into<String>, caption: impl Into<String>) {
        let after = self.transcript.len();
        self.images.push(ImageCell::new(after, path, caption));
    }

    /// The path of the most recently posted render preview, if any — what
    /// `/preview` re-displays.
    pub fn latest_render_path(&self) -> Option<String> {
        self.images.last().map(|c| c.path.clone())
    }

    /// Open the unwind picker over the agent's unwindable turns (newest-first
    /// prompt previews the shell just fetched). An empty list — fresh agent or
    /// everything behind a compaction barrier — just notes "nothing to unwind".
    pub fn open_unwind(&mut self, prompts: Vec<String>) {
        if prompts.is_empty() {
            self.transcript.push(Entry::system("nothing to unwind"));
            return;
        }
        self.unwind = Some(UnwindPicker {
            prompts,
            selected: 0,
        });
    }

    /// Move the picker selection by `delta`, clamped to the list.
    pub(super) fn unwind_move(&mut self, delta: i32) {
        if let Some(p) = self.unwind.as_mut() {
            let last = p.prompts.len().saturating_sub(1);
            p.selected = (p.selected as i32 + delta).clamp(0, last as i32) as usize;
        }
    }

    /// The shell's answer to [`super::Action::UnwindTo`]: roll the transcript back
    /// over the `popped` turns the agent actually dropped. Files are untouched.
    pub fn apply_unwind_to(&mut self, popped: usize) {
        if popped == 0 {
            self.transcript.push(Entry::system("nothing to unwind"));
            return;
        }
        // Truncate at the `popped`-th-from-last user message, dropping it and
        // everything after.
        let cut = self
            .transcript
            .iter()
            .enumerate()
            .filter(|(_, e)| e.speaker == Speaker::User)
            .map(|(i, _)| i)
            .rev()
            .nth(popped - 1);
        if let Some(at) = cut {
            self.transcript.truncate(at);
            // Drop any image previews pinned past the cut so they don't dangle
            // off the rolled-back transcript.
            self.images.retain(|c| c.after <= at);
        }
        self.live_assistant = None;
        self.status.turn_count = self.status.turn_count.saturating_sub(popped);
        self.scroll = 0;
        let what = if popped == 1 {
            "the last turn".to_string()
        } else {
            format!("{popped} turns")
        };
        self.transcript
            .push(Entry::system(format!("unwound {what} (context only)")));
    }
}
