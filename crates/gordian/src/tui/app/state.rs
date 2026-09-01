//! The [`App`] struct itself, its [`Status`] sidebar data, and the turn-lifecycle
//! helpers that bracket an in-flight turn.

use std::time::{Duration, Instant};

use super::{Entry, LiveAssistant, NoticeLevel, PendingApproval, TurnEndReason, UnwindPicker};
use crate::tui::pricing::Ledger;

#[derive(Clone, Debug)]
pub(super) struct StashedPaste {
    pub token: String,
    pub text: String,
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
    /// Live context size: prompt tokens of the latest model call (system +
    /// history + tools), plus its output — what the *next* call will roughly
    /// resend. 0 until the first call reports usage.
    pub ctx_tokens: u64,
    /// Cumulative session token usage + cost basis (the HUD's source of truth).
    pub ledger: Ledger,
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
            ledger: Ledger::default(),
        }
    }
}

/// The full cockpit state.
pub struct App {
    /// The chat transcript (top pane), oldest first.
    pub transcript: Vec<Entry>,
    /// Inline image previews, each pinned after a transcript entry (see
    /// [`super::ImageCell`]). Kept parallel to `transcript` so the text model and
    /// its tests stay free of the non-`PartialEq` image protocol state.
    pub images: Vec<super::ImageCell>,
    /// The assistant entry being built from an in-progress streamed run, or
    /// `None` between runs. Dropping it is the only teardown a run needs — the
    /// buffered tail goes with it, so no reset path can leave text stranded.
    pub live_assistant: Option<LiveAssistant>,
    /// The current input-line buffer.
    pub input: String,
    /// Cursor position in the input line, in **chars** (0 ..= char count).
    pub cursor: usize,
    /// Prompts queued (via Enter) while a turn was running, oldest first. Drained
    /// one at a time as each turn ends, so the next instruction isn't dropped —
    /// and, since a fresh turn's own `TurnEnded` drains the next one, further
    /// queued prompts chain through in order. Empty when nothing waits.
    pub queued: Vec<String>,
    /// Large pasted blocks hidden behind compact, unique composer tokens. A
    /// vector preserves multiple pastes in one prompt without overwriting the
    /// first payload.
    pub(super) pastes: Vec<StashedPaste>,
    /// Previously submitted prompts, oldest first.
    pub history: Vec<String>,
    /// While browsing history: the index being shown. `None` = live draft.
    pub history_pos: Option<usize>,
    /// The live draft stashed while browsing history.
    pub(super) draft: String,
    /// First idle Esc pressed: the next Esc unwinds the last turn. Any other
    /// user action disarms.
    pub esc_armed: bool,
    /// First Ctrl-C pressed: the next Ctrl-C exits. Any other user action
    /// disarms, so accidental interrupts do not tear down the session.
    pub ctrl_c_armed: bool,
    /// The typed `/`-prefix Tab completion is cycling against (the input
    /// itself once Tab starts rewriting it no longer matches).
    pub(super) completion_stem: Option<String>,
    /// Index into the stem's matches that the input currently shows.
    pub completion_idx: Option<usize>,
    /// Approval mode: when `false` (default), schematic applies and immediate
    /// project/board mutations need approval; project-local draft edits do not.
    /// When `true` (`:auto`), gated mutations proceed without a prompt.
    pub auto: bool,
    /// A change awaiting approval, if any. While `Some`, `a`/`r` resolve it.
    pub pending: Option<PendingApproval>,
    /// Whether an agent turn is in flight (submit is blocked, typing is not).
    pub running: bool,
    /// When the in-flight turn started (drives the elapsed display).
    pub turn_started: Option<Instant>,
    /// Total time the elapsed clock has been PAUSED this turn (while an approval
    /// gate held the turn waiting on the user). Subtracted from the raw elapsed so
    /// the working clock reflects model/tool time, not human deliberation.
    pub paused_total: Duration,
    /// When the current pause began, if a gate is open right now. `None` between
    /// gates; folded into `paused_total` when the gate resolves.
    pub paused_since: Option<Instant>,
    /// Tool calls started during the current turn, counted from `ToolStarted`
    /// events. Tracked here (not read from `TurnOutcome`) so the end indicator
    /// can report a count even when the turn was interrupted or errored — paths
    /// that never return an outcome.
    pub turn_tool_calls: usize,
    /// The current unit of agent work for the working row's detail line. Tool
    /// calls set this from `ToolStarted`; the post-commit reviewer sets it from
    /// `ReviewStarted` without counting as a tool call.
    pub active_work: Option<String>,
    /// Animation frame counter, advanced by [`super::Msg::Tick`] while running.
    pub spinner: usize,
    /// The unwind picker, while the user is choosing how far to roll back.
    pub unwind: Option<UnwindPicker>,
    /// Whether `:help` is showing.
    pub help: bool,
    /// Lines scrolled up from the bottom of the transcript (0 = follow tail).
    /// The renderer clamps this to the real maximum for the viewport.
    pub scroll: u16,
    /// Maximum legal transcript scroll offset from the last draw. This lets the
    /// event mapper route empty-prompt Up/Down keys to scrollback only when
    /// there is scrollback to move through.
    pub scroll_max: u16,
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
            images: Vec::new(),
            live_assistant: None,
            input: String::new(),
            cursor: 0,
            queued: Vec::new(),
            pastes: Vec::new(),
            history: Vec::new(),
            history_pos: None,
            draft: String::new(),
            esc_armed: false,
            ctrl_c_armed: false,
            completion_stem: None,
            completion_idx: None,
            auto: false,
            pending: None,
            running: false,
            turn_started: None,
            paused_total: Duration::ZERO,
            paused_since: None,
            turn_tool_calls: 0,
            active_work: None,
            spinner: 0,
            unwind: None,
            help: false,
            scroll: 0,
            scroll_max: 0,
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
    /// flag, the elapsed clock, and follow the tail.
    pub(super) fn begin_turn(&mut self) {
        self.running = true;
        self.turn_started = Some(Instant::now());
        self.paused_total = Duration::ZERO;
        self.paused_since = None;
        self.turn_tool_calls = 0;
        self.active_work = None;
        self.scroll = 0;
    }

    /// Freeze the elapsed clock: called when an approval gate opens, so human
    /// deliberation isn't billed to the working time. Idempotent.
    pub(super) fn pause_clock(&mut self) {
        if self.paused_since.is_none() {
            self.paused_since = Some(Instant::now());
        }
    }

    /// Resume the elapsed clock: fold the just-ended pause into `paused_total`.
    /// Idempotent (a no-op if the clock wasn't paused).
    pub(super) fn resume_clock(&mut self) {
        if let Some(since) = self.paused_since.take() {
            self.paused_total += since.elapsed();
        }
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
        let elapsed = format_duration(secs);
        self.running = false;
        self.turn_started = None;
        self.paused_since = None;
        self.active_work = None;
        self.pending = None;

        // The level glyph is the renderer's job (it tints the whole notice as a
        // callout); the text carries none, or each line would show two markers.
        let entry = match reason {
            TurnEndReason::Compacted => None,
            TurnEndReason::Completed => Some(Entry::notice(
                NoticeLevel::Plain,
                format!("Worked for {elapsed}"),
            )),
            TurnEndReason::ProviderRequestLimit { requests } => Some(Entry::notice(
                NoticeLevel::Error,
                format!(
                    "Worked for {elapsed} — stopped after {requests} model requests (safety limit)"
                ),
            )),
            TurnEndReason::MutationTimedOut => Some(Entry::notice(
                NoticeLevel::Error,
                format!(
                    "Worked for {elapsed} — stopped after a project mutation timed out (it may still be finishing)"
                ),
            )),
            TurnEndReason::NoProgress { completions } => Some(Entry::notice(
                NoticeLevel::Error,
                format!(
                    "Worked for {elapsed} — stopped after {completions} model completions made no durable progress"
                ),
            )),
            TurnEndReason::QualityGateFailed { failures } => Some(Entry::notice(
                NoticeLevel::Error,
                format!(
                    "Worked for {elapsed} — quality gate failed with {failures} unresolved item(s)"
                ),
            )),
            TurnEndReason::Interrupted => Some(Entry::notice(
                NoticeLevel::Plain,
                format!("Worked for {elapsed}"),
            )),
            TurnEndReason::Error(e) => Some(Entry::notice(
                NoticeLevel::Error,
                format!("Worked for {elapsed} — {e}"),
            )),
        };
        if let Some(entry) = entry {
            self.transcript.push(entry);
        }
    }

    /// Seconds the in-flight turn has been actively running (with any
    /// gate-open pause time subtracted), if a turn is in flight.
    pub fn turn_elapsed_secs(&self) -> Option<u64> {
        self.turn_started.map(|t| {
            // Total wall time, minus the closed pauses, minus the pause in
            // progress right now (if a gate is currently open).
            let mut paused = self.paused_total;
            if let Some(since) = self.paused_since {
                paused += since.elapsed();
            }
            t.elapsed().saturating_sub(paused).as_secs()
        })
    }
}

fn format_duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    match (h, m, s) {
        (0, 0, s) => format!("{s}s"),
        (0, m, 0) => format!("{m}m"),
        (0, m, s) => format!("{m}m {s}s"),
        (h, 0, 0) => format!("{h}h"),
        (h, m, 0) => format!("{h}h {m}m"),
        (h, 0, s) => format!("{h}h {s}s"),
        (h, m, s) => format!("{h}h {m}m {s}s"),
    }
}
