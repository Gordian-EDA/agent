# auto-pcb — KiCAD Copilot Agent: Design

**Status:** Brainstorm in progress (foundation + Pillar 1 locked; YML markup under active refinement)
**Date:** 2026-06-09
**Target:** KiCAD 10, Rust, TUI

---

## 1. Vision

A Rust TUI agent that works *alongside* a user in a running KiCAD instance ("copilot mode," not headless), across three pillars:

1. **Schematic synthesis** — one-shot from natural language ("Design me a bluepill-style STM32H7 dev board with USB and I²C pins").
2. **Part sourcing** — browse web, find real parts, import footprints/symbols, link them ("Find the best parts and link them").
3. **Physics-aware auto-layout + auto-routing of the PCB** ("Lay this out: PCB pins on the right, rectangular rounded board").

This document covers the **foundation + Pillar 1 (schematic synthesis)**, which is the MVP. Pillars 2 and 3 are sketched only enough to confirm the architecture supports them; each gets its own spec later.

---

## 2. The defining constraint: KiCAD 10's API is PCB-only

Researched and confirmed across KiCAD dev docs, the devlist, and every existing KiCAD MCP server:

- **The IPC API (protobuf/NNG) is implemented only in the PCB editor** in KiCAD 9 and 10. The schematic editor, symbol/footprint library editors, and headless mode are **not** covered. Schematic IPC is "future" (the team considers eeschema internals too unstable to freeze an API against).
- **eeschema has no scripting surface at all** — not Python, not C++, not action plugins. The legacy SWIG bindings are PCB-only, deprecated in KiCAD 9, and slated for removal in KiCAD 11.
- **Every existing KiCAD MCP server manipulates `.kicad_sch` files directly** for schematics (Seeed-Studio, lamaalrajih/kicad-mcp, circuit-synth/kicad-sch-api, Kletternaut/kicad-mcp-pro, mixelpixx). File-based S-expression manipulation is not a fallback — it is the only method that exists.

**Consequence — the agent is hybrid, and "copilot" means two different things:**

| Pillar | Mechanism | "Copilot" semantics |
|---|---|---|
| **PCB (layout/routing)** | Live IPC into running `pcbnew` | True real-time mutation, begin/end commit transactions |
| **Schematic** | File-based `.kicad_sch` read/write + reload handshake | Agent edits file → KiCAD detects external change → user reloads; agent file-watches user edits |

A `Backend` trait abstracts the two so the agent's tool layer is uniform. When KiCAD ships schematic IPC, an `IpcBackend` replaces `FileBackend` with no change to the agent. File-touching becomes a versioned implementation detail, not the architecture.

---

## 3. The Rust stack (one coherent ecosystem)

| Layer | Crate | Role |
|---|---|---|
| PCB — live | [`kicad-ipc-rs`](https://github.com/Milind220/kicad-ipc-rs) | 100% of KiCAD 10.0.1's 59 IPC commands; read/create/edit/commit/delete board items |
| Schematic + all files | [`kiutils-rs` / `kiutils_kicad`](https://github.com/Milind220/kiutils-rs) (v0.3.0) | Typed, **lossless** `.kicad_sch`/`.kicad_pcb`/`.kicad_sym`/`.kicad_mod` read/write |
| TUI | `ratatui` | Chat pane + circuit-state pane + diff pane |
| LLM | provider-agnostic `LlmBackend` trait | BYOK; Bedrock / OpenAI / Anthropic interchangeable; structured output |

Both KiCAD crates are by the same author (Milind220) — coherent and actively developed. `kiutils_kicad` exposes typed schematic items (`SchematicSymbol`, `SchematicWire`, `SchematicLabel`, `SchematicJunction`, `SchematicSheet`, `SchematicBus`, `SchematicNet`, `SchematicNoConnect`, …) with `WriteMode::Lossless` (byte-preserving) and `WriteMode::Canonical`. Risk: alpha; mutation helpers may be thin → we may work at CST level or contribute upstream.

---

## 4. Core idea: a schematic is HTML + CSS + a browser

Every LLM geometry domain independently converged on the same pattern: **the LLM emits relationships and intent; a deterministic engine owns coordinates.** (Mermaid = describe edges only; AutoPresent slides = Markdown/API not coords; Chat2SVG = structure not raw coords, which cause "coordinate hallucination"; EDA = hierarchical force-directed + constraints.)

Mapped to schematics, and to the user's own three prompts:

- **HTML = connectivity + structure** — components, nets, functional blocks. Pure EE reasoning; LLMs are good at it. → *"STM32H7 with USB and I²C."*
- **CSS = layout intent** — declarative, *relative, semantic* directives, never coordinates. → *"PCB pin at right, rectangular, rounded."* The layout prompts are literally CSS for circuits.
- **The browser = a deterministic Rust layout engine** → emits real `.kicad_sch` coordinates. The LLM never sees a number.

The *same* "intent → layout engine" paradigm powers both Pillar 1 (schematic) and Pillar 3 (PCB).

### Decisions made about the markup
- **Invent a minimal, LLM-friendly markup**, inspired by atopile (`.ato`) and tscircuit, but our own.
- **Surface = YAML with a fixed schema** (not a bespoke grammar): LLMs emit YAML near-perfectly, it's token-efficient, and schema-constrained generation makes malformed output near-impossible and auto-repairable. We invent the *vocabulary/semantics*, not an exotic syntax.
- **Layout intent is co-located on the entity, never via selectors** (the real Tailwind lesson: locality of behavior, no naming/indirection tax that makes LLMs drift). One file, not two.
- **Sparse override + strong engine defaults** — the engine auto-flows by default; the LLM adds intent only to override. Keeps connectivity clean and reduces what the LLM must get right.

---

## 5. The IR — `circuit.yaml` (current draft, the subject of refinement)

```yaml
name: bluepill-h7
rails: [3V3, GND]            # global power nets

parts:
  U1: {is: STM32H743VIT6, in: mcu}
  C1: {is: C, val: 100nF, in: mcu, near: U1}   # `near` = element-level intent
  C3: {is: C, val: 10uF,  in: mcu, near: U1}
  Y1: {is: crystal, val: 25MHz, in: mcu, near: U1}
  J1: {is: USB_C_Receptacle, in: usb}
  R1: {is: R, val: 5.1k, in: usb}
  R3: {is: R, val: 4.7k, in: i2c}
  J2: {is: Conn_2x02, in: i2c}

blocks:
  mcu: {at: center}          # group-level intent, on the block
  usb: {at: left}
  i2c: {at: right}

nets:
  3V3:    [U1.VDD, C1.1, C3.1, R3.1]
  GND:    [U1.VSS, C1.2, C3.2, J1.SHIELD]
  USB_DP: [U1.PA12, J1.DP]
  USB_DM: [U1.PA11, J1.DM]
  SCL:    {pins: [U1.PB6, R3.2, J2.1], expose: right}   # net-level intent inline
  SDA:    {pins: [U1.PB7, R4.2, J2.2], expose: right}
```

Short keys (`is`, `val`, `in`, `at`, `near`) save tokens. A net is **either** a bare pin-list (shorthand) **or** `{pins:[…], expose:…}` — the only union in the schema; everything else is a flat map.

### Closed intent vocabulary (entire layout surface — no `x`/`y` ever)

| Keyword | Scope | Meaning to engine |
|---|---|---|
| `at: center\|left\|right\|top\|bottom\|top-left\|…` | block / part | anchor to a sheet region |
| `near: <ref>` | part | proximity spring toward another part |
| `expose: <edge>` | net | labeled port/header on that sheet edge |
| *(absent)* | — | auto-flow default |

---

## 6. The layout engine ("the browser")

**Placement = two stages, both deterministic (fixed seed):**

1. **Floorplan.** Sheet = coarse 3×3 region grid. Each block is sized by the bounding boxes of its resolved symbols and dropped into the region named by `at:`. Un-anchored blocks auto-flow into free cells in reading order. Pure arithmetic.
2. **Intra-block solve (force-directed).** Within a block's region, parts settle under: **net springs** (same-net pins attract), **collision repulsion** (bounding boxes can't overlap), **`near:` spring** (strong attractor to named ref). Iterate to equilibrium, snap to KiCAD 2.54 mm grid. Pin-aware: because the resolver knows symbol pin positions, `near` biases toward the shared pin's side.

**Routing ≈ none, by design (net labels).** Connections have no paths: each pin → short stub + a net label; identical label name = electrically connected in KiCAD. No 2D path-finding, no crossings. `near:` affects only proximity (cosmetic), never the electrical connection (by-name). Phase 2 may replace `near` label-pairs with short drawn wires for a hand-drawn look; MVP is all-labels = always valid.

**Stable across turns.** On recompile, the engine reads back existing symbol coordinates (kiutils lossless) and pins them as the initial state, re-placing only new/changed parts → clean diffs, sticky positions.

---

## 7. Subsystems & responsibilities

- **Agent core** — owns the LLM conversation; NL → YAML edits via structured output; repair loop. Provider-agnostic `LlmBackend`.
- **IR + Validator** — YAML schema, parse/serialize, and *electrical* checks beyond schema (every referenced pin exists on the resolved symbol, no floating power, no obvious shorts). Errors returned to the LLM as plain text.
- **Symbol Resolver** — maps `is: <type>` → a real KiCAD library symbol (reads `.kicad_sym` via kiutils) and validates pin references (`U1.PA12` must be a real pin). Big deterministic guardrail. Seam where **Pillar 2** later swaps library symbol → sourced MPN.
- **Layout Engine** — §6.
- **KiCAD Bridge** — file-watch + lossless write; project detection; reload handshake. PCB-side IPC lives here (dormant until Pillar 3).
- **TUI** (ratatui) — chat + live circuit-state pane (blocks/parts/nets/ERC) + diff pane.

---

## 8. Round-trip stance (MVP contract)

`circuit.yaml` is **source of truth**; `.kicad_sch` is the **compiled artifact** (source → binary).

- **Connectivity is agent-owned.** To change wiring, the user asks the agent (edits YAML). Hand-rewiring in eeschema is **not** round-tripped in the MVP (reverse `.kicad_sch`→YAML inference is Phase 2+, genuinely hard). *Accepted by user.*
- **Positions are sticky.** Layout engine reads back existing coordinates, so user-dragged symbols stay put. The agent owns *what connects to what*; the user owns *where things sit*.

---

## 9. MVP cut line

**In:** TUI + agent loop + YAML IR + validator/repair + symbol resolver (KiCAD stock libs) + label-based layout engine + lossless `.kicad_sch` write + reload handshake. One-shot *and* iterative ("add a second I²C header").

**Out (later):** real part sourcing & footprint import (Pillar 2), PCB layout/routing via IPC (Pillar 3), drawn-wire prettifier, reverse round-trip of hand-edits.

---

## 10. Open work item — refine the YAML markup (for /ultraplan)

Refine the §5 markup language against these goals:

1. **Easy for LLMs** — familiar YAML surface, token-efficient, schema-constrained, sparse-with-defaults, minimal indirection.
2. **Flexible & expressive** — must express real circuits: buses, hierarchical/multi-sheet designs, differential pairs, multi-unit symbols (e.g., op-amp halves), power trees, no-connects, part attributes (footprint hint, tolerance, MPN slot for Pillar 2).
3. **Enables both schematic correctness and aesthetic intent** — the connectivity layer and the co-located layout-intent layer.
4. **Easy compilation to `.kicad_sch`** — maps cleanly onto kiutils typed items and the §6 engine.
5. **Bonus: round-trip from/to `.kicad_sch`** — what subset of the markup could be losslessly recovered from a compiled schematic (revisiting the §8 boundary)?

Deliverable: a refined, versioned schema (JSON-Schema-able), worked examples beyond the STM32H7 case, and a mapping table from each markup construct → kiutils item(s).
