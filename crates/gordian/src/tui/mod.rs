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
//!   approval mpsc         ─┤                                   (spawn turn,
//!   animation tick        ─┘                                    resolve gate,
//!                                                               cancel,
//!                                                               quit)
//! ```
//!
//! ### Mutation approval across tasks
//!
//! The agent runs in a spawned task holding a [`TuiApprovals`]. When it reaches
//! an approval gate, `TuiApprovals::approve` sends the proposal **plus a
//! oneshot reply channel** over `gate_tx`. The main loop receives it, shows the
//! diff or operation in the App, and stashes the oneshot sender. When the user presses `a`/`r`
//! the loop fulfils the oneshot, unblocking the agent task. This is exactly why
//! [`gordian_core::Approvals::approve`] is async.

pub mod app;
pub mod event;
pub mod md;
pub mod pricing;
pub mod theme;
pub mod ui;

#[cfg(test)]
mod screenshot;

use std::future::Future;
use std::io::{self, Stdout};
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
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
use futures::{FutureExt, StreamExt};
use gordian_core::AgentRuntime;
use gordian_core::GordianConfig;
use gordian_core::prompts::system_prompt;
use gordian_core::{Agent, AgentEvent, Approvals, Provider as _, StopReason};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui_image::picker::Picker;
use serde_json::Value;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;

use app::{Action, App, Msg, Status, TurnEndReason};

/// How often the shell wakes the app for spinner/elapsed redraws.
const TICK: Duration = Duration::from_millis(120);

/// Identity of a spawned turn/compaction task. Async messages carry this so a
/// late gate or completion from an aborted task cannot affect its successor.
type TaskId = u64;
type TaskEvent = (TaskId, AgentEvent);
type TaskDone = (TaskId, TurnEndReason);

/// A pending mutation proposal and the channel the UI uses to answer it.
type GateRequest = (TaskId, Value, oneshot::Sender<bool>);

/// The [`Approvals`] implementation that bridges the agent's gate to the UI.
///
/// On `approve`, it forwards the preview/operation proposal to the main loop and awaits the
/// user's decision over a oneshot. If auto-approve is on, the loop answers
/// immediately; otherwise it waits for an `a`/`r` keypress.
struct TuiApprovals {
    task_id: TaskId,
    gate_tx: UnboundedSender<GateRequest>,
}

#[async_trait]
impl Approvals for TuiApprovals {
    async fn approve(&mut self, proposal: &Value) -> bool {
        let (tx, rx) = oneshot::channel();
        if self
            .gate_tx
            .send((self.task_id, proposal.clone(), tx))
            .is_err()
        {
            // UI is gone — fail safe (reject the write).
            return false;
        }
        rx.await.unwrap_or(false)
    }
}

/// The shared agent handle: the spawned (local) turn task locks it for the
/// turn's duration. `Rc<Mutex<...>>` keeps the TUI side single-threaded, so the
/// main loop can hold the same agent across turns.
type SharedAgent = Rc<Mutex<Agent>>;

/// Convert an unexpected panic inside a local turn task into the same explicit
/// error path as provider/tool failures. User cancellation still aborts the
/// outer task and is reported separately by `cancel_turn`.
async fn guard_turn_task(future: impl Future<Output = TurnEndReason>) -> TurnEndReason {
    match AssertUnwindSafe(future).catch_unwind().await {
        Ok(reason) => reason,
        Err(payload) => {
            let message = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("unknown panic payload");
            TurnEndReason::Error(format!("agent task panicked: {message}"))
        }
    }
}

/// Launch the cockpit over a project directory. Sets up the terminal, builds the
/// agent, and runs the event loop until the user quits.
pub async fn run(project_dir: PathBuf, config: GordianConfig, config_path: PathBuf) -> Result<()> {
    // 1. Detect KiCAD (best-effort: the UI still launches without it, just shows
    //    a disconnected indicator and the agent's tools will error).
    let env = crate::config::detect_kicad(&config);
    let kicad_connected = env.is_some();

    // 2. Build the agent if we have both KiCAD and LLM config; otherwise launch
    //    a "degraded" UI that explains what's missing (so `tui` never panics).
    let client_result = gordian_core::GenaiProvider::from_config(&config.llm);
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

    let agent_handle: Option<SharedAgent> = match (&env, client_result) {
        (Some(env), Ok(client)) => {
            let ctx = AgentRuntime::for_project_with_config(
                env.clone(),
                project_dir.clone(),
                config.clone(),
            )
            .context("building the tool context for the project")?;
            Some(Rc::new(Mutex::new(Agent::new(
                client,
                ctx,
                system_prompt(),
            ))))
        }
        _ => None,
    };

    let sch_path = project_dir.join(&config.project.schematic_filename);
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
        let mut missing = Vec::new();
        if env.is_none() {
            missing.push(
                "no KiCAD install found (install KiCAD 9+/10, or set kicad.symbolDir / \
                 kicad.footprintDir / kicad.cliPath)"
                    .to_string(),
            );
        }
        if let Some(err) = &llm_error {
            missing.push(format!("LLM provider not configured: {err}"));
        }
        app.transcript.push(app::Entry::system(format!(
            "agent unavailable — {}. Edit {} then restart. UI is read-only.",
            missing.join("; "),
            config_path.display()
        )));
    }

    // 3. Terminal setup (RAII guard restores it on any exit path).
    let (mut terminal, keyboard_enhancement) =
        setup_terminal().context("entering the alternate screen")?;
    let result = event_loop(
        &mut terminal,
        &mut app,
        agent_handle,
        config.agent.post_commit_review,
        config.agent.review_fix_rounds,
    )
    .await;
    restore_terminal(&mut terminal, keyboard_enhancement).ok();
    if let Err(error) = &result {
        tracing::error!(error = %error, "TUI event loop failed");
    }
    result
}

/// The side-effecting half of the cockpit: everything the [`Action`]s returned
/// by [`App::update`] need to touch (channels, the agent handle, the in-flight
/// turn task, and project path).
struct Shell {
    agent: Option<SharedAgent>,
    events_tx: UnboundedSender<TaskEvent>,
    gate_tx: UnboundedSender<GateRequest>,
    done_tx: UnboundedSender<TaskDone>,
    post_commit_review: bool,
    review_fix_rounds: u8,
    /// The oneshot answering the currently open mutation approval, if any.
    pending_gate: Option<oneshot::Sender<bool>>,
    /// The in-flight turn task (aborted by [`Action::CancelTurn`]).
    turn_task: Option<JoinHandle<()>>,
    /// Identity of `turn_task`; cleared before cancellation closes the App turn.
    active_task_id: Option<TaskId>,
    /// Monotonic source for task identities (wrapping is harmless in practice;
    /// zero is skipped to keep the initial state visibly distinct).
    next_task_id: TaskId,
}

impl Shell {
    /// Fail closed on every shell exit path, including terminal-stream errors:
    /// reject an unanswered mutation and stop any detached local turn.
    fn shutdown(&mut self) {
        if let Some(reply) = self.pending_gate.take() {
            let _ = reply.send(false);
        }
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
            Action::ResolveApproval(decision) => {
                if let Some(reply) = self.pending_gate.take() {
                    let _ = reply.send(decision);
                }
            }
            Action::CancelTurn => self.cancel_turn(app),
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
        let post_commit_review = self.post_commit_review;
        let review_fix_rounds = self.review_fix_rounds as usize;
        let task_id = self.begin_task();
        // spawn_local: the TUI shares the agent through Rc, so turns run on this
        // thread's LocalSet rather than the shared scheduler.
        self.turn_task = Some(tokio::task::spawn_local(async move {
            let (task_events_tx, mut task_events_rx) = unbounded_channel();
            let forwarder = tokio::task::spawn_local(async move {
                while let Some(event) = task_events_rx.recv().await {
                    let _ = events_tx.send((task_id, event));
                }
            });
            let reason = guard_turn_task(async move {
                let mut approvals = TuiApprovals { task_id, gate_tx };
                let mut agent = handle.lock().await;
                // Route through the self-correction loop: after a turn that COMMITS a
                // design change, an independent reviewer scores the netlist and feeds
                // high-confidence defects into one follow-up fix turn. The reviewer is
                // skipped on read-only/conversational turns (nothing applied). The
                // user's prompt is the design intent the reviewer judges against;
                // `ReviewStarted` / `Reviewed` events flow through `events_tx` to the
                // running detail row and transcript.
                let result = if post_commit_review {
                    agent
                        .run_turn_reviewed(
                            &prompt,
                            &prompt,
                            &mut approvals,
                            Some(&task_events_tx),
                            review_fix_rounds,
                        )
                        .await
                } else {
                    agent
                        .run_turn(&prompt, &mut approvals, Some(&task_events_tx))
                        .await
                };
                match result {
                    Ok(o) => match o.stop_reason {
                        StopReason::Completed => TurnEndReason::Completed,
                        StopReason::ProviderRequestLimit { requests } => {
                            TurnEndReason::ProviderRequestLimit { requests }
                        }
                        StopReason::MutationTimedOut => TurnEndReason::MutationTimedOut,
                        StopReason::NoProgress { completions } => {
                            TurnEndReason::NoProgress { completions }
                        }
                        StopReason::QualityGateFailed { failures } => {
                            TurnEndReason::QualityGateFailed { failures }
                        }
                    },
                    Err(e) => TurnEndReason::Error(format!("{e:#}")),
                }
            })
            .await;
            // Close and fully drain the per-task event stream before publishing
            // completion, preserving event-before-end ordering.
            let _ = forwarder.await;
            let _ = done_tx.send((task_id, reason));
        }));
    }

    /// Allocate and activate an identity before spawning a new local task.
    fn begin_task(&mut self) -> TaskId {
        self.next_task_id = self.next_task_id.wrapping_add(1).max(1);
        self.active_task_id = Some(self.next_task_id);
        self.next_task_id
    }

    /// Esc on a running turn: abort the task mid-flight. The agent gives up
    /// whatever it was doing (an LLM round-trip, a tool call). An open gate is
    /// answered "no" here, so no unapproved mutation begins.
    fn cancel_turn(&mut self, app: &mut App) {
        // Invalidate async messages before aborting. A task can have queued its
        // gate/completion immediately before this handler won the select race.
        self.active_task_id = None;
        if let Some(task) = self.turn_task.take() {
            task.abort();
        }
        if let Some(reply) = self.pending_gate.take() {
            let _ = reply.send(false);
        }
        // The aborted task never sends done_tx, so close the turn ourselves —
        // flagged as a user interruption so the indicator reads "Interrupted".
        let follow_up = app.update(Msg::TurnEnded(TurnEndReason::Interrupted));
        self.handle(app, follow_up);
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
        let task_id = self.begin_task();
        self.turn_task = Some(tokio::task::spawn_local(async move {
            let (task_events_tx, mut task_events_rx) = unbounded_channel();
            let forwarder = tokio::task::spawn_local(async move {
                while let Some(event) = task_events_rx.recv().await {
                    let _ = events_tx.send((task_id, event));
                }
            });
            let reason = guard_turn_task(async move {
                let mut agent = handle.lock().await;
                match agent.compact(Some(&task_events_tx)).await {
                    Ok(_) => TurnEndReason::Compacted,
                    Err(e) => TurnEndReason::Error(format!("{e:#}")),
                }
            })
            .await;
            let _ = forwarder.await;
            let _ = done_tx.send((task_id, reason));
        }));
    }

    /// Accept a gate only from the active task. Rejected stale requests are
    /// answered explicitly so their sender can finish if it has not been
    /// aborted yet.
    fn receive_gate(
        &mut self,
        app: &mut App,
        task_id: TaskId,
        proposal: Value,
        reply: oneshot::Sender<bool>,
    ) {
        if self.active_task_id != Some(task_id) || !app.running {
            let _ = reply.send(false);
            return;
        }
        if app.auto {
            let _ = reply.send(true);
            app.transcript
                .push(app::Entry::system("auto-approved (yolo)"));
        } else {
            self.pending_gate = Some(reply);
            app.update(Msg::PendingApproval(proposal));
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

    /// Run `f` over the agent when it exists and is idle (the turn task holds
    /// the lock for a whole turn, so `try_lock` failing means "busy").
    fn with_idle_agent<R>(&self, f: impl FnOnce(&mut Agent) -> R) -> Option<R> {
        let handle = self.agent.as_ref()?;
        let mut agent = handle.try_lock().ok()?;
        Some(f(&mut agent))
    }
}

impl Drop for Shell {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The async event loop: select over keyboard/mouse input, agent events, gate
/// requests, and the animation tick; update the app; act on the returned
/// action; redraw.
async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    agent_handle: Option<SharedAgent>,
    post_commit_review: bool,
    review_fix_rounds: u8,
) -> Result<()> {
    let mut input = EventStream::new();
    let (events_tx, mut events_rx): (UnboundedSender<TaskEvent>, UnboundedReceiver<TaskEvent>) =
        unbounded_channel();
    let (gate_tx, mut gate_rx): (UnboundedSender<GateRequest>, UnboundedReceiver<GateRequest>) =
        unbounded_channel();
    // Joins back when the spawned turn finishes (so input unlocks even on error).
    let (done_tx, mut done_rx): (UnboundedSender<TaskDone>, UnboundedReceiver<TaskDone>) =
        unbounded_channel();

    let mut shell = Shell {
        agent: agent_handle,
        events_tx,
        gate_tx,
        done_tx,
        post_commit_review,
        review_fix_rounds,
        pending_gate: None,
        turn_task: None,
        active_task_id: None,
        next_task_id: 0,
    };
    let mut tick = tokio::time::interval(TICK);

    // One image picker for the whole session: it carries the terminal's graphics
    // capability and cell font size. `None` (a dumb/piped terminal or tmux/Zellij)
    // means inline renders fall back to a text label rather than corrupting
    // scrollback with graphics escapes.
    let picker = build_picker();
    let mut ctx = ui::RenderCtx {
        picker: picker.as_ref(),
    };

    terminal.draw(|f| ui::draw_with(f, app, &mut ctx))?;

    loop {
        tokio::select! {
            // `biased` makes every poll check branches top-to-bottom instead of
            // tokio's default random order. That matters for exactly one
            // ordering guarantee: `spawn_turn` fully drains a task's own
            // `events_rx` (awaiting the forwarder) before ever sending on
            // `done_rx`, so whenever a `done_rx` item is ready, every event
            // that task sent — its final `AssistantText`, `TurnDone` — is
            // already sitting in `events_rx`. Without `biased`, an unlucky
            // poll could pick `done_rx` first anyway: `finish_task` drains the
            // queued prompt and spawns the next turn (pushing ITS user entry)
            // before the previous turn's own trailing events are processed,
            // interleaving two turns' transcript entries out of order. Listing
            // `events_rx` above `done_rx` and biasing the poll closes that gap.
            biased;

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
            // ── live agent events ─────────────────────────────────────
            Some((task_id, ev)) = events_rx.recv() => {
                shell.receive_agent_event(app, task_id, ev);
            }
            // ── mutation approval requests from the agent task ───────
            Some((task_id, proposal, reply)) = gate_rx.recv() => {
                shell.receive_gate(app, task_id, proposal, reply);
            }
            // ── spawned turn finished ─────────────────────────────────
            Some((task_id, reason)) = done_rx.recv() => {
                shell.finish_task(app, task_id, reason);
            }
        }

        // A pending gate that's still open when we quit must be answered, or the
        // agent task would hang forever waiting on the oneshot.
        if app.should_quit {
            shell.shutdown();
            break;
        }
        terminal.draw(|f| ui::draw_with(f, app, &mut ctx))?;
    }
    Ok(())
}

/// Build the session's image [`Picker`]: query the real terminal for its graphics
/// protocol + cell size, falling back to half-block rendering on a dumb/piped
/// terminal. Under tmux or Zellij we force half-blocks unconditionally — passthrough
/// graphics escapes corrupt those multiplexers' scrollback — so a preview still
/// shows, just as blocks. `None` is reserved for "no inline image at all" (none of
/// these paths hit it today, but the renderer treats `None` as text-label mode).
fn build_picker() -> Option<Picker> {
    let multiplexed = std::env::var_os("TMUX").is_some()
        || std::env::var("TERM")
            .map(|t| t.starts_with("screen") || t.contains("tmux"))
            .unwrap_or(false)
        || std::env::var_os("ZELLIJ").is_some();
    if multiplexed {
        return Some(Picker::halfblocks());
    }
    Some(Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks()))
}

/// Enter raw mode + the alternate screen and build the ratatui terminal.
///
/// Mouse capture is on so a genuine wheel scroll arrives as a real
/// `Event::Mouse`, distinct from an arrow-key press — without it, most
/// terminals translate wheel motion into synthetic Up/Down key events when in
/// the alternate screen, which is indistinguishable from the user's own key
/// presses and forces Up/Down to guess which one happened. The trade is
/// native click-drag text selection in the terminal, which most terminals
/// still offer behind a modifier (e.g. Shift-drag).
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
        let (gate_tx, _gate_rx) = unbounded_channel();
        let (done_tx, _done_rx) = unbounded_channel();
        Shell {
            agent: None,
            events_tx,
            gate_tx,
            done_tx,
            post_commit_review: false,
            review_fix_rounds: 0,
            pending_gate: None,
            turn_task: None,
            active_task_id: None,
            next_task_id: 0,
        }
    }

    #[tokio::test]
    async fn dropping_shell_rejects_an_unanswered_approval() {
        let (events_tx, _events_rx) = unbounded_channel();
        let (gate_tx, _gate_rx) = unbounded_channel();
        let (done_tx, _done_rx) = unbounded_channel();
        let (reply, answer) = oneshot::channel();
        let shell = Shell {
            agent: None,
            events_tx,
            gate_tx,
            done_tx,
            post_commit_review: false,
            review_fix_rounds: 0,
            pending_gate: Some(reply),
            turn_task: None,
            active_task_id: None,
            next_task_id: 0,
        };

        drop(shell);

        assert!(!answer.await.expect("shell sends an explicit decision"));
    }

    #[tokio::test]
    async fn panicking_local_turn_becomes_an_explicit_error() {
        let reason = guard_turn_task(async {
            panic!("simulated turn panic");
        })
        .await;

        assert_eq!(
            reason,
            TurnEndReason::Error("agent task panicked: simulated turn panic".into())
        );
    }

    #[tokio::test]
    async fn stale_gate_after_cancellation_is_rejected_without_opening_the_card() {
        let mut shell = shell();
        let mut app = app();
        shell.active_task_id = Some(2);
        app.running = true;
        let (reply, answer) = oneshot::channel();

        shell.receive_gate(
            &mut app,
            1,
            serde_json::json!({"operation": "write"}),
            reply,
        );

        assert!(
            !answer
                .await
                .expect("stale gate receives an explicit rejection")
        );
        assert!(app.pending.is_none());
        assert_eq!(shell.active_task_id, Some(2));
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
            },
        );
        shell.finish_task(&mut app, 1, TurnEndReason::Completed);

        assert!(app.running);
        assert_eq!(app.turn_tool_calls, 0);
        assert_eq!(shell.active_task_id, Some(2));
    }

    #[tokio::test]
    async fn biased_select_drains_agent_events_before_the_tasks_done_signal() {
        // Regression guard for a real race: `AgentEvent`s (a turn's final
        // `AssistantText`, `TurnDone`) and its completion signal
        // (`TurnEndReason`, which drains the queue and spawns the next turn)
        // travel on two separate channels. `spawn_turn` guarantees it SENDS
        // to `events_tx` before `done_tx` (it awaits the forwarder first),
        // but a bare `select!` does not preserve ordering across different
        // channels — an unlucky poll can process the done signal first,
        // pushing turn 2's user entry before turn 1's own reply is in the
        // transcript. This proves `biased` (mirroring the real loop, events
        // listed above done) closes that gap even when both are ready at the
        // same instant, which is the actual race window.
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
}
