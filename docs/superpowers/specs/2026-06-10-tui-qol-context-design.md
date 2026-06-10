# TUI QoL, persistent context, and non-blocking tools — design

Date: 2026-06-10
Status: approved-by-default (user request was a concrete feature list; decisions
flagged inline were made autonomously and are called out in the final report).

## Problems being solved

1. **The `.sch` is hard to find.** `autopcb tui` without `--project` writes to
   `$TMPDIR/autopcb-tui/design.kicad_sch`, and the UI only shows the basename.
2. **No conversation memory.** `Agent::run_turn` seeds a fresh
   `vec![Message::user(msg)]` every turn; the model never sees prior turns.
3. **`apply_design` freezes the UI.** The whole TUI runs on one thread
   (current-thread runtime + `LocalSet`); `Tools::run` is synchronous and
   compiles/renders/lifts/forks `kicad-cli` inline, blocking redraws for seconds.
4. **TUI ergonomics**: `:` commands with no completion, no token feedback, no
   way to rewind context.
5. **Agent can't answer "can you see my schematic at PATH?"** — no tool reads
   outside the project schematic, and nothing reports project paths.

## Design

### A. Project dir defaults to CWD

`autopcb tui` (no `--project`) uses `std::env::current_dir()` (fallback `"."`),
so the schematic is `./design.kicad_sch`. A startup transcript line prints the
full project dir + schematic path. The headless `agent` subcommand keeps its
`autopcb-project` default (already CWD-relative).

### B. Persistent agent context

`Agent` gains:

- `history: Vec<Message>` — the whole session transcript, appended per turn.
- `turn_starts: Vec<usize>` — history index at each user turn, for unwind.
- `repair_history()` — run at turn start: a cancelled turn can leave a trailing
  assistant message with dangling `tool_use` blocks (Converse rejects that).
  Repair appends synthetic `tool_result`s ("cancelled") and, if the history
  ends in a user message, a synthetic assistant "(turn cancelled)" so roles
  keep alternating.
- `pop_last_turn() -> bool` — truncate to the last turn start (double-Esc).
- `clear_history()`, `context_stats() -> (messages, approx_chars)`.
- `compact(events) -> (before_msgs, after_msgs)` — one tool-less LLM call that
  summarizes the history, then replaces it with a `[user summary, assistant
  ack]` pair. Emits `AgentEvent::Compacted`.

### C. Token usage surface

`Completion` gains `input_tokens`/`output_tokens` parsed from the Converse
`usage` object (0 when absent). The loop emits `AgentEvent::Usage` after each
completion. The TUI status bar shows the live context size as
`ctx 23.4k (12%)` against a 200k window constant.

### D. `/` commands, completion, unwind

- Command prefix becomes `/` (typing a `:`-line gets a migration hint).
  Commands: `/help /auto /undo /clear /context /compact /quit`.
- `/clear` clears the transcript **and** the agent history (full context
  reset). `/context` prints paths, message counts and token stats. `/compact`
  runs B's compaction with the usual spinner.
- Tab completion: when the input is a `/`-prefix with no space, a popup lists
  matching commands with descriptions; Tab cycles through matches, filling the
  input; any edit resets the cycle.
- Esc layering becomes: close help → reject gate → clear input → cancel
  running turn → **arm unwind** (hint shown) → second Esc unwinds the last
  exchange (agent `pop_last_turn` + transcript rollback to before the last user
  entry). Context-only: files are untouched (`/undo` restores the schematic).
  **Esc no longer quits**; quitting is `Ctrl-C` or `/quit`.

### E. New tools

- `project_info {}` → `{project_dir, sch_path, sch_exists, snapshots, cwd}`.
- `read_schematic {path}` → lift any `.kicad_sch` (absolute, `~`-expanded, or
  project-relative) to circuit-YAML; structured errors for missing/wrong-ext
  paths so the model self-repairs. Notes when the path *is* the project
  schematic.
- `apply_design` commit results gain `"path"` so the model can tell the user
  where the file landed.

### F. Non-blocking tool execution

- `RealSymbolProvider`: `RefCell<HashMap>` → `std::sync::Mutex<HashMap>`,
  `elsa::FrozenMap` → `elsa::sync::FrozenMap` (same stable-address contract).
- `ToolCtx`: `OnceCell<SymbolIndex>` → `std::sync::OnceLock<SymbolIndex>`;
  compile-time assert `ToolCtx: Send + Sync`.
- `Agent` holds `Arc<ToolCtx>`; every tool execution goes through
  `tokio::task::spawn_blocking`, so the current-thread UI runtime keeps
  redrawing while compiles/ERC subprocesses run on the blocking pool. The
  LLM call was already async. Architecture of the TUI (LocalSet + Rc) stays.

## Testing

TDD throughout: mock-LLM tests for history persistence/repair/unwind/compact
and usage events; unit tests for new tools (KiCAD-gated like existing ones);
App state-machine tests for `/` commands, completion cycling, esc-arming,
unwind rollback; render tests for the popup, hints, and token display.
Existing `:`-command and Esc-quits tests are updated to the new contract.
