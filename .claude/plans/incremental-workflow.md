# Incremental, agent-driven design workflow (2026-09-02)

User decision: "instead of having super powerful deterministic algorithms, the Agent should take more
charge. A large PCB board is not created one shot but incrementally. Encourage partial states.
Direct the agent with workflows like human engineers: place important parts first, try route, if not,
re-place." Quality first; time later via parallel subagents; schematic too.

## Principles
1. The unit of work is a small step against a visible partial state. The agent runs the loop.
2. Deterministic code = local helpers: fast, scoped, honest. No global one-shot solver decides a board.
3. Partial states are legal and first-class: staging area for unplaced parts, ratsnest for unrouted
   nets, a bench for unplaced schematic symbols. Nothing refuses "because incomplete".
4. Every tool returns what it did AND what it could not, with the blocker and a way out. Refusal is
   reserved for edits that would silently change connectivity.
5. Progress, not pass/fail: `check_*` reports routed 34/52, three blocked by X, and what frees each.
6. The workflow lives in the prompt as phases a human follows, with a render + check after each phase.
7. Time is recorded, not gated. The wall clock hands back the partial state and the next step; the
   next turn continues. Speed comes later from batching per call, parallel tool calls, short outputs
   and parallel subagents.

## PCB state model
- Board file is the state. Unplaced parts live in a staging row outside the outline (`gordian:staged`
  property); `get_board` lists `staged`, `placed`, `locked`.
- Ratsnest = unrouted connections from the netlist minus copper connectivity; `get_board{net}` shows
  pads, existing copper, and, when a route attempt failed, the blocking geometry (pad/track/zone,
  layer, cell) and suggestions (move X, drop to layer, widen channel).
- Locks: `lock_parts{refs}` — locked parts never move in any helper; outline and mechanical parts are
  locked by default once placed.

## PCB tool surface (final)
- `sync_board` — schematic→board delta only; new parts land staged.
- `place_parts_pcb{refs, intent}` (rename of `place_board{refs}`) — local placement of the named refs
  around anchors/edges by intent; whole-board form gone. Returns placed/unplaced with reasons.
- `move_parts`, `rotate_parts`, `lock_parts`, `unlock_parts`.
- `route_nets{nets, layer?, width?}` — routes the named nets; partial success is success; returns
  routed/blocked with blockers. `route_track` for a hand-drawn track. `delete_copper{nets|bbox}`.
- `pour{nets}` explicit; `check_board` = DRC + progress; `render_board` each phase; `export_fab`.
- Every mutator: guard against connectivity change, revision captured, undo.

## PCB workflow (prompt phases)
1. Outline + connectors/mechanical placed by intent and locked.
2. Big ICs placed by intent; render.
3. Satellites (decoupling, crystal, feedback, pull-ups) placed tight to their anchors.
4. Critical nets routed while the board is empty: power, crystal, diff pairs; check; adjust placement
   if blocked (move + re-route the blocked nets only).
5. Remaining parts placed around what exists; remaining nets routed in batches; pours; DRC loop.
6. Fab export only when `check_board` is clean; otherwise report the partial state honestly.

## Schematic: same treatment
- `add_parts{parts}` puts symbols on the bench (unplaced) with pins wired by NAME (nets), no layout.
- `connect`/`label`/`no_connect` edit connectivity incrementally; never touch layout.
- `arrange{refs|block|region, intent}` lays out a set of symbols locally and draws their wires FROM
  THE NETLIST (so it cannot change connectivity, hence never refuses on that ground); reports overlap
  and unrouted nets it left as labels.
- `place_parts` = `add_parts` + `arrange` convenience; unresolved footprints/aliases never block.
- Workflow phases: power entry → regulator → MCU core (decoupling, crystal, reset, boot) → interfaces →
  connectors/indicators; render after each; the VLM critic judges against human references.
- Later lever for looks: idiom tiles (hand-designed sub-layouts with pre-drawn wires for common
  motifs) arranged as rigid units.

## Harness (quality first)
- Rubric: complete (all requested parts), ERC 0 / DRC 0 / unconnected 0, fab files, sch critic ≥ 8,
  pcb critic ≥ 8, human-look judge on the render. `agent_seconds` recorded, not a check.
- Multi-turn continuation: when a turn ends on the wall clock the harness sends "continue" (bounded
  number of turns) and grades the final state; the transcript shows the phases.
- Per-phase render gallery in artifacts; findings harvest per phase.

## Lanes
- W1 (Opus) PCB partial-state model + progress checks + blockers report + locks; whole-board forms
  removed; tests on corpus boards incl. mcu-board (27) and soc-system (69).
- W2 (codex) PCB workflow prompt + continuation across turns + per-phase render; harness rubric.
- W3 (Opus) Schematic bench/`add_parts` + netlist-drawn `arrange` + `place_parts` as convenience.
- W4 (codex) Harness: quality-first rubric, multi-turn continuation, phase gallery, human-look judge.
- Later: idiom tiles; parallel subagents per block for speed.
