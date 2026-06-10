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

/// One line in the chat transcript.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub speaker: Speaker,
    pub text: String,
}

impl Entry {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            speaker: Speaker::User,
            text: text.into(),
        }
    }
    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            speaker: Speaker::Assistant,
            text: text.into(),
        }
    }
    pub fn tool(text: impl Into<String>) -> Self {
        Self {
            speaker: Speaker::Tool,
            text: text.into(),
        }
    }
    pub fn system(text: impl Into<String>) -> Self {
        Self {
            speaker: Speaker::System,
            text: text.into(),
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
    /// Enter — submit the input line (a prompt or a `:command`).
    Submit,
    /// Approve the pending diff (`a`).
    Approve,
    /// Reject the pending diff (`r`).
    Reject,
    /// Scroll the transcript up / down by one line.
    ScrollUp,
    ScrollDown,
    /// Esc — close help / reject a gate / clear input / cancel a turn / quit,
    /// in that order of precedence.
    Cancel,
    /// Ctrl-C — quit unconditionally.
    ForceQuit,
    /// A periodic animation tick from the shell (advances the spinner).
    Tick,
    /// An event from the running agent turn.
    Agent(AgentEvent),
    /// The apply-gate fired: a dry-run diff awaits a decision.
    PendingDiff(Value),
    /// A turn finished (the spawned task joined). Clears the running flag even
    /// if no `TurnDone` event arrived (e.g. the turn errored).
    TurnEnded(Option<String>),
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
    /// Tear down the TUI and exit.
    Quit,
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
    /// Apply-gate mode: when `false` (default) every write needs approval; when
    /// `true` (`:auto`) writes commit without a prompt.
    pub auto: bool,
    /// A change awaiting approval, if any. While `Some`, `a`/`r` resolve it.
    pub pending: Option<PendingDiff>,
    /// Whether an agent turn is in flight (submit is blocked, typing is not).
    pub running: bool,
    /// When the in-flight turn started (drives the elapsed display).
    pub turn_started: Option<Instant>,
    /// Animation frame counter, advanced by [`Msg::Tick`] while running.
    pub spinner: usize,
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
            auto: false,
            pending: None,
            running: false,
            turn_started: None,
            spinner: 0,
            help: false,
            scroll: 0,
            status,
            should_quit: false,
        };
        app.transcript.push(Entry::system(
            "auto-pcb copilot. Type a prompt and Enter. :help for commands.",
        ));
        app
    }

    /// Apply one message, mutating state and returning the shell's next action.
    pub fn update(&mut self, msg: Msg) -> Action {
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
            Msg::TurnEnded(err) => {
                self.running = false;
                self.turn_started = None;
                self.pending = None;
                if let Some(e) = err {
                    self.transcript
                        .push(Entry::system(format!("turn error: {e}")));
                }
                Action::None
            }
        }
    }

    /// Esc, layered: close help → reject the gate → clear a non-empty input →
    /// cancel a running turn → quit.
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
            self.transcript.push(Entry::system("turn cancelled"));
            Action::CancelTurn
        } else {
            self.should_quit = true;
            Action::Quit
        }
    }

    /// Submit the input line: a `:command` or a prompt.
    fn submit(&mut self) -> Action {
        let line = self.input.trim().to_string();
        if line.is_empty() {
            return Action::None;
        }
        if let Some(cmd) = line.strip_prefix(':') {
            self.clear_input();
            return self.run_command(cmd);
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
        self.running = true;
        self.turn_started = Some(Instant::now());
        self.scroll = 0;
        Action::SpawnTurn(line)
    }

    /// Run a `:command`.
    fn run_command(&mut self, cmd: &str) -> Action {
        match cmd.trim() {
            "auto" => {
                self.auto = !self.auto;
                let state = if self.auto { "ON (yolo)" } else { "OFF" };
                self.transcript
                    .push(Entry::system(format!("apply-gate auto-approve: {state}")));
                Action::None
            }
            "undo" => {
                if self.running {
                    self.transcript
                        .push(Entry::system("can't undo while a turn is running"));
                    Action::None
                } else {
                    Action::Undo
                }
            }
            "clear" => {
                self.transcript.clear();
                self.scroll = 0;
                self.transcript.push(Entry::system("transcript cleared"));
                Action::None
            }
            "help" => {
                self.help = !self.help;
                Action::None
            }
            "quit" | "q" => {
                self.should_quit = true;
                Action::Quit
            }
            other => {
                self.transcript
                    .push(Entry::system(format!("unknown command :{other}")));
                Action::None
            }
        }
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
            AgentEvent::TurnDone(_) => {
                self.running = false;
                self.turn_started = None;
            }
        }
    }

    /// Whether keystrokes currently edit the input line (only an open
    /// apply-gate takes the keyboard away; typing during a turn is fine).
    pub fn input_active(&self) -> bool {
        self.pending.is_none()
    }

    /// Seconds the in-flight turn has been running, if any.
    pub fn turn_elapsed_secs(&self) -> Option<u64> {
        self.turn_started.map(|t| t.elapsed().as_secs())
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
        a.update(Msg::TurnEnded(None));
        type_str(&mut a, "second");
        a.update(Msg::Submit);
        a.update(Msg::TurnEnded(None));

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
        a.update(Msg::TurnEnded(None));
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
        type_str(&mut a, ":help");
        a.update(Msg::Submit);
        assert!(a.help, ":help should toggle even mid-turn");
    }

    #[test]
    fn auto_command_toggles_the_gate_flag() {
        let mut a = app();
        assert!(!a.auto);
        type_str(&mut a, ":auto");
        assert_eq!(a.update(Msg::Submit), Action::None);
        assert!(a.auto, ":auto should toggle the flag ON");
        type_str(&mut a, ":auto");
        a.update(Msg::Submit);
        assert!(!a.auto, ":auto again toggles it OFF");
    }

    #[test]
    fn clear_command_resets_the_transcript() {
        let mut a = app();
        type_str(&mut a, "hello");
        a.update(Msg::Submit);
        a.update(Msg::TurnEnded(None));
        type_str(&mut a, ":clear");
        a.update(Msg::Submit);
        assert_eq!(a.transcript.len(), 1, "only the 'cleared' note remains");
        assert_eq!(a.scroll, 0);
    }

    #[test]
    fn quit_command_and_esc_quit() {
        let mut a = app();
        type_str(&mut a, ":quit");
        assert_eq!(a.update(Msg::Submit), Action::Quit);
        assert!(a.should_quit);

        let mut b = app();
        assert_eq!(b.update(Msg::Cancel), Action::Quit);
        assert!(b.should_quit);
    }

    #[test]
    fn esc_clears_a_nonempty_input_before_quitting() {
        let mut a = app();
        type_str(&mut a, "half-typed");
        assert_eq!(a.update(Msg::Cancel), Action::None);
        assert!(a.input.is_empty());
        assert!(!a.should_quit, "first Esc only clears the line");
        assert_eq!(a.update(Msg::Cancel), Action::Quit);
    }

    #[test]
    fn esc_cancels_a_running_turn() {
        let mut a = app();
        type_str(&mut a, "go");
        a.update(Msg::Submit);
        assert!(a.running);
        assert_eq!(a.update(Msg::Cancel), Action::CancelTurn);
        assert!(!a.should_quit, "cancelling a turn must not quit");
        // The shell confirms the abort by sending TurnEnded.
        a.update(Msg::TurnEnded(None));
        assert!(!a.running);
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
        type_str(&mut a, ":undo");
        assert_eq!(a.update(Msg::Submit), Action::Undo);
    }

    #[test]
    fn help_command_toggles_help_and_esc_dismisses() {
        let mut a = app();
        type_str(&mut a, ":help");
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
    fn turn_done_clears_running() {
        let mut a = app();
        a.running = true;
        a.turn_started = Some(Instant::now());
        a.update(Msg::Agent(AgentEvent::TurnDone(TurnOutcomeSummary {
            applied: true,
            tool_calls_made: 3,
            final_text: "done".into(),
        })));
        assert!(!a.running);
        assert!(a.turn_started.is_none());
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
