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
//!   agent AgentEvent mpsc ─┼─ tokio::select! ─► App::update ─► Action ─► shell
//!   apply-gate mpsc       ─┘                                   (spawn turn,
//!                                                               resolve gate,
//!                                                               undo, quit)
//! ```
//!
//! ### The apply-gate across tasks
//!
//! The agent runs in a spawned task holding a [`TuiApprovals`]. When it reaches
//! the apply-gate, `TuiApprovals::approve` sends the dry-run diff **plus a
//! oneshot reply channel** over `gate_tx`. The main loop receives it, shows the
//! diff in the App, and stashes the oneshot sender. When the user presses `a`/`r`
//! the loop fulfils the oneshot, unblocking the agent task. This is exactly why
//! [`agent::Approvals::approve`] is async.

pub mod app;
pub mod event;
pub mod ui;

use std::io::{self, Stdout};
use std::path::PathBuf;
use std::rc::Rc;

use agent::tools::ToolCtx;
use agent::{Agent, AgentEvent, Approvals};
use anyhow::{Context, Result};
use async_trait::async_trait;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture, Event, EventStream};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use kicad_bridge::env::KicadEnv;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use serde_json::Value;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::{Mutex, oneshot};

use app::{Action, App, Msg, Status};

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
    let (provider, model) = match agent::config::Config::from_env() {
        Ok(c) => ("bedrock".to_string(), c.model),
        Err(_) => (
            "bedrock".to_string(),
            agent::config::DEFAULT_MODEL.to_string(),
        ),
    };

    let agent_handle: Option<SharedAgent> = match (&env, agent::llm::from_env()) {
        (Some(env), Ok(client)) => {
            let ctx = ToolCtx::for_project(env.clone(), project_dir.clone())
                .context("building the tool context for the project")?;
            Some(Rc::new(Mutex::new(Agent::new(Box::new(client), ctx))))
        }
        _ => None,
    };

    let sch_path = project_dir.join("design.kicad_sch");
    let snapshots = kicad_bridge::snapshot::SnapshotStore::for_project(&project_dir).ok();

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

/// The async event loop: select over keyboard input, agent events, and gate
/// requests; update the app; act on the returned action; redraw.
async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    agent_handle: Option<SharedAgent>,
    sch_path: PathBuf,
    snapshots: Option<kicad_bridge::snapshot::SnapshotStore>,
) -> Result<()> {
    let mut keys = EventStream::new();
    let (events_tx, mut events_rx): (UnboundedSender<AgentEvent>, UnboundedReceiver<AgentEvent>) =
        unbounded_channel();
    let (gate_tx, mut gate_rx): (UnboundedSender<GateRequest>, UnboundedReceiver<GateRequest>) =
        unbounded_channel();
    // (diff, completion channel) waiting for a/r when the gate is open.
    let mut pending_gate: Option<oneshot::Sender<bool>> = None;
    // Joins back when the spawned turn finishes (so input unlocks even on error).
    let (done_tx, mut done_rx): (
        UnboundedSender<Option<String>>,
        UnboundedReceiver<Option<String>>,
    ) = unbounded_channel();

    terminal.draw(|f| ui::draw(f, app))?;

    loop {
        tokio::select! {
            // ── keyboard ──────────────────────────────────────────────
            maybe_key = keys.next() => {
                match maybe_key {
                    Some(Ok(Event::Key(key))) => {
                        if let Some(msg) = event::map_key(app, key) {
                            let action = app.update(msg);
                            handle_action(
                                app, action, &agent_handle, &events_tx, &gate_tx,
                                &done_tx, &sch_path, &snapshots, &mut pending_gate,
                            );
                        }
                    }
                    Some(Ok(_)) => {} // resize / mouse / paste: just redraw below
                    Some(Err(_)) | None => break,
                }
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
                    pending_gate = Some(reply);
                    app.update(Msg::PendingDiff(diff));
                }
            }
            // ── spawned turn finished ─────────────────────────────────
            Some(err) = done_rx.recv() => {
                app.update(Msg::TurnEnded(err));
            }
        }

        // A pending gate that's still open when we quit must be answered, or the
        // agent task would hang forever waiting on the oneshot.
        if app.should_quit {
            if let Some(reply) = pending_gate.take() {
                let _ = reply.send(false);
            }
            break;
        }
        terminal.draw(|f| ui::draw(f, app))?;
    }
    Ok(())
}

/// Perform the side effect an [`Action`] calls for.
#[allow(clippy::too_many_arguments)]
fn handle_action(
    app: &mut App,
    action: Action,
    agent_handle: &Option<SharedAgent>,
    events_tx: &UnboundedSender<AgentEvent>,
    gate_tx: &UnboundedSender<GateRequest>,
    done_tx: &UnboundedSender<Option<String>>,
    sch_path: &PathBuf,
    snapshots: &Option<kicad_bridge::snapshot::SnapshotStore>,
    pending_gate: &mut Option<oneshot::Sender<bool>>,
) {
    match action {
        Action::None => {}
        Action::Quit => {
            app.should_quit = true;
        }
        Action::ResolveApproval(decision) => {
            if let Some(reply) = pending_gate.take() {
                let _ = reply.send(decision);
            }
        }
        Action::Undo => {
            undo(app, sch_path, snapshots);
        }
        Action::SpawnTurn(prompt) => {
            let Some(handle) = agent_handle.clone() else {
                app.running = false;
                app.transcript
                    .push(app::Entry::system("agent unavailable — cannot run a turn"));
                return;
            };
            let events_tx = events_tx.clone();
            let gate_tx = gate_tx.clone();
            let done_tx = done_tx.clone();
            // spawn_local: the agent's ToolCtx is not Send, so the turn runs on
            // this thread's LocalSet rather than the shared scheduler.
            tokio::task::spawn_local(async move {
                let mut approvals = TuiApprovals { gate_tx };
                let mut agent = handle.lock().await;
                let result = agent
                    .run_turn(&prompt, &mut approvals, Some(&events_tx))
                    .await;
                let err = result.err().map(|e| format!("{e:#}"));
                let _ = done_tx.send(err);
            });
        }
    }
}

/// `:undo` — restore the previous schematic from the snapshot store.
fn undo(
    app: &mut App,
    sch_path: &PathBuf,
    snapshots: &Option<kicad_bridge::snapshot::SnapshotStore>,
) {
    let Some(store) = snapshots else {
        app.transcript
            .push(app::Entry::system("no snapshot store for this project"));
        return;
    };
    match store.undo(sch_path) {
        Ok(()) => app
            .transcript
            .push(app::Entry::system("undo: restored the previous schematic")),
        Err(e) => app
            .transcript
            .push(app::Entry::system(format!("undo failed: {e}"))),
    }
}

/// Enter raw mode + the alternate screen and build the ratatui terminal.
fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

/// Restore the terminal to its normal state. Always safe to call.
fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    Ok(())
}
