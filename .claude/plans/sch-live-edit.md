# Schematic live-edit rewrite — plan

Decision (2026-08-31): the `.kicad_sch` file is the only schematic source of truth.
The agent edits it in place through tools. The LLM owns connectivity + layout
*intent* (relative placement, groups, flow); solvers own all coordinates. Wires
are never drawn by coordinate.

## Target crates

| Crate | Role |
|---|---|
| `sch-doc` (new) | Lossless `.kicad_sch` document over `kiutils_kicad` CST: typed model for what tools touch, unmodeled nodes retained verbatim. Parse/edit/write, snapshot/undo, and the pure-Rust connectivity extractor (geometry → net partition). |
| `sch-check` (new, from `circuit-lang`) | `model.rs`/`lint.rs`/`erc.rs`/`diag.rs`/`provider.rs` rerooted to run over a `Design` built from the extractor. `circuit-graph` unchanged. |
| `sch-model` | `LayoutIr` gains relational intent: `left_of/right_of/above/below`, `group(members, side_of anchor)`, `align`. |
| `sch-floorplan` + engines | Honour relational constraints; new `region::arrange(items, frozen neighbours, obstacles)` adapter; realiser emits `sch-doc` items instead of a whole file. |
| `gordian-core` tools | Pin-level mutators, constraint placement, queries, `place_parts` bulk create, `arrange`, `rewire`, `connect`. Per-call net delta. |

## Deleted at the end
`circuit-lang` parse/desugar/canon/yaml/surface; `sch-io` (writer becomes realiser inside sch-floorplan); the draft workspace; `apply_design`/`edit_design`/`create_design`/`read_schematic(yaml)`/`validate_design`; `multisheet.rs`; the YAML apply-gate diff.

## Waves

**Wave 1 (parallel, worktrees):**
- L1 `sch-doc`: model/parse/write + round-trip gate on the 157-file corpus (kicad-cli netlist + ERC counts identical; hand content survives) + connectivity extractor gated against kicad-cli partitions.
- L2 engine constraints + region adapter on `sch-model::Item` (independent of sch-doc).
- L3 validate the PCB interactive tools: get `local-board-move`, `replace-pcb-component`, `finish-existing-pcb` quality cases to graded runs; fix what breaks.
- L4 `sch-check`: extract model/lint/erc from circuit-lang; define `place_parts` input type (kernel model JSON, keep `decouple` sugar).

**Wave 2:** tools + queries + snapshot/undo wired into `tool_defs`; prompts; quality edit cases pass with untouched parts byte-stable.

**Wave 3:** `place_parts`/`arrange`/`rewire`/`connect` over L1+L2; ten-prompt campaign parity; truthfulness gate green.

**Wave 4:** delete the old path; full quality suite + `validate_pcb_corpus --required`.

## Gates
- Extractor must equal kicad-cli netlist partition on every corpus file; kicad-cli stays the oracle in tests and at save.
- Any placement change passes the truthfulness gate before it is called a win.

## Done
- **Tracing migration** — library and binary code logs through `tracing`; the CLI installs
  the subscriber (`.gordian/logs/`, stderr at `info`, `RUST_LOG` honoured). Only test
  `SKIP:` lines and the CLI's own version/usage output still print.
- **Spine channel-order regression** — fixed on main (`c01da81`): `preseeded`, not
  `frozen`, is what skips idiom seeding.
- **The `.kicad_sch` as sole source of truth** — the YAML front end, the draft workspace
  and the apply gate are gone; `place_parts`/`arrange`/`rewire` are the bulk tools, and
  fixtures are `PlacePartsInput` JSON that reproduces the pre-conversion placements
  byte-for-byte (a part names its own `block`; a payload carries `layout` per region).
- **One board-readiness policy** — `check_schematic` is the gate: ERC errors, an
  unbuildable net (one pin, no power symbol), a proved electrical defect, and a
  symbol/footprint pad mismatch are errors; heuristics stay warnings. `regenerate_board`
  refuses nothing the schematic gate passed.

## Open engine bug (found by W2b's `live::verify`, 2026-09-01)
`stm32f4-buck` and `openmyo-emg` are untruthful on BOTH the old and live paths: 2-pin
parts come back with pins swapped (`R5.1`↔`R5.2`, `C11.1`↔`C11.2`, `C17`/`U2` on
`N_U2_BS`) — `between: [A, B]` pin order is not honoured somewhere in the engine/realiser.
Neither fixture is in `floorplan_netlist`'s lists, so nothing checked them. Add both to
the netlist gate and fix the ordering.

## Open: created designs under-deliver their own brief
`sch-create-medium` still lands ~13 parts against a rubric of 18, and `sch-create-large`
ships wiring the judge faults (VSS on the rail, a half-built feedback divider). Nothing
in the tool contract blocks the model — the completeness is a prompt/design-review lever,
not a gate. The deterministic checks that *can* prove a defect already block the turn.

## Queued (user, 2026-09-01): remove the approval gate entirely
Delete `ToolEffect::ApprovalRequired` (mutators run directly — per-call snapshot +
connectivity guard + `undo` make it safe), the `Approvals`/`AutoApprove` seam and
`AgentEvent::Applied` plumbing in `gordian-core/src/agent.rs`, `AutoApprove::yes()` in the
CLI, and the TUI's approve/reject cockpit (`crates/gordian/src/tui`). PCB mutators
(`move_parts`, `route_track`, `delete_copper`, `regenerate_board`, `export_fab`) run
directly too. Start after the wave-3 close-out lane merges (it edits agent.rs + TUI).
Also in that lane (user, 2026-09-01): **delete the no-progress watchdog** — `StopReason::NoProgress`,
`MAX_CONSECUTIVE_NO_PROGRESS_COMPLETIONS`, `consecutive_no_progress_completions`,
`no_progress_final_text`, and `is_inspection_tool` (only exists to feed it) in
`gordian-core/src/agent.rs`; the "stopped after N model completions made no durable
progress" text goes with it. Keep only the per-turn provider-request cap.

## Open (found by the pin-order lane, 2026-09-01): per-pin name-vs-number shadowing
`sch-floorplan/src/floorplan/place/emit.rs` `resolve_pin_target` does
`comp.pins.get(&pin.number).or_else(|| comp.pins.get(&pin.name))` PER physical pin, while
the canonical resolvers (`sch_check::pins::resolve`, `find_pin`, `pin_endpoints`) are
globally number-first. On a symbol whose pin is *named* `2` the emitter can attach one net
to two pins. No reproduction yet — needs a fixture with such a symbol + a netlist gate.

## Queued (user, 2026-09-01): one revision/undo system for schematic AND board
`gordian_runtime::revisions` owns `<project>/.gordian/revisions/<n>/` — every mutating
tool (schematic: all `gordian-tools-sch` mutators; board: `regenerate_board`, `place_board`,
`route_board`, `move_parts`, `route_track`, `delete_copper`, `set_net_width`,
`update_board_outline`, `assign_footprints`) calls `capture(tool, summary, &[paths])` BEFORE
writing, snapshotting exactly the files it will touch (`.kicad_sch`, `.kicad_pcb`,
`.kicad_pro`, `fp-lib-table`…) and gets a `RevisionId`. One `undo{revision?}` tool restores
every file of that revision atomically (default: the latest) and reloads any live board
session; `history{limit?}` lists revisions (id, tool, summary, files, when). Replaces
`.gordian/sch-undo` + `SnapshotId` plumbing in `gordian-tools-sch/src/session.rs`; the
tools' `snapshot` result field becomes `revision`. Bounded retention (keep last N, prune).
Start after `lane/pcb-diagnostics` merges (shared pcb-workflow write paths). `sync_board` already
writes `.gordian/pcb-undo/pcb-<n>.kicad_pcb` in the `sch-undo` shape and returns it as `revision`;
nothing reads it yet, so this system is what makes that token redeemable.

## PCB architecture (user approved 2026-09-01): mirror the schematic side
Order: (1) ✅ DONE (`lane/sync-board`) — `sync_board` replaced `regenerate_board`: netlist diff
schematic↔board, add/remove/retarget/reseed only the delta, auto-sized outline on an empty board,
`.gordian/pcb-undo/` snapshot + a differential connectivity guard. `place_board` now refuses an
already-placed board unless `replace: true`, which is what makes the kept placement stick;
(2) `place_board{refs?}`
/ `route_board{nets?}` as subset ops with the rest locked / fixed copper (whole = all selected);
(3) one guard: every board mutator snapshot → edit → pcb-drc on the touched region → refuse with
violations or write; invariant = copper connectivity ⊆ schematic netlist; (4) board intent
(`edge`, `keep_near`, `group`, layers/rules, zones) → `PlacementHints`, never coordinates;
(5) unified revisions + `check_board` (DRC + unconnected pairs + netlist consistency) as the
completion signal; (6) delete the `PcbEngine` monolith, the destructive regenerate path, and
`run_pcb_finish_pipeline` (hidden orchestrator) — the model orchestrates with tools.

## Queued (user, 2026-09-02): routing aesthetics
Auto-route works but is "kinda ugly": next router lane — fewer bends/meanders, 45° escapes,
straight pad exits, via minimisation, no wandering around obstacles when a channel exists;
judged by `tools/pcb_critic.py` before/after on the PCB corpus with DRC 0 kept.

## NORTH STAR (user, 2026-09-02)
Near-perfect sch+pcb for 30+ component designs in <5 min end to end; selected QC runs
perfect (checks, critics ≥9, judge ≥9, ERC/DRC/unrouted 0, no refusals/loops). Campaign
set to define: sch-create-large→PCB, create-hard-pcb, bms-10s (46 parts), esp32-multifunction
(92). Loop: run with named questions → findings → lanes → rerun.

**Self-diagnosis (user tip):** after every QC case, the runner asks the agent one more turn
in the same session — "What did you struggle with in the current toolset during this task?
List concrete tool gaps, confusing results, missing information, and what would have made it
faster." — and records the answer as `self_diagnosis` in result.json and the findings file.

## Queued (user, 2026-09-02): local place/route + algorithm quality
Tools: `place_board{refs?|bbox?}` and `route_board{nets?|bbox?}` are LOCAL by default —
only the selection moves / only copper inside the box (plus nets crossing it) is ripped and
re-routed; everything outside is frozen/fixed copper. Whole-board = "select all".
Algorithms (leaf crates, judged by `tools/pcb_critic.py` on the campaign set, DRC 0 kept):
placer — region placement with locked neighbours + edge/keep_near/group intent as
constraints, courtyard-true packing, connector-on-edge, decoupling adjacency; router —
aesthetics (45° escapes, minimal bends, no meanders, via minimisation, bus-like parallel
runs), region rip-up/re-route, honest partial results. Baseline critic before, target ≥9.
Starts after `lane/board-guard` merges.

## Queued (self-diagnosis, 2026-09-02): findings relative to the turn's baseline
`check_schematic` (and `check_board`) must classify each finding as `introduced` (absent at
the turn's first revision) or `pre-existing`, and the loop summary must say "N new, M
pre-existing"; the model then fixes what it broke and leaves inherited warnings alone
unless asked. Same for `render_schematic`'s `visual` facts (`_added` is already computed
by the harness — move it into the tool).

## Queued (ctri4, 2026-09-02): placement is a HARD deadline
A `place_parts` on an 88-part sheet ran 68 s past a 45 s budget (25% of the wall clock) and
still refused. The engine polls the deadline but realise/verify/gate do not, and a slow
engine step can overrun by tens of seconds. Make the budget a hard cancel: run the engine +
realiser on a worker thread, abandon it at the deadline (discard its result, return the
refusal immediately), and size the default block so ≥60-part sheets are placed as
regions (`place_parts` of a block onto the existing sheet) rather than whole-sheet
re-placements. After `lane/spine-panic` merges (shared `bulk.rs` ladder).

## Status 2026-09-02 (night)
Merged: rail-clearance (zero shorts), board-repair (in gate), ERC fixes carry calls, outline
re-fit + dangling-end DRC, hard placement deadline, spine guards, diff_schematic, offline
board I/O, multi-turn transcript. Campaign: bms ERC 0 in 15 requests; audio ERC 0 + board
created; stopping on the 270 s wall clock now, not the cap. Running: engine-opens (UART
field overlap; scattered-GND opens), footprint-compat (compatibility before placement).
Next: full campaign run on the integrated tree → accounting round 2 → lanes.

## Status 2026-09-02 (afternoon): three lanes + a contaminated campaign
- main @4c36d4e0: explicit `sync_board{bounds}` never re-fitted; engine-opens, footprint-compat, pcb-regress merged.
- USER DECISION: KiCAD 10 CLI-only, IPC deleted → `lane/kicad10-cli-only` (codex).
- `lane/engine-shorts` (Opus): `shorted 3V3+NRST` (26-part MCU block, spine) and `I2C_SCL+I2C_SDA` (59-part, spine+cluster).
- Campaign run camp3 is NOT a measurement: the k10 lane switched `~/.config/gordian/config.toml` to the KiCAD 10 libraries mid-run,
  so kicad-cli 9 on PATH could not load the schematics ("Failed to load schematic"; KiCAD 10 loads all four). Rerun after k10 merges.
- Version-independent harvest → `lane/refusal-ergo3` (codex): no_connect on a single-pin net retracts label+stub (32 refusals in one run),
  `delete_wires{net}` must remove labels-on-pin ("no wire matched" while connected), footprint existence + same-library did-you-mean at
  place_parts (C_1206_3216Metric_Polarized reached sync_board; microSD → DSUB-9 suggestion), refusal headline names the fatal category.
- Still true in every case: the 270 s wall clock is the binding bound; sch cleanup consumes it before the board phase.
