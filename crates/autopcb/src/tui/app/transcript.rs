//! The transcript model — what the chat pane is made of — plus the two writers
//! that grow and shrink it: agent-event ingestion ([`App::on_agent_event`]) and
//! the double-Esc context unwind ([`App::open_unwind`] / [`App::apply_unwind_to`]
//! and the [`UnwindPicker`] it drives).

use gordian_core::AgentEvent;
use serde_json::Value;

use super::App;

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
    /// A caution (yellow) — a turn truncated at the iteration cap.
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

/// A proposed change awaiting the user's apply-gate decision. Built from the
/// `apply_design` dry-run JSON the agent surfaces.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct PendingDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
    pub nets_before: usize,
    pub nets_after: usize,
}

impl PendingDiff {
    /// Parse the `apply_design` dry-run JSON (`{ok, would_write, diff: {...}}`)
    /// into a [`PendingDiff`]. Missing fields default to empty.
    pub fn from_dry_run(v: &Value) -> Self {
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
        Self {
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
            AgentEvent::AssistantText(t) => {
                if !t.trim().is_empty() {
                    self.transcript.push(Entry::assistant(t));
                }
            }
            AgentEvent::ToolStarted { name } => {
                self.turn_tool_calls += 1;
                self.transcript
                    .push(Entry::tool(format!("{name}(…) running…")));
            }
            AgentEvent::ToolFinished { name, summary } => {
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
            }
            AgentEvent::Applied { summary } => {
                self.status.applied_count += 1;
                self.transcript.push(Entry::system(format!("applied — {summary}")));
            }
            AgentEvent::Usage {
                input_tokens,
                output_tokens,
            } => {
                // What the next request will roughly resend is this whole call.
                self.status.ctx_tokens = input_tokens + output_tokens;
                self.status.total_input_tokens += input_tokens;
                self.status.total_output_tokens += output_tokens;
            }
            AgentEvent::Compacted {
                messages_before,
                messages_after,
            } => {
                self.transcript.push(Entry::system(format!(
                    "context compacted: {messages_before} → {messages_after} messages"
                )));
            }
            AgentEvent::Reviewed { round, score, defects } => {
                let msg = if defects.is_empty() {
                    format!("design review (round {round}): score {score}/10 — no functional defects")
                } else {
                    format!(
                        "design review (round {round}): score {score}/10 — {} defect(s) to fix:\n  {}",
                        defects.len(),
                        defects.join("\n  ")
                    )
                };
                self.transcript.push(Entry::system(msg));
            }
            AgentEvent::TurnDone(_) => {
                // Stop the spinner promptly, but leave `turn_started` for the
                // upcoming `TurnEnded` to read the elapsed time from. `TurnEnded`
                // owns the rest of teardown and is the sole indicator source, so
                // the two signals can arrive in either order without double-
                // printing or losing the clock.
                self.running = false;
            }
        }
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
    /// over the `popped` turns the agent actually dropped. Files are untouched —
    /// `/undo` is the schematic-level undo.
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
        }
        self.status.turn_count = self.status.turn_count.saturating_sub(popped);
        self.scroll = 0;
        let what = if popped == 1 {
            "the last turn".to_string()
        } else {
            format!("{popped} turns")
        };
        self.transcript.push(Entry::system(format!(
            "unwound {what} (context only — /undo restores the schematic)"
        )));
    }
}
