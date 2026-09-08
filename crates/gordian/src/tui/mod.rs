//! The interactive cockpit: `gordian tui --project <dir>`.
//!
//! All design logic lives in `gordian-core`. This module is the **shell**: it
//! owns the terminal, the crossterm event stream and the run task, and wires
//! them to the testable [`app::App`] state machine and the [`ui::draw`]
//! renderer.
//!
//! ```text
//!   crossterm EventStream ─┐
//!   gordian_core::subscribe┼─ select! ─► App::update ─► Action ─► Shell
//!   animation tick        ─┘                            (spawn a run, cancel,
//!                                                        screenshot, quit)
//! ```
//!
//! One prompt is one whole `gordian_core::run::run`: the design loop, the
//! polish pass and the board. A second prompt on the same project is an edit —
//! the run opens the `design.kicad_sch` that is already there and patches it.

pub mod app;
mod image;
mod keys;
mod pricing;
mod shot;
mod theme;
mod ui;

use std::io::{self, Stdout};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    EventStream, KeyboardEnhancementFlags, MouseEventKind, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};
use futures::StreamExt;
use gordian_core::AgentEvent;
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
use app::{Action, App, Entry, Msg, Status, TurnEnd};

/// How often the shell wakes the app for the spinner and elapsed clock.
const TICK: Duration = Duration::from_millis(120);

/// Launch the cockpit over a project directory and run until the user quits.
pub async fn run(project_dir: PathBuf, loaded: LoadedConfig) -> Result<()> {
    std::fs::create_dir_all(&project_dir)
        .with_context(|| format!("creating {}", project_dir.display()))?;

    // Both halves are best-effort: a cockpit that cannot design still launches
    // and says what is missing, rather than failing at the command line.
    let kicad = crate::config::detect_kicad(&loaded.config).ok();
    let client = gordian_core::GenaiProvider::from_config(&loaded.config.llm).ok();
    let _log = gordian_runtime::logging::init_file_only(
        &project_dir,
        client.as_ref().map_or("tui", |c| c.thread_identifier()),
    );

    let (provider, model) = match &client {
        Some(client) => client.status(),
        None => ("unconfigured".to_string(), "(llm.model unset)".to_string()),
    };
    let sheet = project_dir.join(&loaded.config.project.schematic_filename);
    let mut status = Status::new(provider, model, project_dir.clone());
    status.kicad = kicad.is_some();
    status.edit_mode = sheet.is_file();

    let mut app = App::new(status);
    if kicad.is_none() || client.is_none() {
        let mut missing = Vec::new();
        if kicad.is_none() {
            missing.push("KiCad 10 (kicad.cliPath, kicad.symbolDir, kicad.footprintDir)");
        }
        if client.is_none() {
            missing.push("an LLM provider (llm.adapter, llm.model, llm.apiKey)");
        }
        app.transcript.push(Entry::Error(format!(
            "no runs possible — {} not configured. Edit {} and restart.",
            missing.join(" and "),
            loaded.path.display()
        )));
    }

    // Every stage of a run reports through the core's sink; a send failure only
    // means the cockpit is gone, which the run does not need to know.
    let (events, events_rx) = unbounded_channel();
    gordian_core::subscribe(move |event| {
        let _ = events.send(event);
    });
    let (done, done_rx) = unbounded_channel();

    let shell = Shell {
        client: client.map(Arc::new),
        kicad,
        project_dir,
        schematic_filename: loaded.config.project.schematic_filename.clone(),
        budget: Budget {
            total: Duration::from_secs(loaded.config.agent.budget_seconds),
            max_builds: loaded.config.agent.max_builds,
            ..Budget::default()
        },
        done,
        task: None,
    };

    let (mut terminal, enhanced) = setup_terminal().context("entering the alternate screen")?;
    let result = event_loop(&mut terminal, &mut app, shell, events_rx, done_rx).await;
    restore_terminal(&mut terminal, enhanced).ok();
    gordian_core::unsubscribe();
    if let Err(error) = &result {
        tracing::error!(error = %error, "the cockpit's event loop failed");
    }
    result
}

/// The side-effecting half: everything an [`Action`] needs to touch.
struct Shell {
    client: Option<Arc<gordian_core::GenaiProvider>>,
    kicad: Option<KicadInstallation>,
    project_dir: PathBuf,
    schematic_filename: String,
    budget: Budget,
    done: UnboundedSender<TurnEnd>,
    /// The run in flight, aborted by [`Action::Cancel`].
    task: Option<JoinHandle<()>>,
}

impl Shell {
    fn handle(&mut self, app: &mut App, action: Action) {
        match action {
            Action::None => {}
            Action::Quit => app.should_quit = true,
            Action::Cancel => self.cancel(app),
            Action::Screenshot => self.screenshot(app),
            Action::Spawn(prompt) => self.spawn(app, prompt),
        }
    }

    /// Start one whole design run over the project. The sheet already in the
    /// directory is what makes the second prompt an edit; the run finds it
    /// itself, so nothing here has to track turns.
    fn spawn(&mut self, app: &mut App, prompt: String) {
        let (Some(client), Some(kicad)) = (self.client.clone(), self.kicad.clone()) else {
            app.running = false;
            app.turn_started = None;
            app.transcript
                .push(Entry::Error("no runs possible in this session".to_string()));
            return;
        };
        let options = RunOptions {
            project_dir: self.project_dir.clone(),
            schematic_filename: self.schematic_filename.clone(),
            board: crate::wants_board(&prompt),
            prompt,
            budget: self.budget.clone(),
            polish: true,
            compose_rounds: 2,
        };
        let done = self.done.clone();
        self.task = Some(tokio::spawn(async move {
            let outcome = match design(&*client, &kicad, options).await {
                Ok(report) => TurnEnd::Completed(headline(&report.value)),
                Err(error) => TurnEnd::Error(format!("{error:#}")),
            };
            let _ = done.send(outcome);
        }));
    }

    /// Esc: drop the run mid-flight and close the turn ourselves, since an
    /// aborted task never reports.
    fn cancel(&mut self, app: &mut App) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        let next = app.update(Msg::TurnEnded(TurnEnd::Interrupted));
        self.handle(app, next);
    }

    fn screenshot(&self, app: &mut App) {
        let (w, h) = crossterm::terminal::size().unwrap_or((100, 30));
        let entry = match shot::capture(app, &self.project_dir, w, h) {
            Ok(path) => Entry::Note(format!("screenshot: {}", path.display())),
            Err(error) => Entry::Error(format!("screenshot failed: {error:#}")),
        };
        app.transcript.push(entry);
    }
}

impl Drop for Shell {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
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

/// Select over input, run events, the run's completion and the animation tick;
/// update the app, act on what it returns, redraw.
async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    mut shell: Shell,
    mut events_rx: UnboundedReceiver<AgentEvent>,
    mut done_rx: UnboundedReceiver<TurnEnd>,
) -> Result<()> {
    let mut input = EventStream::new();
    let mut tick = tokio::time::interval(TICK);

    terminal.draw(|f| ui::draw(f, app))?;
    loop {
        tokio::select! {
            // Events before completion: a run's last events are already queued
            // when it reports done, and draining them first keeps two turns'
            // rows from interleaving.
            biased;

            Some(event) = events_rx.recv(), if app.running => {
                app.update(Msg::Agent(event));
            }
            Some(reason) = done_rx.recv() => {
                if app.running {
                    let next = app.update(Msg::TurnEnded(reason));
                    shell.task = None;
                    shell.handle(app, next);
                }
            }
            maybe = input.next() => {
                match maybe {
                    Some(Ok(Event::Key(key))) => {
                        if let Some(msg) = keys::map_key(app, key) {
                            let action = app.update(msg);
                            shell.handle(app, action);
                        }
                    }
                    Some(Ok(Event::Mouse(m))) => match m.kind {
                        MouseEventKind::ScrollUp => { app.update(Msg::ScrollUp); }
                        MouseEventKind::ScrollDown => { app.update(Msg::ScrollDown); }
                        _ => {}
                    },
                    Some(Ok(Event::Paste(text))) => {
                        let action = app.update(Msg::Paste(text));
                        shell.handle(app, action);
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                }
            }
            _ = tick.tick() => {
                if !app.running {
                    continue;
                }
                app.update(Msg::Tick);
            }
        }

        // A run that was interrupted may still have events in flight; they
        // belong to nothing now, so they are dropped rather than attributed to
        // whatever runs next.
        if !app.running {
            while events_rx.try_recv().is_ok() {}
        }
        if app.should_quit {
            break;
        }
        terminal.draw(|f| ui::draw(f, app))?;
    }
    Ok(())
}

/// Enter raw mode and the alternate screen.
///
/// Mouse capture is on so a wheel scroll arrives as a real `Event::Mouse`,
/// distinct from an arrow key — without it most terminals synthesise Up/Down
/// key presses in the alternate screen, which is indistinguishable from the
/// user's own and forces the arrows to guess. On terminals that speak the Kitty
/// protocol, `DISAMBIGUATE_ESCAPE_CODES` makes ⇧Enter distinct from Enter.
fn setup_terminal() -> Result<(Terminal<CrosstermBackend<Stdout>>, bool)> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;
    let enhanced = supports_keyboard_enhancement().unwrap_or(false);
    if enhanced {
        execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
    }
    Ok((Terminal::new(CrosstermBackend::new(stdout))?, enhanced))
}

/// Put the terminal back the way it was. Always safe to call.
fn restore_terminal(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    enhanced: bool,
) -> Result<()> {
    if enhanced {
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
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_headline_reads_the_report_the_run_wrote() {
        let report = json!({
            "schematic": {"review": {"mean": 8.4}, "erc_errors": 0},
            "pcb": {"pcb": "/tmp/p/design.kicad_pcb", "completion": 1.0, "unrouted": 0},
        });
        assert_eq!(headline(&report), "review 8.4/10 · ERC clean · board routed");
    }

    /// A schematic-only run leaves an empty PCB section behind; the board is
    /// named only when one was written.
    #[test]
    fn a_schematic_only_run_says_nothing_about_a_board() {
        let report = json!({
            "schematic": {"erc_errors": 2},
            "pcb": {"pcb": null, "completion": 0.0, "unrouted": 0},
        });
        assert_eq!(headline(&report), "2 ERC error(s)");
        assert_eq!(headline(&json!({"pcb": null})), "ERC clean");
    }
}
