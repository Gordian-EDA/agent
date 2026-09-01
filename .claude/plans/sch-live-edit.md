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
| `sch-place` | `LayoutIr` gains relational intent: `left_of/right_of/above/below`, `group(members, side_of anchor)`, `align`. |
| `sch-floorplan` + engines | Honour relational constraints; new `region::arrange(items, frozen neighbours, obstacles)` adapter; realiser emits `sch-doc` items instead of a whole file. |
| `gordian-core` tools | Pin-level mutators, constraint placement, queries, `place_parts` bulk create, `arrange`, `rewire`, `connect`. Per-call net delta. |

## Deleted at the end
`circuit-lang` parse/desugar/canon/yaml/surface; `sch-io` (writer becomes realiser inside sch-floorplan); the draft workspace; `apply_design`/`edit_design`/`create_design`/`read_schematic(yaml)`/`validate_design`; `multisheet.rs`; the YAML apply-gate diff.

## Waves

**Wave 1 (parallel, worktrees):**
- L1 `sch-doc`: model/parse/write + round-trip gate on the 157-file corpus (kicad-cli netlist + ERC counts identical; hand content survives) + connectivity extractor gated against kicad-cli partitions.
- L2 engine constraints + region adapter on `sch-place::Item` (independent of sch-doc).
- L3 validate the PCB interactive tools: get `local-board-move`, `replace-pcb-component`, `finish-existing-pcb` quality cases to graded runs; fix what breaks.
- L4 `sch-check`: extract model/lint/erc from circuit-lang; define `place_parts` input type (kernel model JSON, keep `decouple` sugar).

**Wave 2:** tools + queries + snapshot/undo wired into `tool_defs`; prompts; quality edit cases pass with untouched parts byte-stable.

**Wave 3:** `place_parts`/`arrange`/`rewire`/`connect` over L1+L2; ten-prompt campaign parity; truthfulness gate green.

**Wave 4:** delete the old path; full quality suite + `validate_pcb_corpus --required`.

## Gates
- Extractor must equal kicad-cli netlist partition on every corpus file; kicad-cli stays the oracle in tests and at save.
- Any placement change passes the truthfulness gate before it is called a win.

## Queued after wave 2 (user, 2026-09-01)
**Tracing migration** (Sonnet lane): add `tracing` + `tracing-subscriber` as workspace
deps; replace every non-TUI `println!`/`eprintln!` in library and binary code (~74 sites:
gordian 27, spine-place 15, sch-floorplan 9, sch-io 6, cluster-place 5, anneal-place 5,
pcb-workflow 3, gordian-core 3, kicad-ipc 1) with `tracing::{info,debug,warn,error}` and
spans; the CLI/TUI install a subscriber writing to `<project>/.gordian/logs/` (rolling,
one file per session, thread id in the filename) plus stderr at `info` for headless
runs; `RUST_LOG` honoured. Tests/examples may keep `println!`. Stdout must stay clean
for `tool_once`-style JSON binaries.

## Open engine bug (found by W2b's `live::verify`, 2026-09-01)
`stm32f4-buck` and `openmyo-emg` are untruthful on BOTH the old and live paths: 2-pin
parts come back with pins swapped (`R5.1`↔`R5.2`, `C11.1`↔`C11.2`, `C17`/`U2` on
`N_U2_BS`) — `between: [A, B]` pin order is not honoured somewhere in the engine/realiser.
Neither fixture is in `floorplan_netlist`'s lists, so nothing checked them. Add both to
the netlist gate and fix the ordering (engine-owned; after the truthfulness-fix lane merges).

## Open: spine channel-order regression (bisected 2026-09-01)
`spine-place/tests/frozen_idioms.rs::spine_preserves_inferred_pc817_channel_cells` fails
(`U1.at.y < U2.at.y` violated) from the L2 merge (`07bbb94`, relational intent) onward;
passes at `100b3bc`. Likely the relation projection / `apply_cells` frozen-seed change in
spine's pass ordering. Engine-owned — assign to the truthfulness-fix lane after its gate.

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
