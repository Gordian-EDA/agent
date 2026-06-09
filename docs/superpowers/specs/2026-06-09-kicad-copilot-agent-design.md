# auto-pcb — KiCAD Copilot Agent: Design

**Status:** Approved design (foundation + Pillar 1)
**Date:** 2026-06-09
**Target:** KiCAD 10, Rust, TUI

---

## 1. Vision

A Rust TUI agent that works *alongside* a user in a running KiCAD instance ("copilot mode," not headless), across three pillars:

1. **Schematic synthesis** — one-shot from natural language ("Design me a bluepill-style STM32H7 dev board with USB and I²C pins").
2. **Part sourcing** — browse web, find real parts, import footprints/symbols, link them.
3. **Physics-aware auto-layout + auto-routing of the PCB** ("Lay this out: PCB pins on the right, rectangular rounded board").

This document specifies the **foundation + Pillar 1 (schematic synthesis)** — the MVP. Pillars 2 and 3 are sketched (§16) only enough to confirm the architecture supports them; each gets its own spec later.

### MVP acceptance criteria

1. From the prompt *"Design me a bluepill-style STM32H7 dev board with USB and I²C pins"*, the agent produces a `.kicad_sch` that opens in KiCAD 10 and passes ERC clean.
2. A follow-up prompt ("add an SPI flash") modifies the design **without disturbing** symbols the user has manually repositioned.
3. The user can edit the schematic in eeschema between agent turns; the agent's next action reflects those edits.

---

## 2. The defining constraint: KiCAD 10's API is PCB-only

Researched and confirmed across KiCAD dev docs, the devlist, and every existing KiCAD MCP server:

- **The IPC API (protobuf/NNG over Unix socket) is implemented only in the PCB editor** in KiCAD 9 and 10. The schematic editor, library editors, and headless mode are **not** covered. Schematic IPC is "future" (the team considers eeschema internals too unstable to freeze an API against).
- **eeschema has no scripting surface at all** — not Python, not C++, not action plugins. The legacy SWIG bindings are PCB-only, deprecated in KiCAD 9, and slated for removal in KiCAD 11.
- **Every existing KiCAD MCP server manipulates `.kicad_sch` files directly** for schematics (Seeed-Studio, lamaalrajih/kicad-mcp, circuit-synth/kicad-sch-api, Kletternaut/kicad-mcp-pro, mixelpixx). File-based S-expression manipulation is not a fallback — it is the only method that exists.

**Consequence — the agent is hybrid, and "copilot" means two different things:**

| Pillar | Mechanism | "Copilot" semantics |
|---|---|---|
| **PCB (layout/routing)** | Live IPC into running `pcbnew` | True real-time mutation, begin/end commit transactions |
| **Schematic** | File-based `.kicad_sch` read/write + reload handshake | Agent edits file → KiCAD detects external change → user reloads; agent file-watches user edits |

A `Backend` trait abstracts the two so the agent's tool layer is uniform. When KiCAD ships schematic IPC, an `IpcBackend` replaces `FileBackend` with no change to the agent. File-touching is a versioned implementation detail, not the architecture.

---

## 3. The Rust stack

| Layer | Crate | Role |
|---|---|---|
| PCB — live | [`kicad-ipc-rs`](https://github.com/Milind220/kicad-ipc-rs) | 100% of KiCAD 10.0.1's 59 IPC commands |
| Schematic + all KiCAD files | [`kiutils-rs` / `kiutils_kicad`](https://github.com/Milind220/kiutils-rs) (v0.3.x) | Typed, **lossless** `.kicad_sch`/`.kicad_pcb`/`.kicad_sym`/`.kicad_mod` read/write (`WriteMode::Lossless` byte-preserving; typed AST over an authoritative CST → minimal diffs) |
| LLM providers | [`rig-core`](https://crates.io/crates/rig-core) + `rig-bedrock` | Multi-provider (OpenAI-compatible incl. Ollama/OpenRouter via base-URL, Anthropic, Bedrock), tool-calling, streaming. **Provider layer delegated; agent loop is ours** (human gate in the cycle). One thin adapter module isolates rig types. Exact crate re-verified at plan time. |
| TUI | `ratatui` + `crossterm` + `tokio` | Cockpit UI (§11) |

Both KiCAD crates share an author (Milind220) — a coherent, actively developed ecosystem. **Risk:** `kiutils_kicad` is alpha; schematic mutation helpers may be thin → work at CST level where needed, contribute upstream.

---

## 4. Core architecture: the React model

Every LLM-geometry domain independently converged on one pattern: **the LLM emits relationships and intent; a deterministic engine owns coordinates** (Mermaid: edges only; slide generators: markup not coords; SVG research: raw coords cause "coordinate hallucination"; EDA: netlist + constraint solvers). We apply it with React's vocabulary:

```
LLM ⇄ circuit YAML ──validate──reconcile──place──▶ .kicad_sch ⇄ user edits in KiCAD
      (virtual DOM,                                (DOM, SOURCE
       desired state)                               OF TRUTH)
            ▲                                            │
            └──── lift: kicad-cli netlist + ap_* props ──┘
```

- **`.kicad_sch` is authoritative** — it is what the user sees and edits. The YAML is an ephemeral projection (optionally saved as a generated artifact for git; never authoritative). This is the opposite of atopile's code-first model, deliberately: copilot mode means user edits must survive.
- **The LLM always emits full desired state** (LLMs are unreliable at diff formats). The **engine** computes the diff (reconciliation, §7).
- **Coordinates never appear in the YAML** — not on write, not on lift. The LLM is constitutionally incapable of touching geometry.
- **Lift** (sch → YAML): connectivity via `kicad-cli sch export netlist` (ships with KiCAD, authoritative — no hand-rolled wire-walker bugs); semantics via KiCAD custom properties (`ap_block`, `ap_role`, `ap_parent` on symbols — survive user edits, git, reload; the sch file is self-describing, no sidecar).

---

## 5. The Circuit Markup Language

A YAML dialect. File extension: `*.circuit.yaml`.

**Layering discipline (normative):** the language is a small universal **kernel**, plus **layout hints** (placer directives, no electrical meaning), plus a thin **sugar** layer. Every schematic is fully expressible in the kernel alone. Every sugar form desugars mechanically to kernel at parse time. **The validator, reconciler, and lift operate exclusively on the desugared kernel model** — sugar↔kernel differences can never cause spurious diffs.

### 5.1 Design principles (traceable to goals)

| Goal | Decisions |
|---|---|
| Easy for LLMs | Familiar YAML 1.2; domain-conventional net names as join keys; locality (connectivity declared *at* the component); no coordinates; full-state rewrite (no diff syntax); errors designed for self-repair |
| Clean | Sparse hints with strong engine defaults; sugar only where boilerplate is brutal; one file (no separate "CSS" — layout intent co-located, Tailwind-style, attached to entities, never via selectors) |
| Universal | Kernel expresses any (flat) schematic: arbitrary parts, fields, multi-unit, DNP, pinless parts |
| Round-trip | Kernel is also the canonical lift format; deterministic canonical ordering (same state ⇒ byte-identical YAML) |

### 5.2 THE KERNEL (the entire language)

```yaml
version: 1                          # required — schema version, strict
name: bluepill-h7                   # optional — title block
description: STM32H7 dev board      # optional — title block comment

blocks:                             # required, ≥1 — the one structural form
  <block_name>:
    note: <string>                  # optional — rendered as text on the sheet
    layout: {...}                   # optional — placer hints (§5.4)
    components:
      <REFDES>:                     # U1, R3, J2 — identity, reconciliation key
        part: <Lib:Symbol>          # required — KiCAD lib_id
        value: <string>             # optional — → Value field
        footprint: <Lib:FP>         # optional — → Footprint field (Pillar 2 fills)
        dnp: true                   # optional — do-not-populate flag
        props: {<Field>: <string>}  # optional — arbitrary KiCAD fields (MPN, …)
        pins:                       # optional — pinless parts (mounting holes) allowed
          <pin-name | pin-number>: <NET_NAME | nc>
        units:                      # multi-unit parts (opamps, gate arrays) only
          <A|B|...>: {pins: {...}}  # component-level `pins:` resolves across units
                                    #   (for shared/stacked power pins)

nets:                               # optional — ATTRIBUTES ONLY, never membership
  <NET_NAME>:
    power: true                     # global net, rendered as power symbols at pins
    class: <netclass>               # → KiCAD net class (feeds Pillar 3 rules)

lint:                               # optional — lint suppression (§5.3.9)
  allow: [<code>, ...]              # silence named lints (e.g. single-pin-net, near-name)
```

~15 keys. `value`/`footprint`/`dnp` are first-class because they map to KiCAD's canonical fields/flags; everything else goes through `props`.

### 5.3 Kernel semantics (normative rules)

1. **Membership lives exclusively in component pin-maps.** There is no net-centric pin list. One way to express connectivity.
2. **Nets exist by reference** — `PA12: USB_DP` creates the net. The `nets:` section only decorates. Declared-but-unreferenced → warning.
3. **Pin key resolution:** exact pin-*number* match first, then pin-*name*; a name matching several physical pins (stacked `VDD`) connects **all** of them. One line powers a 100-pin MCU.
4. **A pin maps to exactly one net.** Two mappings for one pin → hard error.
5. **NC policy:** explicit reserved word `nc` (case-insensitive) places a no-connect marker. Auto-no-connect is **materialized in the circuit-lang kernel**: a final desugar pass enumerates each known symbol's physical pins and inserts `nc` (keyed by pin number) for every unmentioned pin, **except** unconnected *power-input* pins, which are a hard compile error (forgetting the H7's VCAP pins fails loudly rather than being silenced). Consequence: because the kernel model carries these markers explicitly, canonical lift YAML **lists the auto-`nc` pins** (they round-trip as ordinary `nc` entries; the pass is idempotent).
6. **Strict schema.** Unknown keys are errors with did-you-mean suggestions (`decuople:` → `decouple?`). Strictness is LLM-friendliness in a self-repair loop. Growth goes through `version:`.
7. **YAML 1.2 core schema parsing is mandated.** Relays have pins named `NO`; values like `4.7k`/`NO` must never lex as booleans (the Norway problem). Pin keys normalize to strings. **A single YAML document only** — multiple documents (`---`-separated) are a hard error (`multiple-documents`). **Duplicate map keys** within one mapping (a repeated refdes or field) are a hard error (`duplicate-key`), never silently last-wins.
8. **Naming:** nets `UPPER_SNAKE`, no spaces, `/` reserved (future hierarchy); refdes strictly `[A-Z]+[0-9]+` (uppercase-letter prefix then digits, nothing interleaved); blocks `lower_snake`. **Enforcement:** net-name casing (a lowercase letter in a net name) is a **warning** (`net-name-case`) — it never blocks compilation; refdes shape and block-name violations remain hard errors.
9. **Anti-typo lints** (declaration-free nets are a typo hazard): *single-pin-net warning* (almost always a mistake) and *near-name warning* ("`I2C_SDA` and `I2C1_SDA` differ by one char — intentional?"; near-name compares the union of referenced and declared-only nets). **Suppressible** via the top-level `lint.allow` list (§5.2): any code listed there — e.g. `single-pin-net`, `near-name`, `unreferenced-net` — is silenced. The allow-set round-trips through canonical lift (sorted, emitted only when non-empty).
10. **Blocks are a partition** — every component in exactly one block; blocks are grouping + placement only (no electrical meaning, no namespacing; refdes are globally unique). Trivial designs use a single `main` block.

### 5.4 Layout hints (a namespace, not sugar)

Hints direct the placer; they never desugar and never affect electrical semantics. v1 vocabulary — deliberately tiny, **block-level only**:

```yaml
layout: {edge: left|right|top|bottom}   # pin a block to a sheet edge
layout: {near: <block>}                 # adjacency override
```

Component-level hints are excluded: intra-block placement is connectivity-driven (the crystal sits by the MCU because force-direction pulls it there). The same vocabulary reappears for board-edge intent in Pillar 3.

### 5.5 THE SUGAR LAYER (5 forms, each with an exact desugaring)

| Sugar | Desugars to |
|---|---|
| `rails: [3V3, GND]` (top-level) | `nets: {3V3: {power: true}, GND: {power: true}}` |
| Part aliases — exactly `R C L D LED` | `part: R` → `part: Device:R`, etc. Closed 5-entry table. |
| `between: [a, b]` (2-pin parts) | `pins: {1: a, 2: b}` in symbol pin-number order. Lint-warn on known-polarized parts (`D`, `LED`, `CP`) suggesting named pins `{A: …, K: …}`. |
| Pin-ref as net designator — `between: [J1.CC1, GND]`, `3: U1.PB6` | Resolves to the referenced pin's net; if none, synthesizes deterministic `N_J1_CC1`. The LLM never invents throwaway net names. |
| `decouple: {100nF: 10, 4.7uF: 2}` on a component | N caps between the parent's VDD\*/VCC\*-pin net and VSS\*/GND\*-pin net, read from the parent's own pin-map. Either side ambiguous (≠1 net) → error: "write caps explicitly". Pure-YAML map form — no mini-grammar strings. |

**Synthesized-component identity** (`decouple` is the only v1 sugar that creates components): synthesized caps are written with `ap_role: decouple`, `ap_parent: U1`, `ap_index: n` properties. The reconciler matches them by `(parent, role, index)` — not refdes — and **lift re-sugars them** back into one `decouple:` line. The LLM never sees them individually. This tag mechanism is the general carrier for any future synthesizing sugar.

Lift output = canonical kernel + re-sugared role-tagged synthetics. Canonical ordering: blocks in placement order, components by refdes, pins in symbol pin order, nets alphabetical, `lint.allow` sorted.

### 5.6 Deliberately excluded (and why)

- **`pullup:`/`pulldown:` net attrs** — redundant: `R7: {part: R, value: 4.7k, between: [I2C1_SCL, 3V3]}` is already one line. Sugar that saves zero lines is pure surface area.
- **Net-centric membership lists** — second way to say the same thing (§5.3.1).
- **`diffpair:`** — Pillar 3 concern; KiCAD pairs by `_P`/`_N` naming; `class:` covers rule grouping.
- **Arrays/repetition** (`led[8]`) — LLMs unroll loops effortlessly.
- **Buses, hierarchical sheets, graphics beyond `note`, SPICE directives** — non-goals for v1; hierarchy is the designated `version: 2` headline.

### 5.7 Kernel vs. sugared (same circuit, identical after desugar)

```yaml
# ----- kernel only -----                 # ----- sugared -----
nets: {3V3: {power: true},                rails: [3V3, GND]
       GND: {power: true}}
U1:                                       U1:
  part: MCU_ST_STM32H7:STM32H743VITx        part: MCU_ST_STM32H7:STM32H743VITx
  pins: {VDD: 3V3, VSS: GND,                decouple: {100nF: 10, 4.7uF: 2}
         PB6: I2C1_SCL, PB7: I2C1_SDA}      pins: {VDD: 3V3, VSS: GND,
C1: {part: Device:C, value: 100nF,                 PB6: I2C1_SCL, PB7: I2C1_SDA}
     pins: {1: 3V3, 2: GND}}              R7: {part: R, value: 4.7k,
# … C2–C12 identical …                         between: [I2C1_SCL, 3V3]}
R7: {part: Device:R, value: 4.7k,
     pins: {1: I2C1_SCL, 2: 3V3}}
```

---

## 6. Compile pipeline: the validation gauntlet

Every `apply_design`/`validate_design` runs:

```
parse (YAML 1.2; single doc only, dup keys = error) → strict schema check → desugar → kernel model
  → semantic lints:
      parts exist in libs (fuzzy suggestions on miss)
      pins resolve on symbols ("pin 'PB66' not found on U1 — did you mean PB6?")
      pin-on-one-net · power-input-connected · polarized-`between` warn
      single-pin-net warn · near-name warn · unreferenced-net warn
  → reconcile → diff report → [approval gate] → write + snapshot
  → kicad-cli sch erc → parsed findings back to agent
```

**Every error message is designed for LLM self-repair:** precise location, machine-parseable, with a suggested fix. The diff report doubles as the copilot transparency primitive — agent and user both see "adds U1, 12×C, R7; modifies net I2C1_SDA" before anything is written.

**Staleness guard:** the lift records a content hash of the `.kicad_sch`. If the file changed between proposal and approval (user saved in eeschema), the apply aborts, re-lifts, and re-validates against fresh state.

---

## 7. Reconciliation engine

- **Identity:** reference designator (synthesized parts: `(ap_parent, ap_role, ap_index)` tags). Refdes rename = delete + add (positions lost for that part only; documented behavior).
- **Survivors keep their positions** — including user-made moves. Only new components are placed. "Re-layout" is an explicit verb (user- or agent-requested with approval), never a side effect.
- **Deletion** garbage-collects only the component's own attached artifacts (its power symbols, labels, NC markers, wires terminating at its pins), with **net-local rewiring** of affected nets only.
- **Conservative rule for user geometry** (the engine's hardest corner): never delete user-drawn wires/labels unless the net they implement actually changed in the diff; then surgical removal + relabel. When in doubt, prefer leaving user geometry and attaching via labels.

---

## 8. Placement engine (MVP)

**Visual bar: "tidy generated, ERC-clean, readable" — not hand-beautiful.** Deterministic: same input ⇒ byte-same output (snapshot-testable).

1. Symbol bounding boxes from library symbols; **all pins on the 50-mil (1.27 mm) grid** (mandatory for KiCAD connectivity).
2. Blocks → rectangular regions: edge-pinned blocks honor `layout:` hints; the rest pack center; generous margins (sheet space is free). Block title text rendered from block name (+ `note`).
3. Within a block: ICs anchor; passives cluster at their most-connected pin side; `decouple`-tagged caps rank up in a row beside their parent.
4. Wiring policy: power pins → power symbols; short intra-block runs → drawn manhattan wires + auto-junctions; everything else → **local net labels** (sufficient on a flat sheet; global labels only matter multi-sheet).
5. Deferred "prettify" phase (post-MVP): force-directed refinement, aesthetic wire routing.

---

## 9. KiCAD integration

- **File I/O:** `kiutils_kicad`, `WriteMode::Lossless` — minimal diffs, user formatting preserved.
- **Symbol library index:** discover installed libs (`/usr/share/kicad/symbols`, KiCAD config paths, project libs); parse `.kicad_sym` for pin tables, bboxes, unit counts. Backs `search_symbols`/`get_symbol_info` and the placer.
- **`kicad-cli`:** netlist export (lift's connectivity oracle) + ERC (post-write validation). Hard requirement; missing binary → actionable startup error.
- **IPC (`kicad-ipc-rs`), Pillar 1 uses:** detect running KiCAD + open project; **investigate** triggering eeschema reload via IPC `RunAction` (open question §15). Pillar 3 will use the full surface.
- **Reload handshake:** after write, KiCAD prompts "file changed externally — reload?". Mitigation ladder if detection proves unreliable: (1) IPC RunAction, (2) status-bar instruction "File → Revert in eeschema".
- **File watcher:** user saves in eeschema → TUI marks design state dirty → agent re-lifts before next action.
- **Snapshots:** every write copies the previous `.kicad_sch` to `.auto-pcb/history/<n>-<timestamp>.kicad_sch`; `:undo` restores.

---

## 10. Agent

- **Providers:** delegated to `rig-core`/`rig-bedrock` (BYOK config: `~/.config/auto-pcb/config.toml` + env for keys; provider/model/base-url selectable).
- **Loop: ours.** rig's `Agent` abstraction wants to own the tool cycle, but our cycle has a human gate (`apply_design` → diff card → approval) and TUI streaming hooks, so we drive rig's completion API inside our own loop. A single adapter module isolates rig types from the rest of the codebase.
- **System prompt** embeds the full circuit-YAML spec (kernel + hints + sugar, ~1.5k tokens) and workflow doctrine: *search symbols before referencing; validate before applying.*
- **Design state is tool-pulled, not context-pushed** — the agent calls `get_design` when needed; keeps long sessions lean.

### Tool surface (complete for Pillar 1 — six tools)

| Tool | Contract |
|---|---|
| `get_design()` | lift current sch → canonical kernel YAML + stats |
| `search_symbols(query)` | fuzzy search installed sym libs → lib_ids + pin counts (anti-hallucination) |
| `get_symbol_info(lib_id)` | full pin table (names/numbers/types/units) |
| `validate_design(yaml)` | dry-run gauntlet, no write |
| `apply_design(yaml)` | full gauntlet → diff card → (gate) → write + snapshot + post-ERC → final report |
| `run_erc()` | `kicad-cli sch erc`, parsed findings |

---

## 11. TUI — the copilot cockpit

**Philosophy: KiCAD is the renderer.** The TUI never draws schematics; eeschema shows the truth. The TUI shows conversation, intent, and diffs.

```
┌─ auto-pcb ── bluepill.kicad_pro ─────────── [KiCAD ●] [sch synced] ─┐
│ chat transcript (streaming, tool-call cards collapsed)              │
│   ▸ search_symbols("STM32H743") → 4 hits                            │
│   ▸ validate_design → ok (2 warnings)                               │
├──────────────────────────────────────────────────────────────────────┤
│ ◆ PROPOSED CHANGES          +U1 STM32H743VITx (mcu)   +12C  +R7     │
│   nets: +USB_DP +USB_DM +I2C1_SDA(3 pins)…    [a]pprove  [r]eject   │
├──────────────────────────────────────────────────────────────────────┤
│ > input…                                                             │
└─ openai:gpt-5.2 · 14.2k tok · $0.21 ────────────── :help :undo ─────┘
```

- **Apply gate default ON:** propose → diff card → approve → write. `:auto` toggles yolo mode.
- **`:undo`** restores from snapshot history.
- Esc cancels in-flight agent turns; status bar: provider/model, token + cost meter.
- Session transcript persisted to `.auto-pcb/session.json` (resume).
- Stack: `ratatui` + `crossterm` + `tokio` (concurrent LLM streaming, file watching, IPC).

---

## 12. Crate layout (workspace from day 1)

```
crates/
  circuit-lang/   # YAML 1.2 parse, strict schema, desugar, lints, canonical emit — PURE, no I/O
  kicad-bridge/   # kiutils wrapper, sym-lib index, kicad-cli (netlist/ERC), IPC client, watcher, snapshots
  sch-engine/     # reconciler (kernel-model diff), placer, wire/label policy → emits via kicad-bridge
  agent/          # rig adapter, tool registry, agent loop
  autopcb/        # bin: ratatui app wiring it all
```

Dependency DAG: `circuit-lang` ← `sch-engine` → `kicad-bridge`; `agent` orchestrates; `autopcb` renders. `circuit-lang` purity ⇒ exhaustively unit-testable. The current single-crate skeleton is restructured into this workspace.

---

## 13. Testing strategy

- **`circuit-lang`:** unit + golden desugar tables + round-trip property (parse → canonical emit → parse idempotent).
- **`sch-engine`:** `insta` snapshot tests — YAML in, `.kicad_sch` out, byte-snapshotted (determinism makes this possible); reconcile scenarios (add/remove/rename/user-moved/user-rewired) as table tests.
- **`kicad-bridge`:** integration suite gated on KiCAD being installed (`kicad-cli` netlist/ERC against fixture schematics).
- **E2E:** use real LLMs (./.env contains a valid AWS_BEARER_TOKEN_BEDROCK key) to test the full agent. **The bluepill-H7 design is the canonical fixture** — the founding prompt is the acceptance test, asserted ERC-clean.

---

## 14. Error handling

- **Validation/compile errors** → structured, suggestion-bearing, fed to the LLM for self-repair (§6).
- **KiCAD absent / `kicad-cli` missing** → actionable startup diagnostics; sch editing still works without a running GUI (file mode), with reduced status info.
- **IPC unavailable** (KiCAD closed mid-session) → degrade gracefully: file mode + reconnect attempts; header reflects state.
- **LLM provider errors** → retry with backoff; surface in transcript; never lose user input.
- **Concurrent edits** → staleness hash guard (§6); pending diffs are invalidated and recomputed, never blindly applied.
- **Every write is snapshot-protected** (§9) — worst case is one `:undo` away.

---

## 15. Risks & open questions

| Risk | Mitigation |
|---|---|
| `kiutils_kicad` is alpha; mutation helpers thin | CST-level edits where needed; upstream contributions; pinned version |
| eeschema external-change reload detection unverified in KiCAD 10 | Plan-phase investigation: IPC `RunAction` trigger; fallback: "File → Revert" instruction |
| Reconciling user-drawn geometry (free-form wires) | Conservative rule §7; kicad-cli netlist as authoritative semantics regardless of geometry |
| Symbol lib paths vary across distros/installs | Multi-path discovery + config override + clear diagnostics |
| rig API churn | Single adapter module; re-verify crate at plan time |

---

## 16. Roadmap: Pillars 2 & 3 (sketches — separate specs later)

- **Pillar 2 — Part sourcing:** web search for real MPNs; footprint/symbol acquisition (LCSC/easyeda2kicad, SnapEDA-style pipelines) into project libs; fills the kernel's existing `footprint:` and `props.MPN` slots. New tools: `search_parts`, `import_part`, `assign_footprint`. The kernel is already shaped for it.
- **Pillar 3 — PCB layout/routing:** the live-IPC copilot via `kicad-ipc-rs`; netlist→board sync; the same block model and `layout:` hint vocabulary drive board placement ("intent → deterministic engine" paradigm reused); `class:` feeds routing rules; physics-aware = current/thermal/length constraints; routing engine choice (FreeRouting bridge vs native) is its own brainstorm.
- **Language v2:** hierarchical sheets (the `/` in net names is reserved for this), buses.

## 17. Non-goals (v1)

Hierarchical sheets · buses · SPICE/simulation · component arrays · custom symbol *creation* (library management is Pillar 2 territory) · schematic rendering in the TUI · headless operation · KiCAD ≤ 9 support.
