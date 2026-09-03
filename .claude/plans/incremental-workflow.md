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
- Every mutator guards against connectivity changes and uses only a same-call snapshot for refusal rollback.

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

## Review deltas (fresh-context Opus, 2026-09-02) — adopted
- FIRST (W0): partial commit + progress on `place_parts` and `sync_board`. `place_parts` never rejects a
  whole payload: malformed intent entries dropped with warnings; parts whose pins cannot resolve go to
  the bench and are reported; `dangling` NEVER refuses (bulk.rs:287 contradicts prompts.rs:23); an
  engine short (`refused_place`) commits connectivity and leaves the symbols on the bench instead of
  discarding the netlist. `sync_board` syncs compatible parts and stages mismatched ones (sync.rs:359).
- Delete the three PCB preconditions: `route_board` refusing on any DRC violation (route.rs:500) and
  while parts are unplaced (route.rs:254); `place_board` refusing when nothing is unplaced (place.rs:2517).
  Keep `place_board{refs|bbox}` / `route_board{nets|bbox}` signatures (no rename; whole-board = all).
- Staging = the existing seed row (snapshot.rs:79) + `staged_reason`; staged parts excluded from DRC and
  gerbers; `check_board` reports `staged: n` as progress. No second `gordian:staged` property.
- Ratsnest shape, one for `get_board{net}` and `route_board`: `{net, from:{ref,pad,x,y,layer}, to:{…},
  status: open|routed|blocked, blocker?:{kind: pad|track|via|zone|courtyard, owner_ref, net, layer, at,
  gap_mm, need_mm}, escapes:[…]}` built on diagnose.rs unrouted_report/obstruction_between.
- Locks = KiCAD native `locked` + `locked_reason: mechanical|agent|user` (revocable).
- User decision removed revision history, `checkpoint{label}`, `expect_revision`, and undo. Keep
  `reserve_refs{prefix,count}` independently; each block gets its own bench rectangle.
- Bench (schematic) = reserved rectangle + `gordian:bench=1`; excluded from check/critic/render; bench
  pins carry net labels; `export_fab`/done FAIL while non-empty.
- Workflow: pour GND + fan out vias EARLY (phase 3/4, not last); add explicit layer-count choice and
  header/GPIO pin-swap back into the schematic as phase-4 levers (the 2×20-header + QFN board fails at
  the QFN escape otherwise; `escape_bottleneck` already detects it).
- Cut: `place_parts_pcb` rename; idiom tiles deferred until the first typesetter is honest.

## Status 2026-09-02 (evening)
- Merged: place-accept (pin aliases, non-blocking footprints, lenient intent, shared footprint policy;
  refusals 9/11→3/10 stm32, 12/15→2/6 esp32), W4 harness (quality-first gates, `--max-turns`
  continuation, `turns[]`, gallery.html, human-look judge vs KiCAD demos — small cases score 4–5/10).
- Running: W1 PCB partial state (Opus), W2 workflow phases + handoff (codex), engine-shorts (Opus),
  bluepill-erc (Opus). Next: W3 schematic bench + netlist-drawn arrange + place_parts partial commit
  (after bluepill merges); then a full campaign under the new rubric.
- W2 merged: phased prompt (schematic blocks; board phases with explicit layer count, early GND pour,
  blocked-net-only rerouting, pin-swap lever), `## Partial state`/`## Next steps` handoff at every
  budget stop, `--input -` multi-turn continuation, `phase-render:` lines; auto-finish-PCB stages
  deleted (the model owns the loop). Observed: led-driver now DRC 0 + 17 fab files; stm32 48 parts
  ERC 2 then a clean handoff at 270 s (blocked by `place_parts` "disturbed existing GND" → W3/W0).
- W1 merged: staging = seed row (`staged_reason`), the three preconditions deleted, one ratsnest shape
  (`open|routed|blocked` + blocker geometry) shared by get_board/check_board/route_board, native locks
  + `locked_reason` + `reserve_refs`, `sync_board` stages footprint-mismatched parts. The later user
  decision removed revisions, checkpoint, expect_revision, and undo. Quality: local-board-move 10,
  replace-pcb-component 9, finish-existing-pcb 7/7; led-driver fails only on schematic looks.
  Open: `next_refdes` must consult the `reserve_refs` store (W3); human-look PCB reference board fails
  to render (harness fix).
- Harness reference fallback merged (PCB ref: demos/microwave; sch ref: sallen_key). First honest
  human-look PCB verdict on the LED driver: 4/10 — "reference designators scattered far from their
  footprints", oversized outline, silkscreen alignment → queue a PCB-looks lane (silk placement next
  to footprints, outline refit, alignment) after W3.
- Comparison data (user's ~/sch-agent, NOT ported — "idiom cells are kinda cheating"): BluePill 49 symbols
  ERC 0 in minutes; audio 68 symbols ERC 0 in 138 s / 3 builds; BMS 90 symbols in 147 s but 14
  power_pin_not_driven + 1 pin_to_pin, and the render is rows of isolated label-chains (no circuit
  topology drawn). Quality bar kept: titled blocks, notes, pin-map design JSON, 2–5 build loop.
- EXPERIMENT lane/sch-drag (Opus, user idea): KiCAD-style drag as the primitive — move/rotate/mirror a
  symbol, carry its connections as clean orthogonal re-draws of only the attached segments, netlist
  identical; cheap sheet evaluator (length, bends, crossings, through-body, text, label debits,
  adjacency); `tidy` = local search over drag moves; harness vs scrambled human sheets + our BluePill.
  Integrate into move_symbols + a `tidy_schematic` tool only if it beats the engines on the critic.
- engine-shorts merged: port pennants settled once (`plan_port_exits`), second `place_parts` of a
  session now sees the existing sheet (foreign pins/rows fed to router, rails, retraction and the
  net_conflicts audit); short refusals 0 on stm32/bms. Pre-existing on main (lane/snapshot-rebless):
  esp32 netlist fixture dangling IO25/IO32 after the KiCAD-10 pin rename; six placement snapshots.
- USER: undo/history/checkpoint + revision store REMOVED (lane/remove-undo running).
- sch-drag experiment merged (crate `sch-drag`: gated `drag`/`drag_many` with rollback on any net change,
  lattice `route`, tiered `measure`, `tidy` search). Verdict: `tidy` does NOT beat the engines on the
  critic (BluePill 5→5; two good human sheets 9→8, 7→6 while its own score improved) → not in the loop.
  Kept for the primitive. Found+fixed in sch-doc: `gc_lib_symbols` dropped sheet-local `(lib_name)`
  symbol copies on EVERY write (pins lost), `body_rect` unioned multi-unit bodies, `pins::resolve`
  ignored `lib_name`. QUEUED: wire `sch_drag::drag` into `move_symbols` (replace move_attached +
  straighten) once lane/bluepill-erc merges; delete `tidy` if still unused after W3.
- bluepill-erc merged (locally; push after lane/dup-segment): one label scope per net enforced at the
  document, `connect`/`label`/`add_power` clear no-connect markers first, a label on a pin already on
  another authored net REFUSES naming both nets, `remove_symbols`/`delete_wires` retract orphaned runs;
  BluePill repro ERC 89 → 0. Its float-epsilon test exposed a real duplicate wire segment → lane/dup-segment.
- Launched: W3 lane/sch-bench (Opus: place_parts partial commit + bench + netlist-drawn arrange +
  reserve_refs enforcement + extractor label-scope parity), lane/drag-move (codex: sch_drag::drag
  inside move_symbols).
- Pushed together: bluepill-erc, drag-move (`move_symbols` on `sch_drag::drag_many`; straighten/glue
  helpers deleted), dup-segment (second emitter was `retract_colliding_stubs` drawing a fallback stub
  over a same-net route prefix; debug asserts: no duplicate unordered segment in writer or after graft),
  user hotfix "Snapshot only the design files at turn start". USER: the turn-start snapshot itself is
  being removed (lane/no-baseline) with diff_schematic and introduced/pre-existing classification —
  the TUI from the repo root read a 186 GB target/. Harness: judges get clean KiCAD renders
  (lane/judge-clean-render).
- Pushed: W3 (place_parts partial commit + bench + netlist-drawn arrange + reserve_refs enforced;
  extractor already matches KiCAD on same-name scopes), no-baseline (no per-turn snapshot at all;
  diff_schematic gone; check reports one list; repo cwd refused without --project), guard fix (a drag
  may mint a name for a declared derived net). Open on main: `arrange_boundary` 40-part block and
  `floorplan_netlist` challenge fixtures trip the writer's new duplicate-segment debug assert
  (lane/arrange-dup); reversed-LED polarity fix needs turn-in-place (lane/turn-in-place); judges get
  clean renders (lane/judge-clean-render).
- Pushed: turn-in-place (reversed-LED fix works: geometry-derived turn, wires held, declared net swap),
  arrange-dup (MST edge over an earlier span; uncovered fragments only), remove-remnants
  (`remove_symbols` takes dead runs/labels/markers/flags and accepts #PWR refs + UUIDs; new
  `delete_labels{names|uuids|bbox|net}` and `remove_region{bbox|block}` with boundary clipping;
  UUIDs in read_schematic/get_net). Next: merge judge-clean-render, then a full campaign under the
  quality-first rubric on the merged tree → accounting round 3.
- Found in the connector case (rem-runs): `swap_symbol` to a bigger connector left a DIAGONAL wire
  (130.81,109.22)→(152.4,106.68) — it re-seats pins without the drag primitive's orthogonal redraw.
  QUEUED after lane/swap-flex merges: swap_symbol (and any pin re-seat) goes through `sch_drag`, plus a
  document invariant that every wire segment is axis-aligned (debug assert + test).
- Pushed: judge-clean-render (judges see clean KiCAD exports; sch human-look honest 4/10 on
  edit-add-testpoints), swap-flex (swap_symbol/add_symbols: footprints repairable metadata;
  connector case 6→7, replace-ic 10). Running: lane/orthogonal-wires (every pin re-seat via the drag
  primitive + axis-aligned wire invariant), campaign rerun camp5 (`--max-turns 3`, quality-first
  rubric) = accounting round 3; slow floorplan gates on pushed main.
- Campaign round 3 (camp5) was a harness miss: continuation never fired (regex wanted "Stopped
  after…", the agent now hands off with "## Partial state") and the sch critic died (600 dpi
  ImageMagick abort) — both fixed and pushed; round 4 (camp6, --max-turns 3) running. Real harvest:
  audio reached the board (41 parts, ERC 0) but sync_board refused on missing footprints, place_board
  refused edge parts outside the auto outline, pours schema strict → 18 unrouted / 245 DRC / critic 1
  (parts left in the staging row, outline sized around it) → lane/pcb-refusals. esp32: "the bench
  draw did not preserve connectivity" + duplicate_refs whole-payload refusal → lane/bench-draw.
  QUEUED (after lane/orthogonal-wires): add_power generic bar symbol for unknown power nets, get_net
  resolving power nets, remove_symbols partial (missing refs reported), delete_wires{pins} declaring
  the touched derived nets itself, decouple-ambiguous → place + gap instead of refuse.
- Pushed: orthogonal-wires (all pin re-seats via drag redraw; `SchDoc::wire_faults` invariant),
  sch-refusals (generic `power:VDC` bar for unknown rails, get_net resolves power nets/near-misses,
  partial remove_symbols, delete_wires authorises its own cut, decouple-ambiguous → gap, label refusal
  carries a fix, remove_region lists blocks, footprint-as-symbol explained). Running: pcb-refusals,
  bench-draw (sent back: never rename a requested authored net — fix verifier attribution), assign-flex
  (assign_footprints repairable + search_footprints query-only), rail-dup, campaign round 4.
- Round 4 (camp6): continuation works; 3/4 cases reach a DRC-0 board; unrouted 5/20/20 remain; no fab
  yet. QUEUED PCB-2 (after lane/pcb-refusals merges): route_board partial for plane nets (keep what
  routed, report the rest), move_parts nudge-to-free like move_symbols + lenient move schema,
  sync_board with intent on an existing board = delta + place_board(intent), sync_board never refuses
  on ERC errors/missing footprints (stage + report), place_board reporting absent vs staged refs,
  route_board on a net absent from the board → sync hint. QUEUED SCH-2 (after bench/assign merge):
  derived `Net-(X-Y)` accepted as a pin address everywhere, `connect: every connection failed` must
  name each failure, remove_symbols declares the nets it touches, library-NC pin wired → place with nc
  + gap.
