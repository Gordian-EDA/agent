//! The [`App`] struct itself, its [`Status`] sidebar data, and the turn-lifecycle
//! helpers that bracket an in-flight turn (begin/end + the elapsed/token readouts
//! the renderer shows).

use std::time::Instant;

use super::{Entry, NoticeLevel, PendingDiff, TurnEndReason, UnwindPicker};

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
    pub(super) draft: String,
    /// First idle Esc pressed: the next Esc unwinds the last turn. Any other
    /// user action disarms.
    pub esc_armed: bool,
    /// The typed `/`-prefix Tab completion is cycling against (the input
    /// itself once Tab starts rewriting it no longer matches).
    pub(super) completion_stem: Option<String>,
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
    /// Animation frame counter, advanced by [`super::Msg::Tick`] while running.
    pub spinner: usize,
    /// The unwind picker, while the user is choosing how far to roll back.
    pub unwind: Option<UnwindPicker>,
    /// Whether `:help` is showing.
    pub help: bool,
    /// Lines scrolled up from the bottom of the transcript (0 = follow tail).
    /// The renderer clamps this to the real maximum for the viewport.
    pub scroll: u16,
    /// The transcript viewport height the renderer last drew, so a PgUp/PgDn
    /// can jump by a screenful. The renderer writes it; `map_key` reads it.
    pub viewport_h: u16,
    /// Status-bar data.
    pub status: Status,
    /// Set once the user asks to quit; the shell's loop exits.
    pub should_quit: bool,
}

impl App {
    /// Build a fresh cockpit over a project.
    pub fn new(status: Status) -> Self {
        // The empty transcript renders a welcome splash (see `ui::draw_welcome`),
        // so no seed entry is needed.
        Self {
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
            viewport_h: 0,
            status,
            should_quit: false,
        }
    }

    /// Whether keystrokes currently edit the input line (only an open
    /// apply-gate takes the keyboard away; typing during a turn is fine).
    pub fn input_active(&self) -> bool {
        self.pending.is_none()
    }

    /// Mark a turn (a prompt or `/compact`) as started: spin up the running
    /// flag, the elapsed clock, the per-turn token baseline, and follow the tail.
    pub(super) fn begin_turn(&mut self) {
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
    pub(super) fn end_turn(&mut self, reason: TurnEndReason) {
        let secs = self.turn_elapsed_secs().unwrap_or(0);
        let calls = Self::count_phrase(self.turn_tool_calls, "tool call");
        self.running = false;
        self.turn_started = None;
        self.pending = None;

        // The level glyph is the renderer's job (it tints the whole notice as a
        // callout); the text carries none, or each line would show two markers.
        let entry = match reason {
            TurnEndReason::Compacted => None,
            TurnEndReason::Completed => Some(Entry::notice(
                NoticeLevel::Success,
                format!("Cogitated for {secs}s · {calls}"),
            )),
            TurnEndReason::IterationCap => Some(Entry::notice(
                NoticeLevel::Warn,
                format!(
                    "Hit the per-turn step limit after {secs}s · {calls} \
                     — send \"continue\" to resume"
                ),
            )),
            TurnEndReason::Interrupted => Some(Entry::notice(
                NoticeLevel::Plain,
                format!("Interrupted after {secs}s · {calls}"),
            )),
            TurnEndReason::Error(e) => Some(Entry::notice(
                NoticeLevel::Error,
                format!("Stopped after {secs}s — {e}"),
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
}
