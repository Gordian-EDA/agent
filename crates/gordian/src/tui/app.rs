//! The cockpit **state machine** — pure, non-rendering, testable.
//!
//! [`App`] is the whole UI state. [`App::update`] maps a [`Msg`] (a keypress, an
//! [`AgentEvent`] from the run, or a turn ending) into a state transition and
//! returns the [`Action`] the shell performs. Nothing here touches a terminal or
//! the network.
//!
//! The shell ([`super::run`]) owns the terminal, the crossterm stream and the
//! run task; the renderer ([`super::ui`]) reads the `App` and writes back only
//! layout-derived numbers (the clamped scroll offset and the viewport height).

use std::path::PathBuf;
use std::time::Instant;

use gordian_core::AgentEvent;

use super::image::Images;
use super::pricing::Ledger;

/// One thing that happened, in the order it happened.
#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    /// What the user asked for.
    User(String),
    /// Prose the model wrote alongside its tool calls.
    Assistant(String),
    /// A tool call and, once it returns, what it said.
    Tool {
        name: String,
        args: String,
        result: Option<String>,
        seconds: f64,
    },
    /// A PNG the run wrote, drawn inline.
    Render { label: String, path: PathBuf },
    /// The critic's verdict on a build.
    Review {
        score: f64,
        mean: f64,
        defects: usize,
    },
    /// Progress with no further structure, and the cockpit's own remarks.
    Note(String),
    /// A failure worth a callout.
    Error(String),
    /// The rule that closes a turn.
    Divider(String),
}

/// What the shell must do after an update.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    None,
    /// Start a run over the project with this prompt.
    Spawn(String),
    /// Abandon the run in flight.
    Cancel,
    /// Write the current frame to an SVG beside the project.
    Screenshot,
    Quit,
}

/// Everything the cockpit reacts to.
#[derive(Clone, Debug)]
pub enum Msg {
    Char(char),
    Paste(String),
    Newline,
    Backspace,
    Delete,
    CursorLeft,
    CursorRight,
    WordLeft,
    WordRight,
    Home,
    End,
    KillToStart,
    KillWordBack,
    HistoryPrev,
    HistoryNext,
    Submit,
    Cancel,
    ForceQuit,
    ScrollUp,
    ScrollDown,
    PageUp(u16),
    PageDown(u16),
    ScrollToBottom,
    Tick,
    /// One structured moment from the run in flight.
    Agent(AgentEvent),
    /// The run task joined.
    TurnEnded(TurnEnd),
}

/// Why a run stopped.
#[derive(Clone, Debug, PartialEq)]
pub enum TurnEnd {
    /// The run delivered; the string is its one-line result.
    Completed(String),
    Interrupted,
    Error(String),
}

/// What the status bar reports about the session.
#[derive(Clone, Debug)]
pub struct Status {
    pub provider: String,
    pub model: String,
    /// The project directory, as a display string.
    pub project: String,
    /// Whether a usable KiCad 10 install was found.
    pub kicad: bool,
    /// Whether the project already holds a sheet, so the next prompt is an edit.
    pub edit_mode: bool,
    pub turns: usize,
    pub ledger: Ledger,
    /// The best review score the session has seen.
    pub review: Option<f64>,
}

impl Status {
    pub fn new(provider: impl Into<String>, model: impl Into<String>, project: PathBuf) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            project: tildify(&project),
            kicad: false,
            edit_mode: false,
            turns: 0,
            ledger: Ledger::default(),
            review: None,
        }
    }
}

/// The whole cockpit state.
pub struct App {
    pub transcript: Vec<Entry>,
    /// Decoded inline renders, keyed by path and pane width.
    pub images: Images,
    pub input: String,
    /// Cursor position in the input, in **chars**.
    pub cursor: usize,
    /// Prompts entered while a run was working, oldest first.
    pub queued: Vec<String>,
    pub history: Vec<String>,
    /// While browsing history, the index shown; `None` is the live draft.
    pub history_pos: Option<usize>,
    draft: String,
    /// Whether a run is in flight.
    pub running: bool,
    pub turn_started: Option<Instant>,
    /// Tool calls in the current run, for the closing rule.
    pub turn_tools: usize,
    /// What the run is doing right now, for the working row.
    pub active_work: Option<String>,
    /// First Ctrl-C pressed; the next one exits.
    pub ctrl_c_armed: bool,
    pub spinner: usize,
    pub help: bool,
    /// Rows scrolled up from the tail (0 follows the tail). The renderer clamps.
    pub scroll: u16,
    /// The largest legal scroll from the last draw, written by the renderer.
    pub scroll_max: u16,
    /// The transcript viewport height from the last draw, for PgUp/PgDn.
    pub viewport_h: u16,
    pub status: Status,
    pub should_quit: bool,
}

impl App {
    pub fn new(status: Status) -> Self {
        Self {
            transcript: Vec::new(),
            images: Images::default(),
            input: String::new(),
            cursor: 0,
            queued: Vec::new(),
            history: Vec::new(),
            history_pos: None,
            draft: String::new(),
            running: false,
            turn_started: None,
            turn_tools: 0,
            active_work: None,
            ctrl_c_armed: false,
            spinner: 0,
            help: false,
            scroll: 0,
            scroll_max: 0,
            viewport_h: 0,
            status,
            should_quit: false,
        }
    }

    /// Seconds the run in flight has been going.
    pub fn elapsed(&self) -> Option<u64> {
        self.turn_started.map(|t| t.elapsed().as_secs())
    }

    /// Apply one message and report what the shell must do.
    pub fn update(&mut self, msg: Msg) -> Action {
        // Any deliberate act disarms the quit confirmation.
        if !matches!(msg, Msg::ForceQuit | Msg::Tick | Msg::Agent(_)) {
            self.ctrl_c_armed = false;
        }
        match msg {
            Msg::Char(c) => {
                self.insert(&c.to_string());
                Action::None
            }
            Msg::Paste(text) => {
                self.insert(&text);
                Action::None
            }
            Msg::Newline => {
                self.insert("\n");
                Action::None
            }
            Msg::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.remove(self.cursor);
                }
                Action::None
            }
            Msg::Delete => {
                if self.cursor < self.len() {
                    self.remove(self.cursor);
                }
                Action::None
            }
            Msg::CursorLeft => {
                self.cursor = self.cursor.saturating_sub(1);
                Action::None
            }
            Msg::CursorRight => {
                self.cursor = (self.cursor + 1).min(self.len());
                Action::None
            }
            Msg::WordLeft => {
                self.cursor = word_left(&self.chars(), self.cursor);
                Action::None
            }
            Msg::WordRight => {
                self.cursor = word_right(&self.chars(), self.cursor);
                Action::None
            }
            Msg::Home => {
                self.cursor = 0;
                Action::None
            }
            Msg::End => {
                self.cursor = self.len();
                Action::None
            }
            Msg::KillToStart => {
                let rest: String = self.chars()[self.cursor..].iter().collect();
                self.input = rest;
                self.cursor = 0;
                Action::None
            }
            Msg::KillWordBack => {
                let chars = self.chars();
                let start = word_left(&chars, self.cursor);
                let kept: String = chars[..start]
                    .iter()
                    .chain(chars[self.cursor..].iter())
                    .collect();
                self.input = kept;
                self.cursor = start;
                Action::None
            }
            Msg::HistoryPrev => {
                self.recall(-1);
                Action::None
            }
            Msg::HistoryNext => {
                self.recall(1);
                Action::None
            }
            Msg::Submit => self.submit(),
            Msg::Cancel => {
                if self.help {
                    self.help = false;
                    Action::None
                } else if self.running {
                    Action::Cancel
                } else {
                    Action::None
                }
            }
            Msg::ForceQuit => {
                if self.ctrl_c_armed || !self.running {
                    self.should_quit = true;
                    Action::Quit
                } else {
                    self.ctrl_c_armed = true;
                    Action::None
                }
            }
            Msg::ScrollUp => {
                self.scroll = (self.scroll + 1).min(self.scroll_max);
                Action::None
            }
            Msg::ScrollDown => {
                self.scroll = self.scroll.saturating_sub(1);
                Action::None
            }
            Msg::PageUp(h) => {
                self.scroll = (self.scroll + h.max(1)).min(self.scroll_max);
                Action::None
            }
            Msg::PageDown(h) => {
                self.scroll = self.scroll.saturating_sub(h.max(1));
                Action::None
            }
            Msg::ScrollToBottom => {
                self.scroll = 0;
                Action::None
            }
            Msg::Tick => {
                self.spinner = self.spinner.wrapping_add(1);
                Action::None
            }
            Msg::Agent(event) => {
                self.record(event);
                Action::None
            }
            Msg::TurnEnded(reason) => self.end_turn(reason),
        }
    }

    /// Enter: run the prompt, queue it behind the one in flight, or obey a
    /// `/command`.
    fn submit(&mut self) -> Action {
        let text = self.input.trim().to_string();
        self.input.clear();
        self.cursor = 0;
        self.history_pos = None;
        self.draft.clear();
        if text.is_empty() {
            return Action::None;
        }
        if let Some(command) = text.strip_prefix('/') {
            return self.command(command);
        }
        if self.history.last().map(String::as_str) != Some(text.as_str()) {
            self.history.push(text.clone());
        }
        if self.running {
            self.queued.push(text);
            return Action::None;
        }
        self.begin(text)
    }

    fn command(&mut self, command: &str) -> Action {
        match command.split_whitespace().next().unwrap_or("") {
            "help" | "?" => {
                self.help = !self.help;
                Action::None
            }
            "clear" => {
                self.transcript.clear();
                self.scroll = 0;
                Action::None
            }
            "shot" => Action::Screenshot,
            "quit" | "exit" | "q" => {
                self.should_quit = true;
                Action::Quit
            }
            other => {
                self.transcript.push(Entry::Error(format!(
                    "unknown command `/{other}` — try /help, /clear, /shot, /quit"
                )));
                Action::None
            }
        }
    }

    /// Start a run: post the prompt and arm the working row.
    fn begin(&mut self, prompt: String) -> Action {
        self.transcript.push(Entry::User(prompt.clone()));
        self.running = true;
        self.turn_started = Some(Instant::now());
        self.turn_tools = 0;
        self.active_work = None;
        self.scroll = 0;
        self.status.turns += 1;
        Action::Spawn(prompt)
    }

    /// Fold one run event into the transcript.
    fn record(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Assistant(text) => self.transcript.push(Entry::Assistant(text)),
            AgentEvent::ToolCall { name, args } => {
                self.turn_tools += 1;
                self.active_work = Some(name.clone());
                self.transcript.push(Entry::Tool {
                    name,
                    args,
                    result: None,
                    seconds: 0.0,
                });
            }
            AgentEvent::ToolResult {
                name,
                seconds,
                summary,
            } => {
                let open = self.transcript.iter_mut().rev().find(
                    |e| matches!(e, Entry::Tool { name: n, result, .. } if *n == name && result.is_none()),
                );
                match open {
                    Some(Entry::Tool {
                        result, seconds: s, ..
                    }) => {
                        *result = Some(summary);
                        *s = seconds;
                    }
                    _ => self.transcript.push(Entry::Tool {
                        name,
                        args: String::new(),
                        result: Some(summary),
                        seconds,
                    }),
                }
                self.active_work = None;
            }
            AgentEvent::Usage {
                input,
                output,
                cache_write,
                cached,
                ..
            } => self.status.ledger.record(input, output, cache_write, cached),
            AgentEvent::Review {
                score,
                mean,
                defects,
                ..
            } => {
                self.status.review = Some(self.status.review.map_or(mean, |b| b.max(mean)));
                self.transcript.push(Entry::Review {
                    score,
                    mean,
                    defects,
                });
            }
            AgentEvent::Render { label, path } => {
                // The run overwrites `schematic.png` in place across turns, so a
                // repeat of the same path is new bytes, not the cached picture.
                self.images.invalidate(&path);
                self.transcript.push(Entry::Render { label, path });
            }
            AgentEvent::Note(line) => {
                self.active_work = Some(line.clone());
                self.transcript.push(Entry::Note(line));
            }
            AgentEvent::Done => self.active_work = None,
        }
    }

    /// Close the run, post its rule, and start whatever was queued behind it.
    fn end_turn(&mut self, reason: TurnEnd) -> Action {
        let elapsed = format_duration(self.elapsed().unwrap_or(0));
        self.running = false;
        self.turn_started = None;
        self.active_work = None;
        let tools = self.turn_tools;
        match reason {
            TurnEnd::Completed(result) => {
                self.status.edit_mode = true;
                self.transcript.push(Entry::Divider(format!(
                    "Worked for {elapsed} · {tools} tool call(s) · {result}"
                )));
            }
            TurnEnd::Interrupted => self
                .transcript
                .push(Entry::Divider(format!("Interrupted after {elapsed}"))),
            TurnEnd::Error(error) => self
                .transcript
                .push(Entry::Error(format!("Stopped after {elapsed} — {error}"))),
        }
        match self.queued.is_empty() {
            true => Action::None,
            false => {
                let next = self.queued.remove(0);
                self.begin(next)
            }
        }
    }

    // ---- the composer --------------------------------------------------

    fn chars(&self) -> Vec<char> {
        self.input.chars().collect()
    }

    fn len(&self) -> usize {
        self.input.chars().count()
    }

    fn insert(&mut self, text: &str) {
        let byte = self.byte_at(self.cursor);
        self.input.insert_str(byte, text);
        self.cursor += text.chars().count();
        self.history_pos = None;
    }

    fn remove(&mut self, index: usize) {
        let byte = self.byte_at(index);
        self.input.remove(byte);
    }

    fn byte_at(&self, index: usize) -> usize {
        self.input
            .char_indices()
            .nth(index)
            .map_or(self.input.len(), |(b, _)| b)
    }

    /// Step through submitted prompts; stepping past the newest restores the
    /// draft that was stashed on the way in.
    fn recall(&mut self, delta: isize) {
        if self.history.is_empty() {
            return;
        }
        let next = match (self.history_pos, delta) {
            (None, -1) => {
                self.draft = self.input.clone();
                Some(self.history.len() - 1)
            }
            (None, _) => None,
            (Some(0), -1) => Some(0),
            (Some(i), -1) => Some(i - 1),
            (Some(i), _) if i + 1 < self.history.len() => Some(i + 1),
            (Some(_), _) => None,
        };
        self.history_pos = next;
        self.input = match next {
            Some(i) => self.history[i].clone(),
            None => std::mem::take(&mut self.draft),
        };
        self.cursor = self.len();
    }
}

fn word_left(chars: &[char], from: usize) -> usize {
    let mut i = from;
    while i > 0 && chars[i - 1].is_whitespace() {
        i -= 1;
    }
    while i > 0 && !chars[i - 1].is_whitespace() {
        i -= 1;
    }
    i
}

fn word_right(chars: &[char], from: usize) -> usize {
    let mut i = from;
    while i < chars.len() && chars[i].is_whitespace() {
        i += 1;
    }
    while i < chars.len() && !chars[i].is_whitespace() {
        i += 1;
    }
    i
}

/// A path under `$HOME` shown as `~/…`, so the status bar reads as a place
/// rather than a full path.
fn tildify(path: &std::path::Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match home.and_then(|home| path.strip_prefix(home).ok().map(std::path::Path::to_path_buf)) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// `2m 13s` — the elapsed reading on the working row and the closing rule.
pub fn format_duration(secs: u64) -> String {
    match (secs / 3600, (secs % 3600) / 60, secs % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, 0) => format!("{m}m"),
        (0, m, s) => format!("{m}m {s}s"),
        (h, m, _) => format!("{h}h {m}m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        App::new(Status::new(
            "openai",
            "claude-opus-4-5",
            PathBuf::from("/tmp/proj"),
        ))
    }

    fn type_str(a: &mut App, text: &str) {
        for c in text.chars() {
            a.update(Msg::Char(c));
        }
    }

    #[test]
    fn a_project_under_home_reads_as_a_place() {
        // SAFETY: single-threaded test process, and the variable is read only
        // through `tildify` below.
        unsafe { std::env::set_var("HOME", "/home/someone") };
        assert_eq!(tildify(std::path::Path::new("/home/someone/boards/a")), "~/boards/a");
        assert_eq!(tildify(std::path::Path::new("/srv/boards")), "/srv/boards");
    }

    #[test]
    fn typing_and_editing_move_by_characters_not_bytes() {
        let mut a = app();
        type_str(&mut a, "héllo");
        a.update(Msg::Home);
        a.update(Msg::CursorRight);
        a.update(Msg::CursorRight);
        a.update(Msg::Backspace);
        assert_eq!(a.input, "hllo");
        a.update(Msg::End);
        a.update(Msg::KillWordBack);
        assert!(a.input.is_empty());
    }

    #[test]
    fn a_prompt_spawns_a_run_and_lands_in_the_transcript() {
        let mut a = app();
        type_str(&mut a, "design a 555 blinker");
        assert_eq!(a.update(Msg::Submit), Action::Spawn("design a 555 blinker".into()));
        assert!(a.running);
        assert_eq!(a.status.turns, 1);
        assert_eq!(a.transcript[0], Entry::User("design a 555 blinker".into()));
    }

    /// A second prompt while the run works waits its turn rather than racing it,
    /// and starts as soon as the first delivers.
    #[test]
    fn a_second_prompt_queues_and_starts_when_the_run_ends() {
        let mut a = app();
        type_str(&mut a, "first");
        a.update(Msg::Submit);
        type_str(&mut a, "then route it");
        assert_eq!(a.update(Msg::Submit), Action::None);
        assert_eq!(a.queued, vec!["then route it".to_string()]);

        let next = a.update(Msg::TurnEnded(TurnEnd::Completed("review 8".into())));

        assert_eq!(next, Action::Spawn("then route it".into()));
        assert!(a.queued.is_empty());
        assert!(a.status.edit_mode, "the sheet now exists, so the next run edits");
    }

    #[test]
    fn history_walks_back_and_restores_the_draft() {
        let mut a = app();
        type_str(&mut a, "first");
        a.update(Msg::Submit);
        a.update(Msg::TurnEnded(TurnEnd::Completed(String::new())));
        type_str(&mut a, "second");
        a.update(Msg::Submit);
        a.update(Msg::TurnEnded(TurnEnd::Completed(String::new())));

        type_str(&mut a, "draft");
        a.update(Msg::HistoryPrev);
        assert_eq!(a.input, "second");
        a.update(Msg::HistoryPrev);
        assert_eq!(a.input, "first");
        a.update(Msg::HistoryPrev);
        assert_eq!(a.input, "first", "already at the oldest");
        a.update(Msg::HistoryNext);
        assert_eq!(a.input, "second");
        a.update(Msg::HistoryNext);
        assert_eq!(a.input, "draft");
    }

    /// A tool result closes the call it belongs to instead of opening a row of
    /// its own.
    #[test]
    fn a_tool_result_lands_on_its_own_call() {
        let mut a = app();
        a.update(Msg::Agent(AgentEvent::ToolCall {
            name: "build".into(),
            args: "{\"bytes\":900}".into(),
        }));
        a.update(Msg::Agent(AgentEvent::ToolResult {
            name: "build".into(),
            seconds: 2.5,
            summary: "v1 clean".into(),
        }));

        assert_eq!(a.transcript.len(), 1);
        assert_eq!(
            a.transcript[0],
            Entry::Tool {
                name: "build".into(),
                args: "{\"bytes\":900}".into(),
                result: Some("v1 clean".into()),
                seconds: 2.5,
            }
        );
        assert_eq!(a.turn_tools, 1);
    }

    #[test]
    fn usage_and_reviews_feed_the_status_bar() {
        let mut a = app();
        a.update(Msg::Agent(AgentEvent::Usage {
            request: 1,
            input: 1000,
            output: 200,
            cache_write: 0,
            cached: 400,
            seconds: 1.0,
        }));
        a.update(Msg::Agent(AgentEvent::Review {
            score: 8.0,
            mean: 7.6,
            samples: vec![8.0, 7.0],
            defects: 3,
        }));
        a.update(Msg::Agent(AgentEvent::Review {
            score: 6.0,
            mean: 6.1,
            samples: vec![6.0],
            defects: 5,
        }));

        assert_eq!(a.status.ledger.requests, 1);
        assert_eq!(a.status.ledger.input_tokens(), 1000);
        assert_eq!(a.status.review, Some(7.6), "the best read is the one shown");
    }

    /// Ctrl-C is a confirmation while a run is in flight, and immediate when the
    /// cockpit is idle.
    #[test]
    fn quitting_takes_two_presses_only_while_running() {
        let mut a = app();
        assert_eq!(a.update(Msg::ForceQuit), Action::Quit);

        let mut a = app();
        type_str(&mut a, "go");
        a.update(Msg::Submit);
        assert_eq!(a.update(Msg::ForceQuit), Action::None);
        assert!(a.ctrl_c_armed);
        assert_eq!(a.update(Msg::ForceQuit), Action::Quit);
    }

    #[test]
    fn esc_cancels_a_run_and_closes_help_first() {
        let mut a = app();
        assert_eq!(a.update(Msg::Cancel), Action::None);
        type_str(&mut a, "go");
        a.update(Msg::Submit);
        assert_eq!(a.update(Msg::Cancel), Action::Cancel);
    }

    #[test]
    fn commands_toggle_help_clear_and_screenshot() {
        let mut a = app();
        type_str(&mut a, "/help");
        a.update(Msg::Submit);
        assert!(a.help);
        a.transcript.push(Entry::Note("x".into()));
        type_str(&mut a, "/clear");
        a.update(Msg::Submit);
        assert!(a.transcript.is_empty());
        type_str(&mut a, "/shot");
        assert_eq!(a.update(Msg::Submit), Action::Screenshot);
        type_str(&mut a, "/nope");
        a.update(Msg::Submit);
        assert!(matches!(a.transcript[0], Entry::Error(_)));
    }

    #[test]
    fn scrolling_is_clamped_to_what_the_renderer_measured() {
        let mut a = app();
        a.scroll_max = 3;
        for _ in 0..10 {
            a.update(Msg::ScrollUp);
        }
        assert_eq!(a.scroll, 3);
        a.update(Msg::PageDown(2));
        assert_eq!(a.scroll, 1);
        a.update(Msg::ScrollToBottom);
        assert_eq!(a.scroll, 0);
    }
}
