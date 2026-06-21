//! The copilot-cockpit **state machine** — pure-ish, non-rendering, testable.
//!
//! [`App`] is the whole UI state. [`App::update`] maps a [`Msg`] (a keypress,
//! an agent event, or a pending-diff arrival) into a state transition and
//! returns an [`Action`] the shell performs (spawn a turn, resolve the
//! apply-gate, cancel, undo, quit). Nothing here touches a terminal or the
//! network, so it is unit-testable in full.
//!
//! The shell ([`super::run`]) owns the terminal, the crossterm event stream, and
//! the agent task; it translates raw input into [`Msg`]s, calls `update`, and
//! acts on the returned [`Action`]. The renderer ([`super::ui`]) reads the `App`
//! and only writes back one thing: a clamped scroll offset (it alone knows the
//! viewport size).

use std::time::Instant;

use agent::AgentEvent;
use serde_json::Value;

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

/// One `/command` the input line accepts, for dispatch and Tab completion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandSpec {
    /// Full spelling including the leading slash, e.g. `/help`.
    pub name: &'static str,
    /// One-line description shown in the completion popup and help.
    pub desc: &'static str,
}

/// Every command, in display order.
pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "/help",
        desc: "show keys and commands",
    },
    CommandSpec {
        name: "/auto",
        desc: "toggle auto-approve (yolo)",
    },
    CommandSpec {
        name: "/undo",
        desc: "restore the previous schematic",
    },
    CommandSpec {
        name: "/clear",
        desc: "clear the transcript AND the agent's context",
    },
    CommandSpec {
        name: "/context",
        desc: "show project paths and context/token stats",
    },
    CommandSpec {
        name: "/compact",
        desc: "summarize the conversation to shrink context",
    },
    CommandSpec {
        name: "/quit",
        desc: "exit",
    },
];

/// Static-ish status shown in the status bar.
#[derive(Clone, Debug)]
pub struct Status {
    /// Provider label, e.g. `bedrock`.
    pub provider: String,
    /// Model id, e.g. `us.anthropic.claude-opus-4-5-...`.
    pub model: String,
    /// The project's `.kicad_sch` path (as a display string).
    pub sch_path: String,
    /// Whether a KiCAD install was detected (cli + symbol libs).
    pub kicad_connected: bool,
    /// How many turns committed a write this session.
    pub applied_count: usize,
    /// How many user turns ran this session.
    pub turn_count: usize,
    /// Live context size: prompt tokens of the latest model call (system +
    /// history + tools), plus its output — what the *next* call will roughly
    /// resend. 0 until the first call reports usage.
    pub ctx_tokens: u64,
    /// Cumulative provider-reported tokens this session.
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
}

impl Status {
    pub fn new(
        provider: impl Into<String>,
        model: impl Into<String>,
        sch_path: impl Into<String>,
        kicad_connected: bool,
    ) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            sch_path: sch_path.into(),
            kicad_connected,
            applied_count: 0,
            turn_count: 0,
            ctx_tokens: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
        }
    }
}

/// An input event or async arrival the [`App`] reacts to.
#[derive(Clone, Debug)]
pub enum Msg {
    /// A printable character typed into the input line.
    Char(char),
    /// Backspace — delete the char before the cursor.
    Backspace,
    /// Delete — delete the char under the cursor.
    Delete,
    /// Move the input cursor.
    CursorLeft,
    CursorRight,
    Home,
    End,
    /// Ctrl-U — kill from the line start to the cursor.
    KillToStart,
    /// Ctrl-W — kill the word before the cursor.
    KillWordBack,
    /// Recall the previous / next prompt from history (Up / Down).
    HistoryPrev,
    HistoryNext,
    /// Tab — complete / cycle the `/command` matching the input.
    Complete,
    /// Enter — submit the input line (a prompt or a `/command`).
    Submit,
    /// Approve the pending diff (`a`).
    Approve,
    /// Reject the pending diff (`r`).
    Reject,
    /// Scroll the transcript up / down by one line.
    ScrollUp,
    ScrollDown,
    /// Esc — close help / reject a gate / clear input / cancel a turn / arm
    /// (then perform) a context unwind, in that order of precedence.
    Cancel,
    /// Ctrl-C — quit unconditionally.
    ForceQuit,
    /// A periodic animation tick from the shell (advances the spinner).
    Tick,
    /// An event from the running agent turn.
    Agent(AgentEvent),
    /// The apply-gate fired: a dry-run diff awaits a decision.
    PendingDiff(Value),
    /// A turn finished (the spawned task joined). This is the single, reliable
    /// teardown point — it fires exactly once per turn (from the join channel,
    /// or directly from the shell on a user abort) and carries *why* the turn
    /// stopped so the indicator can be labelled. Clears the running flag even if
    /// no `TurnDone` event arrived (e.g. the turn errored or was interrupted).
    TurnEnded(TurnEndReason),
}

/// Why an in-flight turn stopped, carried on [`Msg::TurnEnded`]. The agent loop
/// reports `Completed`/`IterationCap` (via its `StopReason`); the shell adds
/// `Interrupted` (user abort) and `Error`; `/compact` reports `Compacted`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnEndReason {
    /// The model returned a final reply — a clean finish.
    Completed,
    /// The loop hit its per-turn iteration cap and was cut off mid-work.
    IterationCap,
    /// The user pressed Esc to abort the turn.
    Interrupted,
    /// The turn failed (provider/network/tool error); carries the message.
    Error(String),
    /// A `/compact` run finished (its own shrink note is shown separately).
    Compacted,
}

/// What the shell must do after an [`App::update`].
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Do nothing further.
    None,
    /// Spawn an agent turn with this prompt.
    SpawnTurn(String),
    /// Resolve the pending apply-gate with this decision.
    ResolveApproval(bool),
    /// Abort the in-flight agent turn.
    CancelTurn,
    /// Restore the previous schematic from the snapshot store.
    Undo,
    /// `/clear` — drop the agent's conversation history (the transcript is
    /// already cleared by the time this is returned).
    ClearContext,
    /// Double-Esc — the shell fetches the agent's unwindable turns and opens the
    /// picker via [`App::open_unwind`].
    OpenUnwind,
    /// The picker was confirmed: pop this many of the agent's most recent turns,
    /// then roll the transcript back via [`App::apply_unwind_to`].
    UnwindTo(usize),
    /// `/compact` — run the agent's context compaction (spinner like a turn).
    Compact,
    /// `/context` — the shell gathers agent stats and prints them.
    ShowContext,
    /// Tear down the TUI and exit.
    Quit,
}

/// The double-Esc unwind picker: a floating list of recent prompts the user can
/// roll the conversation back to. `prompts` is newest-first; `selected` is a
/// 0-based index into it. Selecting row *i* unwinds `i + 1` turns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnwindPicker {
    pub prompts: Vec<String>,
    pub selected: usize,
}

/// The full cockpit state.
pub struct App {
    /// The chat transcript (top pane), oldest first.
    pub transcript: Vec<Entry>,
    /// The current input-line buffer.
    pub input: String,
    /// Cursor position in the input line, in **chars** (0 ..= char count).
    pub cursor: usize,
    /// Previously submitted prompts, oldest first.
    pub history: Vec<String>,
    /// While browsing history: the index being shown. `None` = live draft.
    pub history_pos: Option<usize>,
    /// The live draft stashed while browsing history.
    draft: String,
    /// First idle Esc pressed: the next Esc unwinds the last turn. Any other
    /// user action disarms.
    pub esc_armed: bool,
    /// The typed `/`-prefix Tab completion is cycling against (the input
    /// itself once Tab starts rewriting it no longer matches).
    completion_stem: Option<String>,
    /// Index into the stem's matches that the input currently shows.
    pub completion_idx: Option<usize>,
    /// Apply-gate mode: when `false` (default) every write needs approval; when
    /// `true` (`:auto`) writes commit without a prompt.
    pub auto: bool,
    /// A change awaiting approval, if any. While `Some`, `a`/`r` resolve it.
    pub pending: Option<PendingDiff>,
    /// Whether an agent turn is in flight (submit is blocked, typing is not).
    pub running: bool,
    /// When the in-flight turn started (drives the elapsed display).
    pub turn_started: Option<Instant>,
    /// `total_output_tokens` snapshot at turn start, so the running line can show
    /// the output tokens streamed *this* turn ([`App::turn_output_tokens`]).
    pub turn_output_base: u64,
    /// Tool calls started during the current turn, counted from `ToolStarted`
    /// events. Tracked here (not read from `TurnOutcome`) so the end indicator
    /// can report a count even when the turn was interrupted or errored — paths
    /// that never return an outcome.
    pub turn_tool_calls: usize,
    /// Animation frame counter, advanced by [`Msg::Tick`] while running.
    pub spinner: usize,
    /// The unwind picker, while the user is choosing how far to roll back.
    pub unwind: Option<UnwindPicker>,
    /// Whether `:help` is showing.
    pub help: bool,
    /// Lines scrolled up from the bottom of the transcript (0 = follow tail).
    /// The renderer clamps this to the real maximum for the viewport.
    pub scroll: u16,
    /// Status-bar data.
    pub status: Status,
    /// Set once the user asks to quit; the shell's loop exits.
    pub should_quit: bool,
}

impl App {
    /// Build a fresh cockpit over a project.
    pub fn new(status: Status) -> Self {
        let mut app = Self {
            transcript: Vec::new(),
            input: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_pos: None,
            draft: String::new(),
            esc_armed: false,
            completion_stem: None,
            completion_idx: None,
            auto: false,
            pending: None,
            running: false,
            turn_started: None,
            turn_output_base: 0,
            turn_tool_calls: 0,
            spinner: 0,
            unwind: None,
            help: false,
            scroll: 0,
            status,
            should_quit: false,
        };
        // The empty transcript renders a welcome splash (see `ui::draw_welcome`),
        // so no seed entry is needed.
        app
    }

    /// Apply one message, mutating state and returning the shell's next action.
    pub fn update(&mut self, msg: Msg) -> Action {
        // While the unwind picker owns the screen it is modal: arrow keys move the
        // selection, Enter confirms, Esc cancels, and every other key is swallowed
        // so it can't disturb the input or scroll underneath.
        if self.unwind.is_some() {
            return match msg {
                Msg::HistoryPrev | Msg::ScrollUp => {
                    self.unwind_move(-1);
                    Action::None
                }
                Msg::HistoryNext | Msg::ScrollDown => {
                    self.unwind_move(1);
                    Action::None
                }
                Msg::Submit | Msg::Approve => self.confirm_unwind(),
                Msg::Cancel => {
                    self.unwind = None;
                    Action::None
                }
                _ => Action::None,
            };
        }

        // Any user action other than another Esc disarms the pending unwind.
        if !matches!(
            msg,
            Msg::Cancel | Msg::Tick | Msg::Agent(_) | Msg::PendingDiff(_) | Msg::TurnEnded(_)
        ) {
            self.esc_armed = false;
        }
        // Any input change other than Tab itself restarts completion cycling.
        if !matches!(
            msg,
            Msg::Complete
                | Msg::Tick
                | Msg::Agent(_)
                | Msg::PendingDiff(_)
                | Msg::TurnEnded(_)
                | Msg::ScrollUp
                | Msg::ScrollDown
        ) {
            self.completion_stem = None;
            self.completion_idx = None;
        }

        match msg {
            Msg::Char(c) => {
                // While a diff is pending, the keyboard belongs to the gate.
                if self.pending.is_some() {
                    match c {
                        'a' => return self.resolve_pending(true),
                        'r' => return self.resolve_pending(false),
                        _ => return Action::None,
                    }
                }
                self.insert_char(c);
                Action::None
            }
            Msg::Backspace => {
                if self.cursor > 0 {
                    let at = self.byte_at(self.cursor - 1);
                    self.input.remove(at);
                    self.cursor -= 1;
                }
                Action::None
            }
            Msg::Delete => {
                if self.cursor < self.char_len() {
                    let at = self.byte_at(self.cursor);
                    self.input.remove(at);
                }
                Action::None
            }
            Msg::CursorLeft => {
                self.cursor = self.cursor.saturating_sub(1);
                Action::None
            }
            Msg::CursorRight => {
                self.cursor = (self.cursor + 1).min(self.char_len());
                Action::None
            }
            Msg::Home => {
                self.cursor = 0;
                Action::None
            }
            Msg::End => {
                self.cursor = self.char_len();
                Action::None
            }
            Msg::KillToStart => {
                let at = self.byte_at(self.cursor);
                self.input.drain(..at);
                self.cursor = 0;
                Action::None
            }
            Msg::KillWordBack => {
                self.kill_word_back();
                Action::None
            }
            Msg::HistoryPrev => {
                self.history_prev();
                Action::None
            }
            Msg::HistoryNext => {
                self.history_next();
                Action::None
            }
            Msg::Complete => {
                if self.pending.is_none() {
                    self.complete_next();
                }
                Action::None
            }
            Msg::Submit => self.submit(),
            Msg::Approve => self.resolve_pending(true),
            Msg::Reject => self.resolve_pending(false),
            Msg::ScrollUp => {
                self.scroll = self.scroll.saturating_add(1);
                Action::None
            }
            Msg::ScrollDown => {
                self.scroll = self.scroll.saturating_sub(1);
                Action::None
            }
            Msg::Cancel => self.cancel(),
            Msg::ForceQuit => {
                self.should_quit = true;
                Action::Quit
            }
            Msg::Tick => {
                if self.running {
                    self.spinner = self.spinner.wrapping_add(1);
                }
                Action::None
            }
            Msg::Agent(ev) => {
                self.on_agent_event(ev);
                Action::None
            }
            Msg::PendingDiff(v) => {
                self.pending = Some(PendingDiff::from_dry_run(&v));
                Action::None
            }
            Msg::TurnEnded(reason) => {
                self.end_turn(reason);
                Action::None
            }
        }
    }

    /// Esc, layered: close help → reject the gate → clear a non-empty input →
    /// cancel a running turn → arm, then perform, a one-turn context unwind.
    /// Esc never quits; that's `Ctrl-C` or `/quit`.
    fn cancel(&mut self) -> Action {
        if self.help {
            self.help = false;
            Action::None
        } else if self.pending.is_some() {
            self.resolve_pending(false)
        } else if !self.input.is_empty() {
            self.clear_input();
            Action::None
        } else if self.running {
            // The shell aborts the task and replies with `TurnEnded(Interrupted)`,
            // which posts the "⊘ Interrupted after …" indicator — no separate
            // note needed here.
            Action::CancelTurn
        } else if self.esc_armed {
            self.esc_armed = false;
            Action::OpenUnwind
        } else {
            self.esc_armed = true;
            Action::None
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
    fn unwind_move(&mut self, delta: i32) {
        if let Some(p) = self.unwind.as_mut() {
            let last = p.prompts.len().saturating_sub(1);
            p.selected = (p.selected as i32 + delta).clamp(0, last as i32) as usize;
        }
    }

    /// Confirm the picker: close it and ask the shell to drop `selected + 1`
    /// turns (the selected prompt and everything after it).
    fn confirm_unwind(&mut self) -> Action {
        match self.unwind.take() {
            Some(p) => Action::UnwindTo(p.selected + 1),
            None => Action::None,
        }
    }

    /// The shell's answer to [`Action::UnwindTo`]: roll the transcript back over
    /// the `popped` turns the agent actually dropped. Files are untouched —
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

    /// Submit the input line: a `/command` or a prompt.
    fn submit(&mut self) -> Action {
        let line = self.input.trim().to_string();
        if line.is_empty() {
            return Action::None;
        }
        if line.starts_with('/') {
            self.clear_input();
            return self.run_command(&line);
        }
        if let Some(cmd) = line.strip_prefix(':') {
            // The old prefix: nudge instead of sending ":help" to the model.
            self.clear_input();
            self.transcript.push(Entry::system(format!(
                "commands now start with / — try /{}",
                cmd.trim()
            )));
            return Action::None;
        }
        if self.running {
            // Don't start a second turn; the draft stays in the input line.
            return Action::None;
        }
        self.clear_input();
        if self.history.last() != Some(&line) {
            self.history.push(line.clone());
        }
        self.transcript.push(Entry::user(line.clone()));
        self.status.turn_count += 1;
        self.begin_turn();
        Action::SpawnTurn(line)
    }

    /// Run a `/command` (the leading slash is included in `line`).
    fn run_command(&mut self, line: &str) -> Action {
        match line.trim() {
            "/auto" => {
                self.auto = !self.auto;
                let state = if self.auto { "ON (yolo)" } else { "OFF" };
                self.transcript
                    .push(Entry::system(format!("apply-gate auto-approve: {state}")));
                Action::None
            }
            "/undo" => {
                if self.running {
                    self.transcript
                        .push(Entry::system("can't undo while a turn is running"));
                    Action::None
                } else {
                    Action::Undo
                }
            }
            "/clear" => {
                self.transcript.clear();
                self.scroll = 0;
                Action::ClearContext
            }
            "/context" => Action::ShowContext,
            "/compact" => {
                if self.running {
                    self.transcript
                        .push(Entry::system("can't compact while a turn is running"));
                    return Action::None;
                }
                self.begin_turn();
                self.transcript.push(Entry::system("compacting context…"));
                Action::Compact
            }
            "/help" => {
                self.help = !self.help;
                Action::None
            }
            "/quit" | "/q" => {
                self.should_quit = true;
                Action::Quit
            }
            other => {
                self.transcript.push(Entry::system(format!(
                    "unknown command {other} — /help lists them"
                )));
                Action::None
            }
        }
    }

    // ── `/command` Tab completion ─────────────────────────────────────

    /// The commands matching a `/`-prefix stem (no completion once a space is
    /// typed — arguments are not completable).
    fn matches_for(stem: &str) -> Vec<&'static CommandSpec> {
        if !stem.starts_with('/') || stem.contains(' ') {
            return Vec::new();
        }
        COMMANDS
            .iter()
            .filter(|c| c.name.starts_with(stem))
            .collect()
    }

    /// What the completion popup should show for the current input: the
    /// matching commands and which one (if any) the input currently is.
    /// `None` when completion does not apply.
    pub fn completion_view(&self) -> Option<(Vec<&'static CommandSpec>, Option<usize>)> {
        if self.pending.is_some() {
            return None;
        }
        let stem = self.completion_stem.as_deref().unwrap_or(&self.input);
        let matches = Self::matches_for(stem);
        if matches.is_empty() {
            return None;
        }
        Some((matches, self.completion_idx))
    }

    /// Tab: fill the input with the next command matching the typed stem.
    fn complete_next(&mut self) {
        let stem = self
            .completion_stem
            .clone()
            .unwrap_or_else(|| self.input.clone());
        let matches = Self::matches_for(&stem);
        if matches.is_empty() {
            return;
        }
        let idx = match self.completion_idx {
            Some(i) => (i + 1) % matches.len(),
            None => 0,
        };
        self.completion_stem = Some(stem);
        self.completion_idx = Some(idx);
        self.input = matches[idx].name.to_string();
        self.cursor = self.char_len();
    }

    /// Resolve a pending apply-gate decision. No-op (returns `None`) if nothing
    /// is pending.
    fn resolve_pending(&mut self, approve: bool) -> Action {
        if self.pending.take().is_none() {
            return Action::None;
        }
        let note = if approve { "approved" } else { "rejected" };
        self.transcript
            .push(Entry::system(format!("change {note}")));
        Action::ResolveApproval(approve)
    }

    /// Fold an agent event into the transcript / status.
    fn on_agent_event(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::AssistantText(t) => {
                if !t.trim().is_empty() {
                    self.transcript.push(Entry::assistant(t));
                }
            }
            AgentEvent::ToolStarted { name } => {
                self.turn_tool_calls += 1;
                self.transcript
                    .push(Entry::tool(format!("▸ {name}(…) running…")));
            }
            AgentEvent::ToolFinished { name, summary } => {
                // Replace the most recent "running…" card for this tool, if any,
                // so the card collapses into its result in place.
                let placeholder = format!("▸ {name}(…) running…");
                if let Some(slot) = self
                    .transcript
                    .iter_mut()
                    .rev()
                    .find(|e| e.speaker == Speaker::Tool && e.text == placeholder)
                {
                    slot.text = format!("▸ {name} → {summary}");
                } else {
                    self.transcript
                        .push(Entry::tool(format!("▸ {name} → {summary}")));
                }
            }
            AgentEvent::Applied { errors, warnings } => {
                self.status.applied_count += 1;
                self.transcript.push(Entry::system(format!(
                    "applied — ERC {errors} errors, {warnings} warnings"
                )));
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

    /// Whether keystrokes currently edit the input line (only an open
    /// apply-gate takes the keyboard away; typing during a turn is fine).
    pub fn input_active(&self) -> bool {
        self.pending.is_none()
    }

    /// Mark a turn (a prompt or `/compact`) as started: spin up the running
    /// flag, the elapsed clock, the per-turn token baseline, and follow the tail.
    fn begin_turn(&mut self) {
        self.running = true;
        self.turn_started = Some(Instant::now());
        self.turn_output_base = self.status.total_output_tokens;
        self.turn_tool_calls = 0;
        self.scroll = 0;
    }

    /// Tear a turn down and post its end indicator. The sole teardown point:
    /// clears `running`/`turn_started`/`pending` and pushes one labelled,
    /// tinted system line saying *why* the turn stopped — a clean finish, the
    /// iteration-cap cutoff, a user interruption, or an error — with the elapsed
    /// time and tool-call count. `Compacted` posts no line (the shrink note from
    /// the `Compacted` event already covers it). Idempotent: a second call (the
    /// `TurnDone`/`TurnEnded` pair can't both reach here, but a stray repeat) is
    /// harmless because `turn_started` is already cleared.
    fn end_turn(&mut self, reason: TurnEndReason) {
        let secs = self.turn_elapsed_secs().unwrap_or(0);
        let calls = Self::count_phrase(self.turn_tool_calls, "tool call");
        self.running = false;
        self.turn_started = None;
        self.pending = None;

        let entry = match reason {
            TurnEndReason::Compacted => None,
            TurnEndReason::Completed => Some(Entry::notice(
                NoticeLevel::Success,
                format!("✓ Cogitated for {secs}s · {calls}"),
            )),
            TurnEndReason::IterationCap => Some(Entry::notice(
                NoticeLevel::Warn,
                format!(
                    "⚠ Hit the per-turn step limit after {secs}s · {calls} \
                     — send \"continue\" to resume"
                ),
            )),
            TurnEndReason::Interrupted => Some(Entry::notice(
                NoticeLevel::Plain,
                format!("⊘ Interrupted after {secs}s · {calls}"),
            )),
            TurnEndReason::Error(e) => Some(Entry::notice(
                NoticeLevel::Error,
                format!("✗ Stopped after {secs}s — {e}"),
            )),
        };
        if let Some(entry) = entry {
            self.transcript.push(entry);
        }
    }

    /// `"1 tool call"` / `"3 tool calls"` — pluralize a count for the indicator.
    fn count_phrase(n: usize, noun: &str) -> String {
        if n == 1 {
            format!("1 {noun}")
        } else {
            format!("{n} {noun}s")
        }
    }

    /// Seconds the in-flight turn has been running, if any.
    pub fn turn_elapsed_secs(&self) -> Option<u64> {
        self.turn_started.map(|t| t.elapsed().as_secs())
    }

    /// Output tokens streamed during the current turn (for the running line).
    pub fn turn_output_tokens(&self) -> u64 {
        self.status
            .total_output_tokens
            .saturating_sub(self.turn_output_base)
    }

    // ── input-line editing helpers ────────────────────────────────────

    fn char_len(&self) -> usize {
        self.input.chars().count()
    }

    /// Byte offset of the `n`th char (or the end of the string).
    fn byte_at(&self, n: usize) -> usize {
        self.input
            .char_indices()
            .nth(n)
            .map(|(i, _)| i)
            .unwrap_or(self.input.len())
    }

    fn insert_char(&mut self, c: char) {
        let at = self.byte_at(self.cursor);
        self.input.insert(at, c);
        self.cursor += 1;
    }

    /// Ctrl-W: delete trailing spaces before the cursor, then the word.
    fn kill_word_back(&mut self) {
        let chars: Vec<char> = self.input.chars().collect();
        let mut new_cursor = self.cursor;
        while new_cursor > 0 && chars[new_cursor - 1].is_whitespace() {
            new_cursor -= 1;
        }
        while new_cursor > 0 && !chars[new_cursor - 1].is_whitespace() {
            new_cursor -= 1;
        }
        let from = self.byte_at(new_cursor);
        let to = self.byte_at(self.cursor);
        self.input.drain(from..to);
        self.cursor = new_cursor;
    }

    fn clear_input(&mut self) {
        self.input.clear();
        self.cursor = 0;
        self.history_pos = None;
    }

    /// Up: step back through history, stashing the live draft first.
    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let pos = match self.history_pos {
            None => {
                self.draft = std::mem::take(&mut self.input);
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(p) => p - 1,
        };
        self.history_pos = Some(pos);
        self.input = self.history[pos].clone();
        self.cursor = self.char_len();
    }

    /// Down: step forward; past the newest entry, restore the live draft.
    fn history_next(&mut self) {
        let Some(pos) = self.history_pos else {
            return;
        };
        if pos + 1 < self.history.len() {
            self.history_pos = Some(pos + 1);
            self.input = self.history[pos + 1].clone();
        } else {
            self.history_pos = None;
            self.input = std::mem::take(&mut self.draft);
        }
        self.cursor = self.char_len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::TurnOutcomeSummary;
    use serde_json::json;

    fn app() -> App {
        App::new(Status::new(
            "bedrock",
            "us.anthropic.claude-opus-4-5-20251101-v1:0",
            "/tmp/p/design.kicad_sch",
            true,
        ))
    }

    fn type_str(a: &mut App, s: &str) {
        for c in s.chars() {
            a.update(Msg::Char(c));
        }
    }

    fn dry_run_json() -> Value {
        json!({
            "ok": true,
            "would_write": true,
            "diff": {
                "added": ["U1", "R7"],
                "removed": [],
                "changed": ["C2"],
                "nets_before": 3,
                "nets_after": 12
            }
        })
    }

    #[test]
    fn typing_builds_the_input_line() {
        let mut a = app();
        type_str(&mut a, "hello");
        assert_eq!(a.input, "hello");
        a.update(Msg::Backspace);
        assert_eq!(a.input, "hell");
    }

    #[test]
    fn cursor_movement_edits_in_the_middle() {
        let mut a = app();
        type_str(&mut a, "ac");
        a.update(Msg::CursorLeft);
        a.update(Msg::Char('b'));
        assert_eq!(a.input, "abc");
        assert_eq!(a.cursor, 2);
        a.update(Msg::Home);
        a.update(Msg::Delete);
        assert_eq!(a.input, "bc");
        a.update(Msg::End);
        a.update(Msg::Backspace);
        assert_eq!(a.input, "b");
    }

    #[test]
    fn cursor_handles_multibyte_chars() {
        let mut a = app();
        type_str(&mut a, "héllo");
        a.update(Msg::Home);
        a.update(Msg::CursorRight);
        a.update(Msg::CursorRight);
        a.update(Msg::Backspace); // removes the é
        assert_eq!(a.input, "hllo");
    }

    #[test]
    fn ctrl_u_kills_to_line_start() {
        let mut a = app();
        type_str(&mut a, "abc def");
        a.update(Msg::CursorLeft); // cursor between "de" and "f"
        a.update(Msg::KillToStart);
        assert_eq!(a.input, "f");
        assert_eq!(a.cursor, 0);
    }

    #[test]
    fn ctrl_w_kills_the_previous_word() {
        let mut a = app();
        type_str(&mut a, "add a resistor  ");
        a.update(Msg::KillWordBack);
        assert_eq!(a.input, "add a ");
        a.update(Msg::KillWordBack);
        assert_eq!(a.input, "add ");
    }

    #[test]
    fn history_recall_round_trips() {
        let mut a = app();
        type_str(&mut a, "first");
        a.update(Msg::Submit);
        a.update(Msg::TurnEnded(TurnEndReason::Completed));
        type_str(&mut a, "second");
        a.update(Msg::Submit);
        a.update(Msg::TurnEnded(TurnEndReason::Completed));

        type_str(&mut a, "draft");
        a.update(Msg::HistoryPrev);
        assert_eq!(a.input, "second");
        a.update(Msg::HistoryPrev);
        assert_eq!(a.input, "first");
        a.update(Msg::HistoryPrev); // already at the oldest — stays
        assert_eq!(a.input, "first");
        a.update(Msg::HistoryNext);
        assert_eq!(a.input, "second");
        a.update(Msg::HistoryNext); // past the newest — the draft returns
        assert_eq!(a.input, "draft");
    }

    #[test]
    fn history_skips_consecutive_duplicates() {
        let mut a = app();
        type_str(&mut a, "same");
        a.update(Msg::Submit);
        a.update(Msg::TurnEnded(TurnEndReason::Completed));
        type_str(&mut a, "same");
        a.update(Msg::Submit);
        assert_eq!(a.history, vec!["same"]);
    }

    #[test]
    fn submitting_a_prompt_enqueues_a_turn() {
        let mut a = app();
        type_str(&mut a, "design a board");
        let action = a.update(Msg::Submit);
        assert_eq!(action, Action::SpawnTurn("design a board".to_string()));
        assert!(a.running, "submitting should mark the turn running");
        assert!(a.turn_started.is_some(), "elapsed clock starts");
        assert_eq!(a.status.turn_count, 1);
        assert!(a.input.is_empty(), "input clears on submit");
        // The user message is recorded in the transcript.
        assert!(
            a.transcript
                .iter()
                .any(|e| e.speaker == Speaker::User && e.text == "design a board")
        );
    }

    #[test]
    fn empty_submit_does_nothing() {
        let mut a = app();
        assert_eq!(a.update(Msg::Submit), Action::None);
        assert!(!a.running);
    }

    #[test]
    fn typing_while_running_drafts_but_cannot_submit() {
        let mut a = app();
        type_str(&mut a, "first");
        a.update(Msg::Submit);
        assert!(a.running);
        // Drafting the next prompt while the agent works is allowed…
        type_str(&mut a, "second");
        assert_eq!(a.input, "second");
        // …but submitting it is not; the draft is kept.
        assert_eq!(a.update(Msg::Submit), Action::None);
        assert_eq!(a.input, "second");
        assert_eq!(a.status.turn_count, 1);
    }

    #[test]
    fn commands_still_work_while_running() {
        let mut a = app();
        type_str(&mut a, "go");
        a.update(Msg::Submit);
        type_str(&mut a, "/help");
        a.update(Msg::Submit);
        assert!(a.help, "/help should toggle even mid-turn");
    }

    #[test]
    fn auto_command_toggles_the_gate_flag() {
        let mut a = app();
        assert!(!a.auto);
        type_str(&mut a, "/auto");
        assert_eq!(a.update(Msg::Submit), Action::None);
        assert!(a.auto, "/auto should toggle the flag ON");
        type_str(&mut a, "/auto");
        a.update(Msg::Submit);
        assert!(!a.auto, "/auto again toggles it OFF");
    }

    #[test]
    fn clear_command_resets_transcript_and_requests_context_clear() {
        let mut a = app();
        type_str(&mut a, "hello");
        a.update(Msg::Submit);
        a.update(Msg::TurnEnded(TurnEndReason::Completed));
        type_str(&mut a, "/clear");
        assert_eq!(a.update(Msg::Submit), Action::ClearContext);
        assert!(
            a.transcript.is_empty(),
            "transcript wiped; shell adds the note"
        );
        assert_eq!(a.scroll, 0);
    }

    #[test]
    fn colon_commands_get_a_migration_hint() {
        let mut a = app();
        type_str(&mut a, ":help");
        assert_eq!(a.update(Msg::Submit), Action::None);
        assert!(!a.help, "the old prefix must not run the command");
        assert!(
            a.transcript
                .iter()
                .any(|e| e.text.contains("commands now start with /")),
            "{:?}",
            a.transcript
        );
    }

    #[test]
    fn quit_is_command_or_ctrl_c_but_not_esc() {
        let mut a = app();
        type_str(&mut a, "/quit");
        assert_eq!(a.update(Msg::Submit), Action::Quit);
        assert!(a.should_quit);

        let mut b = app();
        assert_eq!(b.update(Msg::Cancel), Action::None, "first idle Esc arms");
        assert!(!b.should_quit, "Esc never quits");
    }

    #[test]
    fn esc_clears_a_nonempty_input_then_arms_unwind() {
        let mut a = app();
        type_str(&mut a, "half-typed");
        assert_eq!(a.update(Msg::Cancel), Action::None);
        assert!(a.input.is_empty());
        assert!(!a.esc_armed, "clearing the input is its own Esc step");
        assert_eq!(a.update(Msg::Cancel), Action::None);
        assert!(a.esc_armed, "second Esc arms the unwind");
        assert_eq!(
            a.update(Msg::Cancel),
            Action::OpenUnwind,
            "third Esc opens the picker"
        );
        assert!(!a.esc_armed, "the unwind consumed the arming");
        assert!(!a.should_quit);
    }

    #[test]
    fn typing_disarms_a_pending_unwind() {
        let mut a = app();
        a.update(Msg::Cancel);
        assert!(a.esc_armed);
        a.update(Msg::Char('x'));
        assert!(!a.esc_armed, "any user action disarms");
        // Ticks and agent events must NOT disarm (they arrive on their own).
        a.update(Msg::Cancel);
        a.update(Msg::Cancel);
        let mut b = app();
        b.update(Msg::Cancel);
        b.update(Msg::Tick);
        assert!(b.esc_armed, "ticks don't disarm");
    }

    #[test]
    fn apply_unwind_to_rolls_the_transcript_back_to_before_the_user_turn() {
        let mut a = app();
        type_str(&mut a, "build it");
        a.update(Msg::Submit);
        a.update(Msg::Agent(AgentEvent::AssistantText("working".into())));
        a.update(Msg::TurnEnded(TurnEndReason::Completed));
        let turns_before = a.status.turn_count;

        a.apply_unwind_to(1);
        assert!(
            !a.transcript
                .iter()
                .any(|e| e.speaker == Speaker::User && e.text == "build it"),
            "user entry removed: {:?}",
            a.transcript
        );
        assert!(
            !a.transcript.iter().any(|e| e.text == "working"),
            "assistant reply removed too"
        );
        assert!(
            a.transcript.iter().any(|e| e.text.contains("unwound")),
            "confirmation note shown"
        );
        assert_eq!(a.status.turn_count, turns_before - 1);

        let len = a.transcript.len();
        a.apply_unwind_to(0);
        assert!(
            a.transcript[len..]
                .iter()
                .any(|e| e.text.contains("nothing"))
        );
    }

    #[test]
    fn apply_unwind_to_rolls_back_multiple_turns_at_once() {
        let mut a = app();
        for prompt in ["first", "second", "third"] {
            type_str(&mut a, prompt);
            a.update(Msg::Submit);
            a.update(Msg::Agent(AgentEvent::AssistantText(format!("re: {prompt}"))));
            a.update(Msg::TurnEnded(TurnEndReason::Completed));
        }
        assert_eq!(a.status.turn_count, 3);

        // Pick the 2nd-newest prompt ("second"): drop it and "third".
        a.apply_unwind_to(2);
        assert!(
            a.transcript.iter().any(|e| e.text == "first"),
            "the kept turn survives: {:?}",
            a.transcript
        );
        assert!(
            !a.transcript
                .iter()
                .any(|e| e.text == "second" || e.text == "third"),
            "the selected turn and everything after are gone: {:?}",
            a.transcript
        );
        assert_eq!(a.status.turn_count, 1);
        assert!(a.transcript.iter().any(|e| e.text.contains("2 turns")));
    }

    #[test]
    fn unwind_picker_opens_navigates_and_confirms() {
        let mut a = app();
        // Newest-first prompts, as the agent would report them.
        a.open_unwind(vec![
            "swap the regulator".into(),
            "add usb-c".into(),
            "make the board".into(),
        ]);
        let p = a.unwind.as_ref().expect("picker open");
        assert_eq!(p.selected, 0, "latest turn preselected");

        // Down moves toward older turns; up clamps back at the top.
        a.update(Msg::HistoryNext);
        a.update(Msg::HistoryNext);
        assert_eq!(a.unwind.as_ref().unwrap().selected, 2);
        a.update(Msg::HistoryNext); // clamps at the last row
        assert_eq!(a.unwind.as_ref().unwrap().selected, 2);
        a.update(Msg::HistoryPrev);
        assert_eq!(a.unwind.as_ref().unwrap().selected, 1);

        // Enter confirms: drop selected + 1 = 2 turns, picker closes.
        assert_eq!(a.update(Msg::Submit), Action::UnwindTo(2));
        assert!(a.unwind.is_none(), "confirm closes the picker");
    }

    #[test]
    fn unwind_picker_esc_cancels_without_acting() {
        let mut a = app();
        a.open_unwind(vec!["a".into(), "b".into()]);
        assert!(a.unwind.is_some());
        assert_eq!(a.update(Msg::Cancel), Action::None);
        assert!(a.unwind.is_none(), "Esc closes the picker, no unwind");
    }

    #[test]
    fn unwind_picker_is_empty_when_nothing_to_unwind() {
        let mut a = app();
        a.open_unwind(vec![]);
        assert!(a.unwind.is_none(), "no picker for an empty list");
        assert!(a.transcript.iter().any(|e| e.text.contains("nothing")));
    }

    #[test]
    fn compact_command_spins_like_a_turn() {
        let mut a = app();
        type_str(&mut a, "/compact");
        assert_eq!(a.update(Msg::Submit), Action::Compact);
        assert!(a.running, "compaction shows the working spinner");
        a.update(Msg::TurnEnded(TurnEndReason::Compacted));
        assert!(!a.running);
        assert!(
            !a.transcript.iter().any(|e| e.text.contains("Cogitated")),
            "compaction posts no end indicator (it has its own shrink note)"
        );
    }

    #[test]
    fn compact_while_running_is_refused() {
        let mut a = app();
        type_str(&mut a, "go");
        a.update(Msg::Submit);
        type_str(&mut a, "/compact");
        assert_eq!(a.update(Msg::Submit), Action::None);
        assert!(
            a.transcript
                .iter()
                .any(|e| e.text.contains("can't compact")),
            "{:?}",
            a.transcript
        );
    }

    #[test]
    fn context_command_requests_stats() {
        let mut a = app();
        type_str(&mut a, "/context");
        assert_eq!(a.update(Msg::Submit), Action::ShowContext);
    }

    #[test]
    fn tab_cycles_through_matching_commands() {
        let mut a = app();
        type_str(&mut a, "/c");
        let (matches, idx) = a.completion_view().expect("matches for /c");
        let names: Vec<&str> = matches.iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["/clear", "/context", "/compact"]);
        assert_eq!(idx, None, "nothing highlighted before the first Tab");

        a.update(Msg::Complete);
        assert_eq!(a.input, "/clear");
        a.update(Msg::Complete);
        assert_eq!(a.input, "/context", "Tab cycles against the typed stem");
        a.update(Msg::Complete);
        assert_eq!(a.input, "/compact");
        a.update(Msg::Complete);
        assert_eq!(a.input, "/clear", "cycling wraps");

        // Typing again resets the cycle to the new stem.
        a.update(Msg::Backspace);
        assert!(a.completion_view().is_some());
        assert_eq!(a.completion_idx, None, "edit resets the cycle");
    }

    #[test]
    fn completion_does_not_apply_to_prompts_or_arguments() {
        let mut a = app();
        type_str(&mut a, "hello");
        assert!(a.completion_view().is_none());
        a.update(Msg::Complete);
        assert_eq!(a.input, "hello", "Tab is inert outside / commands");

        let mut b = app();
        type_str(&mut b, "/clear now");
        assert!(b.completion_view().is_none(), "no completion after a space");
    }

    #[test]
    fn usage_events_update_token_status() {
        let mut a = app();
        a.update(Msg::Agent(AgentEvent::Usage {
            input_tokens: 1000,
            output_tokens: 200,
        }));
        a.update(Msg::Agent(AgentEvent::Usage {
            input_tokens: 1500,
            output_tokens: 300,
        }));
        assert_eq!(a.status.ctx_tokens, 1800, "latest call defines the context");
        assert_eq!(a.status.total_input_tokens, 2500);
        assert_eq!(a.status.total_output_tokens, 500);
    }

    #[test]
    fn compacted_event_notes_the_shrink() {
        let mut a = app();
        a.update(Msg::Agent(AgentEvent::Compacted {
            messages_before: 24,
            messages_after: 2,
        }));
        assert!(
            a.transcript.iter().any(|e| e.text.contains("24 → 2")),
            "{:?}",
            a.transcript
        );
    }

    #[test]
    fn esc_cancels_a_running_turn() {
        let mut a = app();
        type_str(&mut a, "go");
        a.update(Msg::Submit);
        assert!(a.running);
        assert_eq!(a.update(Msg::Cancel), Action::CancelTurn);
        assert!(!a.should_quit, "cancelling a turn must not quit");
        // Esc itself posts no note now; the shell confirms the abort by sending
        // TurnEnded(Interrupted), which posts the interruption indicator.
        a.update(Msg::TurnEnded(TurnEndReason::Interrupted));
        assert!(!a.running);
        assert!(
            a.transcript
                .iter()
                .any(|e| e.text.contains("Interrupted") && e.level == NoticeLevel::Plain),
            "interruption indicator posted: {:?}",
            a.transcript
        );
    }

    #[test]
    fn ctrl_c_force_quits_even_mid_turn() {
        let mut a = app();
        type_str(&mut a, "go");
        a.update(Msg::Submit);
        assert_eq!(a.update(Msg::ForceQuit), Action::Quit);
        assert!(a.should_quit);
    }

    #[test]
    fn tick_advances_the_spinner_only_while_running() {
        let mut a = app();
        a.update(Msg::Tick);
        assert_eq!(a.spinner, 0);
        type_str(&mut a, "go");
        a.update(Msg::Submit);
        a.update(Msg::Tick);
        a.update(Msg::Tick);
        assert_eq!(a.spinner, 2);
    }

    #[test]
    fn undo_command_returns_undo_action() {
        let mut a = app();
        type_str(&mut a, "/undo");
        assert_eq!(a.update(Msg::Submit), Action::Undo);
    }

    #[test]
    fn help_command_toggles_help_and_esc_dismisses() {
        let mut a = app();
        type_str(&mut a, "/help");
        a.update(Msg::Submit);
        assert!(a.help);
        // Esc dismisses help rather than quitting.
        assert_eq!(a.update(Msg::Cancel), Action::None);
        assert!(!a.help);
        assert!(!a.should_quit);
    }

    #[test]
    fn pending_diff_arrives_and_approve_resolves_it() {
        let mut a = app();
        a.update(Msg::PendingDiff(dry_run_json()));
        let pending = a.pending.as_ref().expect("diff is pending");
        assert_eq!(pending.added, vec!["U1", "R7"]);
        assert_eq!(pending.changed, vec!["C2"]);
        assert_eq!(pending.nets_after, 12);
        assert!(!a.input_active(), "input locked while a gate is open");

        // Pressing 'a' resolves approval and clears the pending diff.
        let action = a.update(Msg::Char('a'));
        assert_eq!(action, Action::ResolveApproval(true));
        assert!(a.pending.is_none());
    }

    #[test]
    fn reject_key_resolves_false() {
        let mut a = app();
        a.update(Msg::PendingDiff(dry_run_json()));
        let action = a.update(Msg::Char('r'));
        assert_eq!(action, Action::ResolveApproval(false));
        assert!(a.pending.is_none());
    }

    #[test]
    fn other_chars_do_not_leak_into_input_while_gate_open() {
        let mut a = app();
        a.update(Msg::PendingDiff(dry_run_json()));
        a.update(Msg::Char('x'));
        assert!(a.input.is_empty(), "gate keys only while pending");
    }

    #[test]
    fn resolving_with_nothing_pending_is_a_noop() {
        let mut a = app();
        assert_eq!(a.update(Msg::Approve), Action::None);
    }

    #[test]
    fn tool_finished_event_appends_or_replaces_a_card() {
        let mut a = app();
        a.update(Msg::Agent(AgentEvent::ToolStarted {
            name: "search_symbols".into(),
        }));
        assert!(
            a.transcript
                .iter()
                .any(|e| e.speaker == Speaker::Tool && e.text.contains("running"))
        );
        a.update(Msg::Agent(AgentEvent::ToolFinished {
            name: "search_symbols".into(),
            summary: "\"STM32\" → 4 hits".into(),
        }));
        // The running placeholder is replaced in place by the finished card.
        let cards: Vec<&Entry> = a
            .transcript
            .iter()
            .filter(|e| e.speaker == Speaker::Tool)
            .collect();
        assert_eq!(cards.len(), 1, "the card collapses in place");
        assert!(cards[0].text.contains("→ \"STM32\" → 4 hits"));
    }

    #[test]
    fn applied_event_bumps_count_and_notes_erc() {
        let mut a = app();
        a.update(Msg::Agent(AgentEvent::Applied {
            errors: 0,
            warnings: 2,
        }));
        assert_eq!(a.status.applied_count, 1);
        assert!(
            a.transcript
                .iter()
                .any(|e| e.text.contains("ERC 0 errors, 2 warnings"))
        );
    }

    #[test]
    fn turn_done_stops_spinner_but_keeps_the_clock_for_turn_ended() {
        // TurnDone now only stops the spinner; it leaves `turn_started` intact so
        // the following TurnEnded can read the elapsed time. TurnEnded owns the
        // rest of teardown.
        let mut a = app();
        a.running = true;
        a.turn_started = Some(Instant::now());
        a.update(Msg::Agent(AgentEvent::TurnDone(TurnOutcomeSummary {
            applied: true,
            tool_calls_made: 3,
            final_text: "done".into(),
        })));
        assert!(!a.running, "spinner stops");
        assert!(
            a.turn_started.is_some(),
            "the clock survives until TurnEnded reads it"
        );

        a.update(Msg::TurnEnded(TurnEndReason::Completed));
        assert!(a.turn_started.is_none(), "TurnEnded clears the clock");
    }

    #[test]
    fn turn_ended_posts_a_labelled_indicator_per_reason() {
        // Completed → green "Cogitated", with a pluralized tool-call count.
        let mut a = app();
        type_str(&mut a, "go");
        a.update(Msg::Submit);
        a.update(Msg::Agent(AgentEvent::ToolStarted {
            name: "get_design".into(),
        }));
        a.update(Msg::TurnEnded(TurnEndReason::Completed));
        let last = a.transcript.last().unwrap();
        assert!(last.text.contains("Cogitated"), "{}", last.text);
        assert!(last.text.contains("1 tool call"), "singular: {}", last.text);
        assert_eq!(last.level, NoticeLevel::Success);
        assert!(!a.running && a.turn_started.is_none());

        // IterationCap → yellow warning with the resume hint.
        let mut a = app();
        type_str(&mut a, "go");
        a.update(Msg::Submit);
        a.update(Msg::TurnEnded(TurnEndReason::IterationCap));
        let last = a.transcript.last().unwrap();
        assert!(last.text.contains("step limit"), "{}", last.text);
        assert!(last.text.contains("continue"), "resume hint: {}", last.text);
        assert!(last.text.contains("0 tool calls"), "plural: {}", last.text);
        assert_eq!(last.level, NoticeLevel::Warn);

        // Error → red, carries the message.
        let mut a = app();
        type_str(&mut a, "go");
        a.update(Msg::Submit);
        a.update(Msg::TurnEnded(TurnEndReason::Error("throttled".into())));
        let last = a.transcript.last().unwrap();
        assert!(last.text.contains("throttled"), "{}", last.text);
        assert_eq!(last.level, NoticeLevel::Error);
    }

    #[test]
    fn turn_done_then_turn_ended_posts_exactly_one_indicator() {
        // The pair can arrive in either select order; only TurnEnded posts, so
        // there is never a double line nor a lost clock.
        for done_first in [true, false] {
            let mut a = app();
            type_str(&mut a, "go");
            a.update(Msg::Submit);
            let done = Msg::Agent(AgentEvent::TurnDone(TurnOutcomeSummary {
                applied: false,
                tool_calls_made: 0,
                final_text: "ok".into(),
            }));
            if done_first {
                a.update(done);
                a.update(Msg::TurnEnded(TurnEndReason::Completed));
            } else {
                a.update(Msg::TurnEnded(TurnEndReason::Completed));
                a.update(done);
            }
            let indicators = a
                .transcript
                .iter()
                .filter(|e| e.text.contains("Cogitated"))
                .count();
            assert_eq!(indicators, 1, "exactly one indicator (done_first={done_first})");
        }
    }

    #[test]
    fn assistant_text_appends_to_transcript() {
        let mut a = app();
        a.update(Msg::Agent(AgentEvent::AssistantText(
            "I'll search for the part.".into(),
        )));
        assert!(
            a.transcript
                .iter()
                .any(|e| e.speaker == Speaker::Assistant && e.text.contains("search"))
        );
    }
}
