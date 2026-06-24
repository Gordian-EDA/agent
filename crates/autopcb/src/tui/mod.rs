//! The ratatui copilot cockpit (spec §11).
//!
//! ## Architecture
//!
//! All agent logic lives in the `agent` crate. This module is the **shell**: it
//! owns the terminal, the crossterm event stream, and the spawned agent task,
//! and wires them to the testable [`app::App`] state machine and the
//! [`ui::draw`] renderer.
//!
//! ```text
//!   crossterm EventStream ─┐
//!   agent AgentEvent mpsc ─┼─ tokio::select! ─► App::update ─► Action ─► Shell
//!   apply-gate mpsc       ─┤                                   (spawn turn,
//!   animation tick        ─┘                                    resolve gate,
//!                                                               cancel, undo,
//!                                                               quit)
//! ```
//!
//! ### The apply-gate across tasks
//!
//! The agent runs in a spawned task holding a [`TuiApprovals`]. When it reaches
//! the apply-gate, `TuiApprovals::approve` sends the dry-run diff **plus a
//! oneshot reply channel** over `gate_tx`. The main loop receives it, shows the
//! diff in the App, and stashes the oneshot sender. When the user presses `a`/`r`
//! the loop fulfils the oneshot, unblocking the agent task. This is exactly why
//! [`gordian_core::Approvals::approve`] is async.

pub mod app;
pub mod event;
pub mod md;
pub mod ui;

#[cfg(test)]
mod screenshot;

use std::io::{self, Stdout};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use gordian_core::{Agent, AgentEvent, Approvals, StopReason};
use gordian_kicad::PcbTools;
use gordian_kicad::prompts::system_prompt;
use gordian_kicad::tools::PcbToolCtx;
use anyhow::{Context, Result};
use async_trait::async_trait;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyboardEnhancementFlags,
    MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};
use futures::StreamExt;
use kicad_cli_rs::env::KicadEnv;
use kicad_sexpr::snapshot::SnapshotStore;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use serde_json::Value;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;

use app::{Action, App, Msg, Status, TurnEndReason};

/// How often the shell wakes the app for spinner/elapsed redraws.
const TICK: Duration = Duration::from_millis(120);

/// A pending apply-gate request: the dry-run diff and the channel the UI uses to
/// answer it.
type GateRequest = (Value, oneshot::Sender<bool>);

/// The [`Approvals`] implementation that bridges the agent's gate to the UI.
///
/// On `approve`, it forwards the dry-run diff to the main loop and awaits the
/// user's decision over a oneshot. If auto-approve is on, the loop answers
/// immediately; otherwise it waits for an `a`/`r` keypress.
struct TuiApprovals {
    gate_tx: UnboundedSender<GateRequest>,
}

#[async_trait]
impl Approvals for TuiApprovals {
    async fn approve(&mut self, diff: &Value) -> bool {
        let (tx, rx) = oneshot::channel();
        if self.gate_tx.send((diff.clone(), tx)).is_err() {
            // UI is gone — fail safe (reject the write).
            return false;
        }
        rx.await.unwrap_or(false)
    }
}

/// The shared agent handle: the spawned (local) turn task locks it for the
/// turn's duration. `Rc<Mutex<…>>` — single-threaded, since the agent's tool
/// context is not `Send` — so the main loop can hold it across turns.
type SharedAgent = Rc<Mutex<Agent>>;

/// Launch the cockpit over a project directory. Sets up the terminal, builds the
/// agent, and runs the event loop until the user quits.
pub async fn run(project_dir: PathBuf) -> Result<()> {
    // 1. Detect KiCAD (best-effort: the UI still launches without it, just shows
    //    a disconnected indicator and the agent's tools will error).
    let env = KicadEnv::detect();
    let kicad_connected = env.is_some();

    // 2. Build the agent if we have both KiCAD and credentials; otherwise launch
    //    a "degraded" UI that explains what's missing (so `tui` never panics).
    let (provider, model) = llm_client::config::provider_status();

    let agent_handle: Option<SharedAgent> = match (&env, llm_client::from_env()) {
        (Some(env), Ok(client)) => {
            let ctx = PcbToolCtx::for_project(env.clone(), project_dir.clone())
                .context("building the tool context for the project")?;
            Some(Rc::new(Mutex::new(Agent::new(
                client,
                Box::new(PcbTools::new(ctx)),
                system_prompt(),
            ))))
        }
        _ => None,
    };

    let sch_path = project_dir.join("design.kicad_sch");
    let snapshots = SnapshotStore::for_project(&project_dir).ok();

    let mut status = Status::new(
        provider,
        model,
        sch_path.display().to_string(),
        kicad_connected,
    );
    if env.is_none() {
        status.kicad_connected = false;
    }
    let mut app = App::new(status);
    app.transcript.push(app::Entry::system(format!(
        "project: {} — schematic: {}",
        project_dir.display(),
        sch_path.display()
    )));
    if agent_handle.is_none() {
        app.transcript.push(app::Entry::system(
            "agent unavailable: need KiCAD + AWS_BEARER_TOKEN_BEDROCK (set in .env). UI is read-only.",
        ));
    }

    // 3. Terminal setup (RAII guard restores it on any exit path).
    let mut terminal = setup_terminal().context("entering the alternate screen")?;
    let result = event_loop(&mut terminal, &mut app, agent_handle, sch_path, snapshots).await;
    restore_terminal(&mut terminal).ok();
    result
}

/// The side-effecting half of the cockpit: everything the [`Action`]s returned
/// by [`App::update`] need to touch (channels, the agent handle, the in-flight
/// turn task, the snapshot store).
struct Shell {
    agent: Option<SharedAgent>,
    events_tx: UnboundedSender<AgentEvent>,
    gate_tx: UnboundedSender<GateRequest>,
    done_tx: UnboundedSender<TurnEndReason>,
    sch_path: PathBuf,
    snapshots: Option<SnapshotStore>,
    /// The oneshot answering the currently open apply-gate, if any.
    pending_gate: Option<oneshot::Sender<bool>>,
    /// The in-flight turn task (aborted by [`Action::CancelTurn`]).
    turn_task: Option<JoinHandle<()>>,
}

impl Shell {
    /// Perform the side effect an [`Action`] calls for.
    fn handle(&mut self, app: &mut App, action: Action) {
        match action {
            Action::None => {}
            Action::Quit => {
                app.should_quit = true;
            }
            Action::ResolveApproval(decision) => {
                if let Some(reply) = self.pending_gate.take() {
                    let _ = reply.send(decision);
                }
            }
            Action::CancelTurn => self.cancel_turn(app),
            Action::Undo => self.undo(app),
            Action::ClearContext => self.clear_context(app),
            Action::OpenUnwind => self.open_unwind(app),
            Action::UnwindTo(k) => self.unwind_to(app, k),
            Action::Compact => self.spawn_compact(app),
            Action::ShowContext => self.show_context(app),
            Action::SpawnTurn(prompt) => self.spawn_turn(app, prompt),
        }
    }

    fn spawn_turn(&mut self, app: &mut App, prompt: String) {
        let Some(handle) = self.agent.clone() else {
            app.running = false;
            app.turn_started = None;
            app.transcript
                .push(app::Entry::system("agent unavailable — cannot run a turn"));
            return;
        };
        let events_tx = self.events_tx.clone();
        let gate_tx = self.gate_tx.clone();
        let done_tx = self.done_tx.clone();
        // spawn_local: the agent's PcbToolCtx is not Send, so the turn runs on
        // this thread's LocalSet rather than the shared scheduler.
        self.turn_task = Some(tokio::task::spawn_local(async move {
            let mut approvals = TuiApprovals { gate_tx };
            let mut agent = handle.lock().await;
            let result = agent
                .run_turn(&prompt, &mut approvals, Some(&events_tx))
                .await;
            let reason = match result {
                Ok(o) => match o.stop_reason {
                    StopReason::Completed => TurnEndReason::Completed,
                    StopReason::IterationCap => TurnEndReason::IterationCap,
                },
                Err(e) => TurnEndReason::Error(format!("{e:#}")),
            };
            let _ = done_tx.send(reason);
        }));
    }

    /// Esc on a running turn: abort the task mid-flight. The agent gives up
    /// whatever it was doing (an LLM round-trip, a tool call) but nothing has
    /// been written — writes only happen behind the gate, and an open gate is
    /// answered "no" here.
    fn cancel_turn(&mut self, app: &mut App) {
        if let Some(task) = self.turn_task.take() {
            task.abort();
        }
        if let Some(reply) = self.pending_gate.take() {
            let _ = reply.send(false);
        }
        // The aborted task never sends done_tx, so close the turn ourselves —
        // flagged as a user interruption so the indicator reads "Interrupted".
        app.update(Msg::TurnEnded(TurnEndReason::Interrupted));
    }

    /// `/undo` — restore the previous schematic from the snapshot store.
    fn undo(&self, app: &mut App) {
        let Some(store) = &self.snapshots else {
            app.transcript
                .push(app::Entry::system("no snapshot store for this project"));
            return;
        };
        match store.undo(&self.sch_path) {
            Ok(()) => app
                .transcript
                .push(app::Entry::system("undo: restored the previous schematic")),
            Err(e) => app
                .transcript
                .push(app::Entry::system(format!("undo failed: {e}"))),
        }
    }

    /// `/clear` — the transcript is already wiped; drop the agent's history
    /// too so the next turn truly starts fresh.
    fn clear_context(&self, app: &mut App) {
        let dropped = self.with_idle_agent(|agent| {
            let messages = agent.context_stats().messages;
            agent.clear_history();
            messages
        });
        app.status.ctx_tokens = 0;
        let note = match dropped {
            Some(n) => format!("cleared transcript and agent context ({n} messages dropped)"),
            None => "transcript cleared (agent unavailable or busy — context untouched)".into(),
        };
        app.transcript.push(app::Entry::system(note));
    }

    /// Double-Esc — fetch the agent's unwindable turns and open the picker. When
    /// the agent is busy (or has nothing to offer) `open_unwind` notes it.
    fn open_unwind(&self, app: &mut App) {
        let turns = self.with_idle_agent(|agent| agent.unwindable_turns());
        app.open_unwind(turns.unwrap_or_default());
    }

    /// The picker was confirmed — pop `k` of the agent's most recent turns and
    /// roll the transcript back over however many were actually dropped.
    fn unwind_to(&self, app: &mut App, k: usize) {
        let popped = self.with_idle_agent(|agent| agent.pop_turns(k));
        app.apply_unwind_to(popped.unwrap_or(0));
    }

    /// `/context` — print project paths and context/token stats.
    fn show_context(&self, app: &mut App) {
        let stats = self.with_idle_agent(|agent| agent.context_stats());
        let s = &app.status;
        let mut lines = vec![
            format!("schematic: {}", s.sch_path),
            format!(
                "model: {} ({}) · turns {} · applied {}",
                s.model, s.provider, s.turn_count, s.applied_count
            ),
            format!(
                "tokens: ctx {} · session {} in / {} out",
                s.ctx_tokens, s.total_input_tokens, s.total_output_tokens
            ),
        ];
        match stats {
            Some(c) => lines.push(format!(
                "context: {} unwindable turns · {} messages · ~{}k chars",
                c.turns,
                c.messages,
                c.approx_chars / 1000
            )),
            None => lines.push("context: agent unavailable or busy".into()),
        }
        for l in lines {
            app.transcript.push(app::Entry::system(l));
        }
    }

    /// `/compact` — run the agent's context compaction with turn plumbing
    /// (running flag is already set; `done_tx` clears it via `TurnEnded`).
    fn spawn_compact(&mut self, app: &mut App) {
        let Some(handle) = self.agent.clone() else {
            app.running = false;
            app.turn_started = None;
            app.transcript
                .push(app::Entry::system("agent unavailable — nothing to compact"));
            return;
        };
        let events_tx = self.events_tx.clone();
        let done_tx = self.done_tx.clone();
        self.turn_task = Some(tokio::task::spawn_local(async move {
            let mut agent = handle.lock().await;
            let reason = match agent.compact(Some(&events_tx)).await {
                Ok(_) => TurnEndReason::Compacted,
                Err(e) => TurnEndReason::Error(format!("{e:#}")),
            };
            let _ = done_tx.send(reason);
        }));
    }

    /// Run `f` over the agent when it exists and is idle (the turn task holds
    /// the lock for a whole turn, so `try_lock` failing means "busy").
    fn with_idle_agent<R>(&self, f: impl FnOnce(&mut Agent) -> R) -> Option<R> {
        let handle = self.agent.as_ref()?;
        let mut agent = handle.try_lock().ok()?;
        Some(f(&mut agent))
    }
}

/// The async event loop: select over keyboard/mouse input, agent events, gate
/// requests, and the animation tick; update the app; act on the returned
/// action; redraw.
async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    agent_handle: Option<SharedAgent>,
    sch_path: PathBuf,
    snapshots: Option<SnapshotStore>,
) -> Result<()> {
    let mut input = EventStream::new();
    let (events_tx, mut events_rx): (UnboundedSender<AgentEvent>, UnboundedReceiver<AgentEvent>) =
        unbounded_channel();
    let (gate_tx, mut gate_rx): (UnboundedSender<GateRequest>, UnboundedReceiver<GateRequest>) =
        unbounded_channel();
    // Joins back when the spawned turn finishes (so input unlocks even on error).
    let (done_tx, mut done_rx): (
        UnboundedSender<TurnEndReason>,
        UnboundedReceiver<TurnEndReason>,
    ) = unbounded_channel();

    let mut shell = Shell {
        agent: agent_handle,
        events_tx,
        gate_tx,
        done_tx,
        sch_path,
        snapshots,
        pending_gate: None,
        turn_task: None,
    };
    let mut tick = tokio::time::interval(TICK);

    terminal.draw(|f| ui::draw(f, app))?;

    loop {
        tokio::select! {
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
                            _ => {}
                        }
                    }
                    Some(Ok(_)) => {} // resize / paste: just redraw below
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
            // ── live agent events ─────────────────────────────────────
            Some(ev) = events_rx.recv() => {
                app.update(Msg::Agent(ev));
            }
            // ── apply-gate requests from the agent task ───────────────
            Some((diff, reply)) = gate_rx.recv() => {
                if app.auto {
                    // Yolo mode: approve immediately, never show the gate.
                    let _ = reply.send(true);
                    app.transcript.push(app::Entry::system("auto-approved (yolo)"));
                } else {
                    shell.pending_gate = Some(reply);
                    app.update(Msg::PendingDiff(diff));
                }
            }
            // ── spawned turn finished ─────────────────────────────────
            Some(reason) = done_rx.recv() => {
                shell.turn_task = None;
                app.update(Msg::TurnEnded(reason));
            }
        }

        // A pending gate that's still open when we quit must be answered, or the
        // agent task would hang forever waiting on the oneshot.
        if app.should_quit {
            if let Some(reply) = shell.pending_gate.take() {
                let _ = reply.send(false);
            }
            if let Some(task) = shell.turn_task.take() {
                task.abort();
            }
            break;
        }
        terminal.draw(|f| ui::draw(f, app))?;
    }
    Ok(())
}

/// Enter raw mode + the alternate screen and build the ratatui terminal.
///
/// On terminals that speak the Kitty keyboard protocol we push
/// `DISAMBIGUATE_ESCAPE_CODES` so chords like Shift+Enter arrive distinct from a
/// bare Enter; legacy terminals are left untouched (the composer hint still
/// advertises ⇧⏎, it just won't fire there).
fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    if supports_keyboard_enhancement().unwrap_or(false) {
        execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
    }
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

/// Restore the terminal to its normal state. Always safe to call.
fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    if supports_keyboard_enhancement().unwrap_or(false) {
        execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags)?;
    }
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    Ok(())
}
