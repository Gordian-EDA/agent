//! The ratatui copilot cockpit.
//!
//! ## Architecture
//!
//! All design logic lives in `gordian-core`. This module is the **shell**: it
//! owns the terminal, the crossterm event stream, and the spawned run task, and
//! wires them to the testable [`app::App`] state machine and the [`ui::draw`]
//! renderer.
//!
//! ```text
//!   crossterm EventStream ─┐
//!   gordian_core::subscribe┼─ tokio::select! ─► App::update ─► Action ─► Shell
//!   animation tick        ─┘                                   (spawn a run,
//!                                                              cancel,
//!                                                              quit)
//! ```
//!
//! One prompt is one whole `gordian_core::run::run`: the design loop, the polish
//! pass and the board. A second prompt on the same project is an edit — the run
//! opens the `design.kicad_sch` that is already there and patches it. The core
//! reports its progress through [`bridge`], which is the only place the cockpit
//! and the core's event vocabulary meet.

pub mod app;
pub mod bridge;
pub mod event;
pub mod md;
pub mod pricing;
pub mod theme;
pub mod ui;

#[cfg(test)]
mod screenshot;

use std::io::{self, Stdout};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    EventStream, KeyboardEnhancementFlags, MouseButton, MouseEventKind, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};
use futures::StreamExt;
use gordian_core::agent::Budget;
use gordian_core::run::{RunOptions, run as design};
use gordian_llm::Provider as _;
use kicad::KicadInstallation;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use serde_json::Value;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;

use crate::config::LoadedConfig;
use app::{Action, App, Msg, Status, TurnEndReason};
use bridge::AgentEvent;

/// How often the shell wakes the app for spinner/elapsed redraws.
const TICK: Duration = Duration::from_millis(120);

/// Identity of a spawned run task. Async messages carry this so a late event or
/// completion from an aborted task cannot affect its successor.
type TaskId = u64;
type TaskEvent = (TaskId, AgentEvent);
type TaskDone = (TaskId, TurnEndReason);

/// What a feature the old context-editing commands drove says now that a turn
/// is one whole run and the core keeps no conversation to edit.
const NO_CONTEXT_EDITING: &str = "not available in this build — a prompt is one \
     whole design run, and the core keeps no conversation to roll back";

/// Launch the cockpit over a project directory. Sets up the terminal, builds the
/// provider and KiCad handles, and runs the event loop until the user quits.
pub async fn run(project_dir: PathBuf, loaded: LoadedConfig) -> Result<()> {
    std::fs::create_dir_all(&project_dir)
        .with_context(|| format!("creating {}", project_dir.display()))?;

    // 1. Detect KiCAD (best-effort: the UI still launches without it, just shows
    //    a disconnected indicator and no run is possible).
    let env_result = crate::config::detect_kicad(&loaded.config);
    let kicad_error = env_result.as_ref().err().map(ToString::to_string);
    let kicad = env_result.ok();

    // 2. Build the provider. Without one the cockpit launches "degraded" and
    //    says what is missing, so `tui` never fails at the command line.
    let client_result = gordian_core::GenaiProvider::from_config(&loaded.config.llm);
    let log_thread_id = client_result
        .as_ref()
        .map(|client| client.thread_identifier())
        .unwrap_or("tui-unconfigured");
    let _log_guard = gordian_runtime::logging::init_file_only(&project_dir, log_thread_id);
    let llm_error = client_result.as_ref().err().map(|e| format!("{e:#}"));
    let (provider, model) = match &client_result {
        Ok(client) => client.status(),
        Err(_) => ("unconfigured".to_string(), "(llm.model unset)".to_string()),
    };

    let sch_path = project_dir.join(&loaded.config.project.schematic_filename);
    let status = Status::new(
        provider,
        model,
        sch_path.display().to_string(),
        kicad.is_some(),
    );
    let mut app = App::new(status);
    let client = client_result.ok().map(Arc::new);
    if kicad.is_none() || client.is_none() {
        let mut missing = Vec::new();
        if kicad.is_none() {
            missing.push(kicad_error.clone().unwrap_or_else(|| {
                "KiCad 10 is required; set kicad.cliPath, kicad.symbolDir, and \
                 kicad.footprintDir"
                    .to_string()
            }));
        }
        if let Some(err) = &llm_error {
            missing.push(format!("LLM provider not configured: {err}"));
        }
        app.transcript.push(app::Entry::system(format!(
            "no runs possible — {}. Edit {} then restart. UI is read-only.",
            missing.join("; "),
            loaded.path.display()
        )));
    }

    // 3. Terminal setup (restored on any exit path).
    let (mut terminal, keyboard_enhancement) =
        setup_terminal().context("entering the alternate screen")?;
    let result = event_loop(
        &mut terminal,
        &mut app,
        Runner {
            client,
            kicad,
            project_dir,
            schematic_filename: loaded.config.project.schematic_filename.clone(),
            budget: Budget {
                total: Duration::from_secs(loaded.config.agent.budget_seconds),
                max_builds: loaded.config.agent.max_builds,
                ..Budget::default()
            },
        },
    )
    .await;
    restore_terminal(&mut terminal, keyboard_enhancement).ok();
    gordian_core::unsubscribe();
    if let Err(error) = &result {
        tracing::error!(error = %error, "TUI event loop failed");
    }
    result
}

/// Everything a design run needs, once. Held by the [`Shell`] and cloned into
/// each spawned run.
struct Runner {
    client: Option<Arc<gordian_core::GenaiProvider>>,
    kicad: Option<KicadInstallation>,
    project_dir: PathBuf,
    schematic_filename: String,
    budget: Budget,
}

/// The side-effecting half of the cockpit: everything the [`Action`]s returned
/// by [`App::update`] need to touch (channels, the runner, and the in-flight
/// run task).
struct Shell {
    runner: Runner,
    events_tx: UnboundedSender<TaskEvent>,
    done_tx: UnboundedSender<TaskDone>,
    /// The in-flight run task (aborted by [`Action::CancelTurn`]).
    turn_task: Option<JoinHandle<()>>,
    /// Identity of `turn_task`; cleared before cancellation closes the App turn.
    active_task_id: Option<TaskId>,
    /// Monotonic source for task identities (wrapping is harmless in practice;
    /// zero is skipped to keep the initial state visibly distinct).
    next_task_id: TaskId,
}

impl Shell {
    /// Stop any detached run on every shell exit path.
    fn shutdown(&mut self) {
        if let Some(task) = self.turn_task.take() {
            task.abort();
        }
    }

    /// Perform the side effect an [`Action`] calls for.
    fn handle(&mut self, app: &mut App, action: Action) {
        match action {
            Action::None => {}
            Action::Quit => {
                app.should_quit = true;
            }
            Action::CancelTurn => self.cancel_turn(app),
            Action::ClearContext => {
                app.status.ctx_tokens = 0;
                app.transcript
                    .push(app::Entry::system("transcript cleared"));
            }
            Action::OpenUnwind | Action::UnwindTo(_) => {
                app.transcript
                    .push(app::Entry::system(format!("unwind: {NO_CONTEXT_EDITING}")));
            }
            Action::Compact => {
                let follow_up = app.update(Msg::TurnEnded(TurnEndReason::Compacted));
                app.transcript
                    .push(app::Entry::system(format!("compact: {NO_CONTEXT_EDITING}")));
                self.handle(app, follow_up);
            }
            Action::ShowContext => self.show_context(app),
            Action::OpenPreview(path) => open_preview(app, path),
            Action::SpawnTurn(prompt) => self.spawn_turn(app, prompt),
        }
    }

    /// Start one whole design run over the project. The sheet already in the
    /// directory is what makes the second prompt an edit; the run finds it
    /// itself, so nothing here has to track turns.
    fn spawn_turn(&mut self, app: &mut App, prompt: String) {
        let (Some(client), Some(kicad)) = (self.runner.client.clone(), self.runner.kicad.clone())
        else {
            app.running = false;
            app.turn_started = None;
            app.transcript
                .push(app::Entry::system("agent unavailable — cannot run a turn"));
            return;
        };
        let options = RunOptions {
            project_dir: self.runner.project_dir.clone(),
            schematic_filename: self.runner.schematic_filename.clone(),
            board: crate::wants_board(&prompt),
            prompt,
            budget: self.runner.budget.clone(),
            polish: true,
            compose_rounds: 2,
        };
        let task_id = self.begin_task();
        // Every stage of the run reports through the core's process-wide sink;
        // re-subscribing per run tags each event with the task that owns it, so
        // a late event from an aborted run is dropped rather than attributed to
        // its successor.
        let events_tx = self.events_tx.clone();
        gordian_core::subscribe(move |event| {
            let _ = events_tx.send((task_id, bridge::translate(event)));
        });
        let done_tx = self.done_tx.clone();
        let results_tx = self.events_tx.clone();
        self.turn_task = Some(tokio::spawn(async move {
            // The run's one-line result goes down the event channel, so it
            // lands in the transcript ahead of the "Worked for …" indicator the
            // completion posts.
            let reason = match design(&*client, &kicad, options).await {
                Ok(report) => {
                    let line = headline(&report.value);
                    let _ = results_tx.send((task_id, AgentEvent::Note(line)));
                    TurnEndReason::Completed
                }
                Err(error) => TurnEndReason::Error(format!("{error:#}")),
            };
            let _ = done_tx.send((task_id, reason));
        }));
    }

    /// Allocate and activate an identity before spawning a new task.
    fn begin_task(&mut self) -> TaskId {
        self.next_task_id = self.next_task_id.wrapping_add(1).max(1);
        self.active_task_id = Some(self.next_task_id);
        self.next_task_id
    }

    /// Esc on a running turn: abort the task mid-flight. The run gives up
    /// whatever it was doing (an LLM round-trip or a KiCad call).
    fn cancel_turn(&mut self, app: &mut App) {
        // Invalidate async messages before aborting. A task can have queued its
        // completion immediately before this handler won the select race.
        self.active_task_id = None;
        if let Some(task) = self.turn_task.take() {
            task.abort();
        }
        // The aborted task never sends done_tx, so close the turn ourselves —
        // flagged as a user interruption so the indicator reads "Interrupted".
        let follow_up = app.update(Msg::TurnEnded(TurnEndReason::Interrupted));
        self.handle(app, follow_up);
    }

    /// `/context` — print project paths and token stats.
    fn show_context(&self, app: &mut App) {
        let s = &app.status;
        let lines = [
            format!("schematic: {}", s.sch_path),
            format!(
                "model: {} ({}) · turns {}",
                s.model, s.provider, s.turn_count
            ),
            {
                let l = &s.ledger;
                let cost = l
                    .cost(&s.model)
                    .map(|c| format!("${c:.2}"))
                    .unwrap_or_else(|| "—".into());
                format!(
                    "provider requests: {} (all model invocations) · tokens: ctx {} · session {} in / {} out · {} cached · {cost}",
                    l.provider_requests,
                    s.ctx_tokens,
                    l.input_tokens(),
                    l.output,
                    l.cache_read
                )
            },
            format!("context: {NO_CONTEXT_EDITING}"),
        ];
        for l in lines {
            app.transcript.push(app::Entry::system(l));
        }
    }

    /// Drop buffered events from an aborted/finished task rather than letting
    /// them append to or alter the counters of a later turn.
    fn receive_agent_event(&self, app: &mut App, task_id: TaskId, event: AgentEvent) {
        if self.active_task_id == Some(task_id) {
            app.update(Msg::Agent(event));
        }
    }

    /// Finish only the current task and execute any follow-up action returned by
    /// the reducer (notably the next queued prompt).
    fn finish_task(&mut self, app: &mut App, task_id: TaskId, reason: TurnEndReason) {
        if self.active_task_id != Some(task_id) {
            return;
        }
        self.active_task_id = None;
        self.turn_task = None;
        let follow_up = app.update(Msg::TurnEnded(reason));
        self.handle(app, follow_up);
    }
}

impl Drop for Shell {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The one-line result of a finished run, from its report.
fn headline(report: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(review) = report["schematic"]["review"]["mean"].as_f64() {
        parts.push(format!("review {review:.1}/10"));
    }
    parts.push(match report["schematic"]["erc_errors"].as_u64().unwrap_or(0) {
        0 => "ERC clean".to_string(),
        n => format!("{n} ERC error(s)"),
    });
    // A run told to skip the board still reports an empty PCB section, so the
    // board is only mentioned when one was actually written.
    if report["pcb"]["pcb"].is_string() {
        let completion = report["pcb"]["completion"].as_f64().unwrap_or(0.0);
        parts.push(match report["pcb"]["unrouted"].as_u64().unwrap_or(0) {
            0 => "board routed".to_string(),
            n => format!("board {:.0}% routed, {n} open", completion * 100.0),
        });
    }
    parts.join(" · ")
}

/// The async event loop: select over keyboard/mouse input, run events, run
/// completion, and the animation tick; update the app; act on the returned
/// action; redraw.
async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    runner: Runner,
) -> Result<()> {
    let mut input = EventStream::new();
    let (events_tx, mut events_rx): (UnboundedSender<TaskEvent>, UnboundedReceiver<TaskEvent>) =
        unbounded_channel();
    // Joins back when the spawned run finishes (so input unlocks even on error).
    let (done_tx, mut done_rx): (UnboundedSender<TaskDone>, UnboundedReceiver<TaskDone>) =
        unbounded_channel();

    let mut shell = Shell {
        runner,
        events_tx,
        done_tx,
        turn_task: None,
        active_task_id: None,
        next_task_id: 0,
    };
    let mut tick = tokio::time::interval(TICK);

    terminal.draw(|f| ui::draw(f, app))?;

    loop {
        tokio::select! {
            // `biased` makes every poll check branches top-to-bottom instead of
            // tokio's default random order. A run's last events are already
            // queued when it reports done, and draining them first keeps two
            // turns' transcript rows from interleaving: without it an unlucky
            // poll picks `done_rx` first, `finish_task` drains the queued
            // prompt and spawns the next run, and its user entry lands before
            // the previous run's own trailing rows.
            biased;

            // ── live run events ───────────────────────────────────────
            Some((task_id, ev)) = events_rx.recv() => {
                shell.receive_agent_event(app, task_id, ev);
            }
            // ── the spawned run finished ──────────────────────────────
            Some((task_id, reason)) = done_rx.recv() => {
                shell.finish_task(app, task_id, reason);
            }
            // ── keyboard / mouse ──────────────────────────────────────
            maybe_ev = input.next() => {
                match maybe_ev {
                    Some(Ok(Event::Key(key))) => {
                        if let Some(msg) = event::map_key(app, key) {
                            let action = app.update(msg);
                            shell.handle(app, action);
                        }
                    }
                    Some(Ok(Event::Mouse(m))) => {
                        match m.kind {
                            MouseEventKind::ScrollUp => { app.update(Msg::ScrollUp); }
                            MouseEventKind::ScrollDown => { app.update(Msg::ScrollDown); }
                            MouseEventKind::Down(MouseButton::Left) => {
                                open_preview_at(app, m.column, m.row);
                            }
                            _ => {}
                        }
                    }
                    Some(Ok(Event::Paste(text))) => {
                        let action = app.update(Msg::Paste(text));
                        shell.handle(app, action);
                    }
                    Some(Ok(_)) => {} // resize / focus: just redraw below
                    Some(Err(_)) | None => break,
                }
            }
            // ── animation tick (spinner / elapsed while running) ──────
            _ = tick.tick() => {
                if !app.running {
                    continue; // nothing animates; skip the redraw
                }
                app.update(Msg::Tick);
            }
        }

        if app.should_quit {
            shell.shutdown();
            break;
        }
        terminal.draw(|f| ui::draw(f, app))?;
    }
    Ok(())
}

/// A left click on an inline preview link opens its PNG in the system viewer.
fn open_preview_at(app: &mut App, x: u16, y: u16) {
    let Some(path) = app.preview_at(x, y).map(|i| app.images[i].path.clone()) else {
        return;
    };
    open_preview(app, path);
}

/// Open a preview PNG in the system viewer
/// (cross-platform via the `open` crate; xdg-open on Linux). Failures surface
/// as an error notice rather than silently doing nothing.
fn open_preview(app: &mut App, path: String) {
    if let Err(e) = open::that_detached(&path) {
        app.transcript.push(app::Entry::notice(
            app::NoticeLevel::Error,
            format!("couldn't open {path}: {e}"),
        ));
    }
}

/// Enter raw mode + the alternate screen and build the ratatui terminal.
///
/// Mouse capture is on so a genuine wheel scroll arrives as a real
/// `Event::Mouse`, distinct from an arrow-key press — without it, most
/// terminals translate wheel motion into synthetic Up/Down key events when in
/// the alternate screen, which is indistinguishable from the user's own key
/// presses and forces Up/Down to guess which one happened. Capture also lets a
/// left click on an inline preview link open its render. The trade is native
/// click-drag text selection in the terminal, which most terminals still offer
/// behind a modifier (e.g. Shift-drag).
///
/// On terminals that speak the Kitty keyboard protocol we push
/// `DISAMBIGUATE_ESCAPE_CODES` so chords like Shift+Enter arrive distinct from a
/// bare Enter; terminals without that protocol are left untouched (the composer hint still
/// advertises ⇧⏎, it just won't fire there).
fn setup_terminal() -> Result<(Terminal<CrosstermBackend<Stdout>>, bool)> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;
    let keyboard_enhancement = supports_keyboard_enhancement().unwrap_or(false);
    if keyboard_enhancement {
        execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
    }
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok((terminal, keyboard_enhancement))
}

/// Restore the terminal to its normal state. Always safe to call.
fn restore_terminal(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    keyboard_enhancement: bool,
) -> Result<()> {
    if keyboard_enhancement {
        execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags)?;
    }
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    Ok(())
}

#[cfg(test)]
mod shell_tests {
    use super::*;
    use serde_json::json;

    fn app() -> App {
        App::new(Status::new(
            "bedrock",
            "model",
            "/tmp/design.kicad_sch",
            true,
        ))
    }

    fn shell() -> Shell {
        let (events_tx, _events_rx) = unbounded_channel();
        let (done_tx, _done_rx) = unbounded_channel();
        Shell {
            runner: Runner {
                client: None,
                kicad: None,
                project_dir: PathBuf::from("/tmp/p"),
                schematic_filename: "design.kicad_sch".into(),
                budget: Budget::default(),
            },
            events_tx,
            done_tx,
            turn_task: None,
            active_task_id: None,
            next_task_id: 0,
        }
    }

    #[test]
    fn the_headline_reads_the_report_the_run_wrote() {
        let report = json!({
            "schematic": {"review": {"mean": 8.4}, "erc_errors": 0},
            "pcb": {"pcb": "/tmp/p/design.kicad_pcb", "completion": 1.0, "unrouted": 0},
        });
        assert_eq!(headline(&report), "review 8.4/10 · ERC clean · board routed");
        assert_eq!(headline(&json!({"pcb": null})), "ERC clean");
    }

    #[test]
    fn stale_completion_and_events_cannot_end_or_mutate_a_newer_turn() {
        let mut shell = shell();
        let mut app = app();
        app.update(Msg::Char('x'));
        app.update(Msg::Submit);
        shell.active_task_id = Some(2);

        shell.receive_agent_event(
            &mut app,
            1,
            AgentEvent::ToolStarted {
                name: "stale tool".into(),
                args: json!({}),
                seq: 0,
            },
        );
        shell.finish_task(&mut app, 1, TurnEndReason::Completed);

        assert!(app.running);
        assert_eq!(app.turn_tool_calls, 0);
        assert_eq!(shell.active_task_id, Some(2));
    }

    #[tokio::test]
    async fn biased_select_drains_run_events_before_the_tasks_done_signal() {
        // Regression guard for a real race: a run's events and its completion
        // signal travel on two separate channels, and a bare `select!` does not
        // preserve ordering across them — an unlucky poll can process the done
        // signal first, pushing turn 2's user entry before turn 1's own reply
        // is in the transcript. This proves `biased` (mirroring the real loop,
        // events listed above done) closes that gap even when both are ready at
        // the same instant, which is the actual race window.
        let (events_tx, mut events_rx) = unbounded_channel::<TaskEvent>();
        let (done_tx, mut done_rx) = unbounded_channel::<TaskDone>();
        let mut shell = shell();
        shell.events_tx = events_tx.clone();
        shell.done_tx = done_tx.clone();
        shell.active_task_id = Some(1);
        shell.next_task_id = 2;

        let mut app = app();
        app.transcript.push(crate::tui::app::Entry::user("first"));
        app.queued.push("second".into());
        app.running = true;

        // Both ready before the loop ever polls — the exact race window.
        let _ = events_tx.send((1, AgentEvent::AssistantText("the story".into())));
        let _ = done_tx.send((1, TurnEndReason::Completed));

        for _ in 0..2 {
            tokio::select! {
                biased;
                Some((task_id, ev)) = events_rx.recv() => {
                    shell.receive_agent_event(&mut app, task_id, ev);
                }
                Some((task_id, reason)) = done_rx.recv() => {
                    shell.finish_task(&mut app, task_id, reason);
                }
            }
        }

        let texts: Vec<&str> = app.transcript.iter().map(|e| e.text.as_str()).collect();
        let story_at = texts
            .iter()
            .position(|t| *t == "the story")
            .expect("turn 1's reply landed");
        let second_at = texts
            .iter()
            .position(|t| *t == "second")
            .expect("the queued prompt's user entry landed");
        assert!(
            story_at < second_at,
            "turn 1's reply must land before turn 2's user entry: {texts:?}"
        );
    }

    #[test]
    fn turn_completion_dispatches_the_prompt_queued_during_the_turn() {
        let mut shell = shell();
        let mut app = app();
        app.update(Msg::Char('x'));
        app.update(Msg::Submit);
        for c in "next".chars() {
            app.update(Msg::Char(c));
        }
        app.update(Msg::Submit);
        shell.active_task_id = Some(1);

        shell.finish_task(&mut app, 1, TurnEndReason::Completed);

        assert!(app.queued.is_empty());
        assert!(
            app.transcript
                .iter()
                .any(|entry| entry.text.contains("agent unavailable — cannot run a turn")),
            "the shell must execute the reducer's SpawnTurn follow-up"
        );
        assert!(
            !app.running,
            "failed spawning the follow-up closes its state"
        );
    }

    /// The commands the new core cannot back still exist and still answer.
    #[test]
    fn context_editing_commands_say_they_are_unavailable() {
        let mut shell = shell();
        let mut app = app();
        for c in "/compact".chars() {
            app.update(Msg::Char(c));
        }
        let action = app.update(Msg::Submit);
        shell.handle(&mut app, action);
        assert!(
            app.transcript
                .iter()
                .any(|e| e.text.starts_with("compact: not available in this build")),
            "{:?}",
            app.transcript
        );
        assert!(!app.running, "the unavailable command closes its turn");
    }
}
