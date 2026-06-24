# Unified PCB state: the `.kicad_pcb` is the single source of truth

## Problem

The PCB flow keeps board state in **two divergent stores**, and footprint
*assignment* lives in a third, throwaway place. The result is silent desync and a
footprint round-trip that is dead end-to-end.

### The double state

- **Layer 1 — headless engine.** `derive_board → place_board → route_board →
  export_board` runs in pure Rust (`pcb_place`, `negotiated_mesh`, `pcb_synth`) over
  a persisted draft `.gordian/board.json` (`BoardDraft`) plus `.gordian/route.json`.
  `export_board` synthesizes the `.kicad_pcb` *from scratch* out of the draft.
- **Layer 2 — live KiCAD IPC.** `open_board` launches `xvfb-run pcbnew <file>`;
  `move_part` / `route_track` / `set_net_width` mutate the *running* document over the
  socket.

The two stores are never reconciled. IPC edits do not flow back into `board.json`,
and re-running the engine overwrites from the draft, ignoring them. Worse, **nothing
persists the IPC edits**: `save_session_if_open` (`tools_pcb.rs:3077`) has zero
callers, and `Session::drop` (`session.rs:80`) just `child.kill()`s pcbnew — so live
edits evaporate.

### The dead footprint round-trip

Footprint *assignment* (which footprint a part uses) belongs, in KiCAD's model, to
the **schematic** (the symbol's `Footprint` field); the board only holds the
*instance*. KiCAD's "Update PCB from Schematic" (F8) is **non-destructive** — it
preserves placement/routing and reconciles only deltas — precisely so assignment can
re-sync without losing board work.

Our pipeline violates both properties, and the footprint never survives the trip:

```
circuit.yaml   footprint: Package_SO:SOIC-8     ✓ parses (parse.rs:359), serializes (canon.rs:144), model (model.rs:42)
   │ apply_design → emit.rs
   ▼
.kicad_sch     (property "Footprint" "")          ✗ emit.rs:2136 writes EMPTY, unconditionally
   │ lift → design_from_netlist
   ▼
Component { ..default() }  footprint: None         ✗ lift.rs:113 never copies comp.properties["Footprint"]
   │ derive_board
   ▼
board.json     missing_footprints: [everything]
   │ assign_footprint
   ▼
board.json     part.footprint = "…"                ✗ tools_pcb.rs:3027 writes board.json ONLY — lost on re-derive
```

The harnesses (`board_harness`, `pcb_gate`) never catch this because they build drafts
from standalone JSON via `build_board_draft`, bypassing the schematic entirely.

## Goal

One canonical home per fact. No draft, no reconciliation, no evaporating edits.

- **Board state** (placement, routing, outline, keepouts, pours, footprint
  instances) → the **`.kicad_pcb`** file (with design rules in its `.kicad_pro`
  sidecar, where KiCAD itself keeps them).
- **Footprint assignment** → the **schematic**, via the circuit YAML's existing
  `footprint:` field, persisted by `emit` and read back by `lift`.
- **Placement intent** (groups / edge / surround) → **ephemeral arguments** to
  `auto_layout`; it is an input to a transform, not stored state.

`board.json` and `route.json` are deleted. The engine becomes a **stateless
transform over the file**: read `.kicad_pcb` → compute → write `.kicad_pcb`.

## Design

### 1. Repair the footprint round-trip (assignment home = schematic)

Two one-line-ish fixes make the schematic the durable home — independently a real
bug fix (today *no* footprint a user sets in the DSL ever reaches their `.kicad_sch`):

- **`emit.rs:2136`** — write the real footprint instead of `""`:
  `(property "Footprint" "<c.footprint>")` (empty when `None`, preserving current
  behaviour for unassigned parts). Keep it hidden, per KiCAD convention.
- **`lift.rs` `design_from_netlist`** (~line 113) — populate
  `kernel.footprint` from `comp.properties.get("Footprint")` (treating empty/`~` as
  `None`, mirroring `kernel_value`). The netlist parser already captures
  `<footprint>` into that property (`cli.rs:334`); lift just has to read it.

After this, the circuit YAML `footprint:` ↔ `.kicad_sch` `Footprint` field round-trips.

### 2. `assign_footprint` writes through the schematic, not the board

`assign_footprint` becomes a thin helper over the **schematic-side draft**: edit the
named component's `footprint:` in `draft.circuit.yaml` and re-commit via the existing
`apply_design` path, so the assignment lands in `.kicad_sch` and survives every
re-sync. It no longer touches any board store.

**Decision (flag for review):** `assign_footprint` is **one-shot** — it patches the
YAML *and* re-applies (commits) in a single call, rather than leaving the agent to
call `apply_design` separately. Rationale: a forgotten `apply_design` would silently
drop the assignment, which is the exact failure mode we are eliminating. It still
validates pad coverage against the footprint index as it does today
(`tools_pcb.rs:3018`).

### 3. The engine operates on the `.kicad_pcb` file

| Tool (new → old) | Reads | Writes |
|---|---|---|
| `create_board` (was `derive_board`) | schematic netlist (`lift`) | synthesizes the `.kicad_pcb`: footprint instances + pad nets + ratsnest, **no placement**, at requested `bounds`/`rules`. **Non-destructive re-sync** if a board exists: preserve placement/routing/locks of unchanged parts; add/remove/renet only the delta. |
| `auto_layout` (was `place_board`) | `.kicad_pcb` (`read_problem`) + `hints` **arg** | `pcb_place::place_board(&problem, &hints)`; write each footprint's `at`/rotation back. **Respects `locked` footprints** so manual nudges survive. |
| `auto_route` (was `route_board`) | `.kicad_pcb` (`read_problem`) | `route_auto`; `write_solution` tracks/vias back (the `autoroute`/freerouting path at `tools_pcb.rs:3041` is the model). |
| `finalize_board` (was `export_board`) | `.kicad_pcb` | in-place post-route finish: tighten `Edge.Cuts` to copper + margin (`content_bounds`), emit plane/pour/keepout zones, run `kicad-cli pcb drc`. **No re-synth from a draft** — the file already exists. |

Placement hints (`PlacementHints`: `groups`, `edge_seek`, `corner_seek`, `surround`)
are passed as an `auto_layout` argument and consumed; the auto edge/corner affinity
for connectors/mounting-holes (`tools_pcb.rs:1001`) still applies. The connectivity
oracle (`drc_lint`) and the `(failed nets, geometry violations)` ranking are unchanged
— they already read board geometry.

### 4. Live KiCAD session edits persist to the same file

`open_board` + `move_part` / `route_track` / `set_net_width` edit the running pcbnew,
which holds the **same** `.kicad_pcb`. Each interactive edit (or session teardown)
calls `session.kicad().save()` — wiring up the currently-dead `save_session_if_open`
— so edits land in the canonical file. `Session::drop` saves before killing pcbnew.

### 5. Engine ↔ session coherence rule

The engine writes the file directly; a live pcbnew won't reload a file changed under
it. To avoid a stale in-memory document, the **engine tools require no open session**:
they save-and-close any open session before operating on the file (the agent re-opens
to inspect). The live session is exclusively for interactive manual edits. This keeps
exactly one writer of the file at a time and the engine pure/file-based.

### 6. Delete the draft stores

Remove `BoardDraft`, `DraftRules`, `DraftPart`, `Keepout`/`PourSpec` draft types,
`Workspace::{read,write}_board`, `Workspace::{read,write}_route`, and the
`board.json`/`route.json` files. `build_board_draft` and `apply_spec_extras` (used by
the deterministic harnesses) are re-pointed at `create_board`'s file-synth path so
`board_harness`/`pcb_gate` build a `.kicad_pcb` from their standalone circuit JSON —
keeping the no-LLM DRC gate intact, still without a live KiCAD.

## State → home mapping (the `board.json` autopsy)

| `BoardDraft` field | New home |
|---|---|
| `parts` (footprint + `pad_nets`) | footprint instances + pad nets in `.kicad_pcb` |
| `last_placement` | footprint `at`/rotation in `.kicad_pcb` |
| `locked` | footprint `locked` attribute in `.kicad_pcb` |
| `bounds` / `outline` | `Edge.Cuts` in `.kicad_pcb` |
| `keepouts` | keepout rule-area zones in `.kicad_pcb` |
| `rules` (clearance/width/via), `net_widths` | net classes / design rules in `.kicad_pro` |
| `pours` | copper zones in `.kicad_pcb` |
| `hints` | ephemeral `auto_layout` argument (not stored) |
| `last_place_illegal` | recomputed, not stored |

## Transcript (the target UX)

1. `assign_footprint` → patches circuit YAML, commits to `.kicad_sch`.
2. `auto_layout` → seeds initial placement into the `.kicad_pcb`.
3. `render_board`; `open_board` + `move_part` → nudge, saved to the file.
4. `auto_route` → routes the file.
5. `route_track` / rip-up refine → saved to the file.

## Testing

- **Round-trip unit test:** circuit YAML with `footprint:` → `apply_design` →
  `.kicad_sch` carries the `Footprint` field → `lift` → YAML `footprint:` restored.
  Guards the emit + lift fixes (the gap the harnesses miss today).
- **`assign_footprint` persistence:** assign → re-`create_board` → assignment present
  (proves it survives a re-sync; the current bug's regression test).
- **Non-destructive re-sync:** place a board, `create_board` again with an added part
  → existing placements preserved, new part added unplaced.
- **Engine-over-file:** `auto_layout`/`auto_route` read a `.kicad_pcb` and write a
  valid one (DRC-clean) with no `board.json` present.
- **Session save:** `open_board` → `move_part` → reopen → moved position persisted.
- **Gates unchanged:** `cargo test --release -p pcb-engine -p kicad-bridge -p agent`
  and `board_harness` (DRC stays clean), per CLAUDE.md. The netlist oracle
  (`floorplan_netlist`) gates the emit/lift change.

## Out of scope

- Bidirectional placement sync back into the schematic (board → sch). Placement stays
  board-only.
- Changing the placement/routing algorithms. This is a state/IO refactor; the engine's
  compute is untouched.
- Microvia/HDI, pours, plane changes beyond re-homing them onto the file.

## Risks

- **Non-destructive re-sync** is the hardest new behaviour (matching footprint
  instances to schematic parts across edits via the `ap_*` identity tags already in
  the file). If it proves heavy, a fallback first cut is *destructive* re-sync that
  still reads assignment from the (now-fixed) schematic — losing only placement on
  re-sync, never the footprint. Ship destructive first, harden to non-destructive.
- **Session/engine file contention** (§5) — mitigated by the single-writer rule.
