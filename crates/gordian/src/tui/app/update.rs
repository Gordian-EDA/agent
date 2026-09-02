//! The event reducer: [`Msg`] in, [`Action`] out.
//!
//! [`App::update`] is the single entry point — it maps a keypress, an agent
//! event, or an async arrival into a state transition and returns the [`Action`]
//! the shell performs. The submit, command-dispatch, and cancel transitions it
//! delegates to live here too ([`App::submit`], [`App::run_command`], [`App::cancel`]).

use super::{App, Entry};
use gordian_core::AgentEvent;

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
    WordLeft,
    WordRight,
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
    /// Scroll the transcript up / down by one line.
    ScrollUp,
    ScrollDown,
    /// Jump the transcript by roughly a viewport height (PgUp / PgDn). The
    /// shell passes the live viewport height so the jump tracks the window.
    PageUp(u16),
    PageDown(u16),
    /// Snap the transcript back to the live tail (End / Ctrl-End).
    ScrollToBottom,
    /// Shift/Alt+Enter — insert a literal newline into the composer (a
    /// multi-line prompt) rather than submitting.
    Newline,
    /// A bracketed-paste payload from the terminal. A large block is collapsed to
    /// a `[Pasted N chars]` placeholder in the composer (the real text expands
    /// back in on submit); a small one is inserted verbatim.
    Paste(String),
    /// Esc — close help / clear input / cancel a turn / arm
    /// (then perform) a context unwind, in that order of precedence.
    Cancel,
    /// Ctrl-C — arm quit; a second Ctrl-C exits.
    ForceQuit,
    /// A periodic animation tick from the shell (advances the spinner).
    Tick,
    /// An event from the running agent turn.
    Agent(AgentEvent),
    /// A turn finished (the spawned task joined). This is the single, reliable
    /// teardown point — it fires exactly once per turn (from the join channel,
    /// or directly from the shell on a user abort) and carries *why* the turn
    /// stopped so the indicator can be labelled. Clears the running flag even if
    /// no `TurnDone` event arrived (e.g. the turn errored or was interrupted).
    TurnEnded(TurnEndReason),
}

/// Why an in-flight turn stopped, carried on [`Msg::TurnEnded`]. The agent loop
/// reports a clean completion or a bounded safety stop (via its `StopReason`);
/// the shell adds `Interrupted` (user abort) and `Error`; `/compact` reports
/// `Compacted`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnEndReason {
    /// The model returned a final reply — a clean finish.
    Completed,
    /// The model kept requesting tools until the safety ceiling was reached.
    ProviderRequestLimit { requests: usize },
    /// A project mutation timed out and may still be running in the background.
    MutationTimedOut,
    /// Required artifact checks or independent review still have findings.
    QualityGateFailed { failures: usize },
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
    /// Abort the in-flight agent turn.
    CancelTurn,
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
    /// `/preview` — open the latest render in the system viewer.
    OpenPreview(String),
    /// Tear down the TUI and exit.
    Quit,
}

impl App {
    /// Apply one message, mutating state and returning the shell's next action.
    pub fn update(&mut self, msg: Msg) -> Action {
        if matches!(msg, Msg::ForceQuit) {
            return self.request_quit();
        }

        // Any user action other than Ctrl-C disarms the two-step quit catcher.
        if !matches!(msg, Msg::Tick | Msg::Agent(_) | Msg::TurnEnded(_)) {
            self.ctrl_c_armed = false;
        }

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
                Msg::Submit => self.confirm_unwind(),
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
            Msg::Cancel | Msg::Tick | Msg::Agent(_) | Msg::TurnEnded(_)
        ) {
            self.esc_armed = false;
        }
        // Any input change other than Tab itself restarts completion cycling.
        // Submit is excluded so it can read the highlighted completion (it
        // resets the cycle itself once it has decided accept-vs-run).
        if !matches!(
            msg,
            Msg::Complete
                | Msg::Submit
                | Msg::Tick
                | Msg::Agent(_)
                | Msg::TurnEnded(_)
                | Msg::ScrollUp
                | Msg::ScrollDown
                | Msg::PageUp(_)
                | Msg::PageDown(_)
        ) {
            self.completion_stem = None;
            self.completion_idx = None;
        }

        match msg {
            Msg::Char(c) => {
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
            Msg::WordLeft => {
                self.move_word_left();
                Action::None
            }
            Msg::WordRight => {
                self.move_word_right();
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
                if self.completion_view().is_some() {
                    // A `/command` stem: Tab completes / cycles it.
                    self.complete_next();
                }
                Action::None
            }
            Msg::Submit => self.submit(),
            Msg::Paste(text) => {
                self.paste_text(text);
                Action::None
            }
            Msg::ScrollUp => {
                self.scroll = self.scroll.saturating_add(1);
                Action::None
            }
            Msg::ScrollDown => {
                self.scroll = self.scroll.saturating_sub(1);
                Action::None
            }
            Msg::PageUp(h) => {
                self.scroll = self.scroll.saturating_add(h.max(1));
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
            Msg::Newline => {
                self.insert_char('\n');
                Action::None
            }
            Msg::Cancel => self.cancel(),
            Msg::ForceQuit => unreachable!("handled before modal dispatch"),
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
            Msg::TurnEnded(reason) => {
                let interrupted = reason == TurnEndReason::Interrupted;
                self.end_turn(reason);
                // The oldest queued message normally runs next — `submit()`
                // spawns its turn, whose own `TurnEnded` will in turn drain the
                // next one, so several queued prompts chain through in order.
                // An explicit user interruption restores it to the composer
                // instead and leaves the rest queued: cancelling one task must
                // not silently launch another.
                if !self.queued.is_empty() {
                    let prompt = self.queued.remove(0);
                    self.input = prompt;
                    self.cursor = self.char_len();
                    if interrupted {
                        return Action::None;
                    }
                    return self.submit();
                }
                Action::None
            }
        }
    }

    /// Esc, layered: close help → clear a non-empty input → cancel a running
    /// turn → arm, then perform, a one-turn context unwind.
    /// Esc never quits; that's double Ctrl-C or `/quit`.
    fn cancel(&mut self) -> Action {
        if self.help {
            self.help = false;
            Action::None
        } else if !self.input.is_empty() {
            self.clear_input();
            Action::None
        } else if self.running {
            // The shell aborts the task and replies with `TurnEnded(Interrupted)`,
            // which posts the "Worked for …" indicator — no separate
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

    /// First Ctrl-C arms the footer guard; the second exits.
    fn request_quit(&mut self) -> Action {
        if self.ctrl_c_armed {
            self.should_quit = true;
            return Action::Quit;
        }
        self.ctrl_c_armed = true;
        self.esc_armed = false;
        Action::None
    }

    /// Confirm the picker: close it and ask the shell to drop `selected + 1`
    /// turns (the selected prompt and everything after it).
    fn confirm_unwind(&mut self) -> Action {
        match self.unwind.take() {
            Some(p) => Action::UnwindTo(p.selected + 1),
            None => Action::None,
        }
    }

    /// Submit the input line: a `/command` or a prompt. When the user has
    /// highlighted a completion (Tab-cycled into the popup), Enter accepts it
    /// into the input instead of submitting — they confirm the command first,
    /// then press Enter again to run it. A bare `/help` with no highlight still
    /// submits directly.
    fn submit(&mut self) -> Action {
        if let Some((matches, Some(idx))) = self.completion_view() {
            self.input = matches[idx].name.to_string();
            self.cursor = self.char_len();
            self.completion_stem = None;
            self.completion_idx = None;
            return Action::None;
        }
        // Expand any `[Pasted N chars]` placeholder back to the real text before
        // dispatch, so the model receives what was actually pasted.
        let line = self.expanded_input().trim().to_string();
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
            // A turn is already in flight: queue this one rather than dropping
            // it or starting a second turn. Each `TurnEnded` drains the oldest
            // queued prompt and submits it, so further Enters chain in order.
            self.queued.push(line);
            self.clear_input();
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
            "/clear" => {
                self.transcript.clear();
                self.images.clear();
                self.live_assistant = None;
                self.scroll = 0;
                Action::ClearContext
            }
            "/context" => Action::ShowContext,
            "/preview" => {
                match self.latest_render_path() {
                    Some(path) => return Action::OpenPreview(path),
                    None => self.transcript.push(Entry::system(
                        "no render yet — ask me to render the board first",
                    )),
                }
                Action::None
            }
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
}
