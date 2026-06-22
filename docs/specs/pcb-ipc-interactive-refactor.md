# PCB path → interactive, on KiCAD IPC (engine as assist), 9+ on hard boards

Status: in progress (Jun 2026). Branch `pcb-ipc-refactor`.

## Goal (user)

Completely refactor the PCB path to be **interactive over KiCAD IPC connections**, leave
**no legacy** behind. The LLM now drives geometry (for PCB, geometry *is* the engineering —
trace width = current, routing = SI/EMI, placement = thermal/decoupling); the harness still
provides **flexible, controllable assist tools** (auto-layout, auto-route, …) the LLM invokes
and overrides. Reach **9+ on e2e** for hard boards (custom copper layers, weird geometries,
dense BGA). Minimize schematic-engine changes (another agent owns it).

## Architecture

`LLM ⟷ agent tools ⟷ kicad-ipc ⟷ live KiCAD board` (+ `pcb-engine` as autoplace/autoroute
assist that computes geometry the IPC layer applies; + `kicad-cli pcb drc` as the DRC oracle).

- **KEEP + improve:** `pcb-engine` (placer/router), `kicad-bridge` (synth/footprint IO), the
  footprint index.
- **REMOVE (no legacy):** `board-lang` DSL, `design_board`/`derive_board`/`import_board`, the
  `BoardDraft` pipeline, the "LLM never emits geometry" contract (wrong for PCB).
- **ADD:** `kicad-ipc` (done) + the interactive tool layer + a KiCAD session manager.

## Foundation — DONE & validated (`crates/kicad-ipc`)

Native Rust client (no Python): prost + `nng` REQ0 over `ipc:///tmp/kicad/api.sock`, empty-token
bootstrap, Any-wrapped commands (`type.googleapis.com` domain — see [[kicad-ipc-protocol]]).
Validated vs live KiCAD 9.0.2 (headless under Xvfb): `connect/version/open_board/footprints/
tracks/nets/get_items/create_items/update_items/set_net_class/commit/save`. Proven edits:
move footprint, route 1.0mm power track, set a Power net class (the "wide copper" lever).
KiCAD 9 is sufficient; headless via Xvfb (9) or `kicad-cli api-server` (11+).

## Engine diagnosis (faithful boards only — many test circuits are DEGENERATE)

Test-fidelity trap: the `*-scale` BGA circuits have ~all single-pin nets + global VCC/GND rails
(bga256-201parts = 202/204 single-pin) — they test placement scale, NOT routing, and the
placer can't co-place decoupling caps because every cap shares the *global* rails (no per-IC
association). Faithful routing boards (≥2-pin signal nets): `soc-system`, `bga-escape-fineclear`,
`tqfp64-stress`, `dual-bga-bus`.

Route success on faithful boards (`route_auto` = best of detailed-mesh ∨ naive-grid):
| board | routed | verdict |
|---|---|---|
| tqfp64-stress | 61/64 (95%) | good (3 stragglers) |
| dual-bga-bus | 44/48 (92%) | good (4 stragglers) |
| bga64-stress (2-layer) | 42/46 (91%) | good (4 stragglers) |
| bga64-8layer | 39/46 (85%) | 8 layers did NOT help (7 fail vs 4) |
| bga-escape-fineclear | 21/70 (30%) | FAILS (tight clearance) |
| soc-system | 11/79 (14%) | FAILS (2 BGAs, congestion) |

**Refined root cause: the router under-uses copper layers + can't relieve congestion.** Note
bga64 routes the same (~85-91%) at 2 AND 8 layers — the (selected) naive grid router stays
effectively ~2-layer and does NOT exploit inner layers for escape or congestion relief, so extra
layers are wasted. Two failure modes: (a) **a few stragglers everywhere** (3-4 nets that lose a
congestion race — need rip-up/reorder retry); (b) **hard outliers** (fineclear = escape channels
too tight for vias; soc-system = two-BGA congestion) — these need genuine **layer-aware escape**
(drop inner pads to inner layers via dog-bone vias; `crossing.rs::place_via` assigns via sites
per mesh-leaf, not per-pad, so per-pad fan-out isn't produced). Lever = make routing layer-aware
(use inner layers) + per-pad BGA escape; secondary = stronger rip-up/retry for stragglers.

Placement also sprawls / scatters caps on large boards (partly degenerate-circuit-driven).

## Plan (priority order)

1. **BGA escape fan-out pre-pass** (the targeted router fix). Before the mesh router: for each
   high-pin-count grid package, place a dog-bone via in the channel adjacent to each inner pad,
   connect pad→via on the top layer, and rewrite that terminal to the via on an inner signal
   layer. Then the existing router routes inner-layer→destination. Gate on `board_harness`
   (bga-escape-fineclear + soc-system route %, DRC stays clean).
2. **Routing escalation lever — Freerouting** (if (1) is insufficient for the hardest boards).
   Java 21 is present; need a `.dsn` exporter + `.ses` importer (NOT in `kicad-cli`; write in
   `kicad-bridge`, or drive pcbnew's Specctra actions via IPC `RunAction`). Freerouting is the
   field's proven escape/dense router. Expose as the `autoroute` assist's escalation tier.
3. **Faithful hard test circuits.** Author/generate dense boards with REAL connectivity
   (per-IC decoupling power nets, real inter-chip buses, BGA escape) so place+route are tested
   honestly. The current `*-scale` circuits stay only as placement-scale tests.
4. **Placement quality.** With faithful per-IC nets: decoupling cap↔IC + series co-placement,
   and compaction (kill sprawl). Placement tuning is fragile (global springs rebalance every
   board) — prefer a per-board arbiter / locality-anneal. See [[pcb-placement-tuning-is-fragile]].
5. **Interactive tool layer (Phase 2) + doctrine (Phase 4).** KiCAD session manager (launch
   headless/local, connect, seed-from-schematic). Tools over `kicad-ipc`: `board_state`,
   `move_part`, `route_track`(width), `add_via`, `set_net_class`, `add_zone`, `set_outline`,
   `drc`, + `autoplace`/`autoroute` (scopeable: region/net/layer/width-policy). Remove the DSL
   tools. Rewrite the agent PCB prompt to the interactive doctrine. e2e harness over the IPC
   flow on hard boards → iterate render→critic to consistently 9+.

## Gates

`cargo test --release -p pcb-engine -p kicad-bridge -p agent` + `board_harness` (DRC clean,
route % up, no regressions). A prettier board that regresses DRC/connectivity is a regression.
