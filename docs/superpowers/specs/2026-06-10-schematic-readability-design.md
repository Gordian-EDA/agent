# Schematic Readability: From ERC-Clean to Near-Human Professional

**Date:** 2026-06-10
**Status:** PARTIALLY SUPERSEDED — Phases 1–2 (and the draft workspace) are
implemented as designed (see `2026-06-10-schematic-readability-slice1.md`).
**Phases 3–4 below are obsolete**: the idiom-library approach was replaced by a
structural layout grammar. See `2026-06-10-layout-grammar-design.md` for the
current design of everything past Phase 2 (it also carries the vision loop
forward). The Phase 3–4 sections are kept only as historical context.
**Quality bar:** Near-human professional (reference: hand-drawn BluePill schematic with
titled sections, power symbols, idiomatic sub-circuit layouts)

## Problem

The current pipeline produces ERC-clean but unreadable schematics:

1. Net labels are stamped at exact pin endpoints with rotation 0 — they overprint pin
   names, refdes/value text, and each other.
2. Power nets are expressed as text labels (`GND`, `3V3`) at every power pin instead of
   graphic power symbols — the single largest source of visual noise.
3. No wires at all; labels sit directly on pins with no breathing room.
4. Blocks are invisible: no frames, titles, or annotations; decoupling caps render as
   floating label-pairs.
5. Everything is placed at angle 0 on a fixed 25.4 mm grid — passives are never
   oriented to their role, big ICs are cramped while passives waste space.

## Decisions (from brainstorming)

- **Quality bar:** near-human professional, not merely tidy.
- **Intelligence split:** hybrid. The engine owns deterministic layout primitives and an
  idiom library; the LLM chooses idioms and arrangement via semantic DSL hints, with a
  per-component escape hatch.
- **Vision feedback:** the agent renders the schematic to an image and iterates on
  hints based on what it sees.
- **Invariant preserved:** same YAML → byte-identical `.kicad_sch`. The vision loop
  edits YAML hints only, never output coordinates directly.

## Architecture: four phases, each independently shippable

```
Phase 1: render_schematic tool        (agent, llm)        — see the problem
Phase 2: engine layout primitives     (sch-engine)        — 80% of the visual gap
Phase 3: idiom library + DSL hints    (circuit-lang, sch-engine)
Phase 4: vision critique loop         (agent prompt + flow)
```

Cross-cutting (lands with Phase 1, used by all later phases): the `.autopcb/` draft
workspace and incremental-edit tool surface (see "Draft workspace" below).

## Phase 1 — `render_schematic` tool

New agent tool exposing the current schematic as an image.

- **Pipeline:** `kicad-cli sch export svg` → rasterize to PNG with the `resvg` crate →
  base64. (kicad-cli 10 has no PNG export for schematics; KiCAD strokes text as paths
  so resvg needs no font setup. Fallback if fidelity disappoints: shell out to
  `rsvg-convert`.)
- **Resolution:** ~150 DPI (A4 ≈ 1750×1240 px) — under Bedrock size limits, near
  Claude's 1568 px sweet spot.
- **LLM plumbing:** add `ContentBlock::Image { format, data }` to `agent/src/llm.rs`
  and map to Bedrock Converse `{"image": {"format": "png", "source": {"bytes": …}}}`
  inside tool results.
- **Tool shape:** `render_schematic {region?: "full" | <block_name>}` — full sheet
  default; per-block crop (from the block's placement bbox) for zoomed inspection.
  Crop support may land after the full-sheet version.
- **TUI:** PNG written to `.autopcb/renders/render-NNN.png`, path shown in transcript.
- **Dev harness:** script renders every `validation/` example for eyeball regression
  checks while building later phases.

## Draft workspace (`.autopcb/`) + incremental editing

Today the agent's only write path is `apply_design {yaml}` — whole-document
replacement, with no persistent YAML anywhere (`get_design` lifts the `.kicad_sch` on
demand). Whole-document rewrites are token-expensive per iteration and are where silent
mutations happen (the netlist-level diff gate won't catch a quietly dropped `note:` or
hint). The vision loop multiplies both costs.

**Project-local state directory** (gitignored by default):

```
.autopcb/
  draft.circuit.yaml    # the persistent working draft — source for apply_design
  draft.meta.json       # sch content-hash the draft was seeded from, timestamps
  renders/              # render-NNN.png from render_schematic
  session/              # reserved: transcript/context for a future resume feature
```

**Tool surface changes:**

- `create_design {yaml}` — seed `draft.circuit.yaml` from scratch (anchored edits
  cannot create from nothing). Fails if a draft exists unless `overwrite: true`.
- `edit_design {old_string, new_string, replace_all?}` — anchored string replacement on
  the draft, Claude-Edit-style: `old_string` must match exactly once (or pass
  `replace_all`); no match / ambiguous match → clean error. Anchored replacement is
  deliberately chosen over diff/patch formats, which LLMs emit unreliably. Each edit
  response includes compile diagnostics for the resulting draft, so the model gets
  immediate validation feedback per edit.
- `get_design {}` — returns the draft if present; otherwise lifts the `.kicad_sch`
  **and seeds the draft from the lift** (canonical form — lift output is sorted, so
  `old_string` anchors are stable), making `edit_design` immediately usable.
- `apply_design {commit?}` — `yaml` becomes optional: omitted → applies the current
  draft. Passing `yaml` explicitly still works (one-shot use, back-compat).

**Staleness rule:** `draft.meta.json` records the content hash of the `.kicad_sch` the
draft was seeded from. If the schematic changed since (user edited in KiCAD),
`get_design`/`apply_design` surface a conflict note instead of silently clobbering; the
agent re-lifts and merges deliberately.

The existing snapshot store and the session-resume feature itself are out of scope
here; `.autopcb/` just gives them an obvious home later.

## Phase 2 — Engine layout primitives (deterministic, minimal DSL change)

In order of visual impact:

1. **Power symbols replace power-net labels.** Any pin on a net in `rails:` gets a
   graphic power symbol plus a short wire: `power:GND` pointing down below the pin,
   positive rails (`power:+3V3`, `power:VBUS`, …) pointing up above it. Stock
   `power:` lib mapping by net name; arbitrary rail names use a generic bar symbol
   with its **Value** field overridden (KiCAD derives the net from Value).
   ⚠ **Day-one spike:** the Value-rename trick must be verified with ERC + netlist
   round-trip before anything builds on it (known KiCAD rename quirks).
2. **Wire stubs + oriented labels.** Non-power pins get a short wire (~3.81 mm) in the
   pin's direction with the net label at the far end, rotated/justified per pin
   orientation. Pin direction comes from `PinGeom`. Labels can no longer overprint the
   symbol body.
3. **Role-aware passive orientation.** A two-pin passive `between: [RAIL, GND]` draws
   vertical: rail symbol on top, GND below. Requires carrying a layout-role hint from
   desugar into the kernel `Component` (new optional `layout` field).
4. **Decoupling banks.** Synthesized decouple clusters render as a horizontal row of
   vertical caps with aligned top-rail/bottom-GND, placed beside the parent IC. (First
   idiom, engine-side, applied automatically.)
5. **Bbox-aware cells.** Cell pitch derives from actual symbol bounding box + label
   clearance instead of fixed 25.4 mm. Passives pack densely; big ICs get room.
6. **Block frames + titles.** Each block region gets a styled title (`title:` or block
   name) and a thin divider/rectangle; block `note:` becomes a sheet annotation.
7. **Field placement.** Refdes/value positioned on the symbol side that wires/labels
   don't use.
8. **Layout lint.** Engine computes text/symbol bboxes and reports deterministic
   warnings via `apply_design` ("label X collides with U2 pin name", "block A overlaps
   block B"). Doubles as a test oracle and as free pre-vision feedback to the LLM.

## Reconciliation & lift impact

Three-part answer, resolved as follows:

**Decoration is regenerated (free).** Reconciliation's two-layer ownership model is
unchanged: symbols (position/angle/uuid) are preserved and user drags win; connectivity
decoration is regenerated every emit from current symbol positions. Power symbols, wire
stubs, frames, and field offsets join the decoration layer — drag a symbol in KiCAD and
its stubs/labels/power symbols follow on the next emit. Lift is unaffected: power
symbols carry `#PWR` refdes prefixes (netlist-excluded, like `#FLG`), stubs only express
connectivity, frames are invisible to the netlist. Hand-edits to decoration are
engine-owned and get regenerated away (already the contract for labels; documented).

**Block metadata rides on components (lossy → fixed).** Block `title:`/`note:` and
Phase 3 hints round-trip through lift via hidden `ap_*` properties stamped on the
block's components (same mechanism as `ap_block`), keeping lift netlist-only.

**Layout revision hashing (the real fix).** Position preservation would otherwise
defeat both the new placer (existing sheets never improve) and the vision loop
(changing a hint would change nothing visible). Each component is stamped with
`ap_layout_rev` = hash of its placement-relevant inputs (block hints + idiom + own
layout fields). On reconcile, per block:

- hash unchanged → preserve prior positions (user drags survive),
- hash changed → the block is re-placed fresh.

Conflict rule: if the user dragged symbols in a block *and* the agent re-hints that
block, the hint wins (a re-hint is an explicit instruction to rearrange). Explicit
escape hatch: `apply_design {relayout: "all" | ["block", …]}` — also the migration path
for pre-Phase-2 sheets. The apply-gate diff calls out relayouts explicitly ("block
`mcu`: layout changed, 14 components re-placed") so they are never silent.

## Phase 3 — DSL hints + idiom library (OBSOLETE — superseded by `2026-06-10-layout-grammar-design.md`)

Surface syntax:

```yaml
blocks:
  power:
    title: "Power Supply"            # frame title; defaults to block name
    note: "AMS1117-3.3, 800 mA max"  # free-text sheet annotation
    layout: {sheet: bottom-left}     # 3x3 sheet grid; `edge` kept as alias
    components:
      U1:
        part: Regulator_Linear:AMS1117-3.3
        idiom: regulator             # in-cap left, device center, out-cap right
        pins: {VI: VBUS, VO: 3V3, GND: GND}
      C1: {part: C, value: 10uF, between: [VBUS, GND]}   # rail_passive, automatic
      R5:
        part: R
        place: {at: [50.8, 76.2], rot: 90}   # escape hatch: sheet mm, snapped + linted
```

- **Model:** `LayoutHint` gains `sheet:` (3×3 grid enum: `top-left` … `bottom-right`);
  `Component` gains `idiom:`, `orient:`, `place:`. `edge:` maps onto sheet positions
  for back-compat.
- **Lint:** unknown idiom → error with fuzzy did-you-mean (SkimMatcherV2, house
  convention); `place:` collision → warning.
- **Idiom registry v1** (Rust functions: cluster → relative positions/rotations/wires):
  - `rail_passive` — automatic for `between: [rail, rail]`; vertical, rail up, GND down
  - `decoupling_bank` — automatic for decouple groups; row of vertical caps
  - `regulator` — opt-in; input cap left, device center, output cap right
  - `connector_fanout` — opt-in; connector at block edge, stubs + labels fanning out
  - `led_indicator` — opt-in; rail → resistor → LED → GND vertical chain
  Registry designed for cheap additions.
- **Placement integration:** idioms produce cluster-local layouts; the bbox-aware block
  packer arranges cluster envelopes; the sheet-grid hint places block regions on an
  A4-modeled sheet with the title-block area reserved.
- All hints feed `ap_layout_rev` and round-trip via `ap_*` properties.

## Phase 4 — Vision critique loop (OBSOLETE HERE — carried forward, unchanged in design, into `2026-06-10-layout-grammar-design.md`)

- **Flow:** after an approved apply, the agent calls `render_schematic`, judges the
  image against a layout rubric in the system prompt (no text overlap, power up / GND
  down, signal flow left→right, related parts adjacent, blocks titled, sheet balanced),
  and edits *hints* and re-applies if defects remain. Hard cap: 3 vision rounds per
  request (configurable). Iterations use `edit_design` against the draft — a few
  anchored hint edits per round, not a whole-document rewrite.
- **Lint before vision:** deterministic layout lint runs first (free, textual) so
  vision rounds are spent on what only vision can see.
- **Apply-gate ergonomics:** the diff classifier marks diffs **layout-only** (netlist
  identical); the TUI gains an "auto-approve layout-only diffs" toggle so iteration
  doesn't nag the user. Connectivity-changing diffs always gate.

## Testing

- **Spike first:** power-symbol Value rename for non-stock rails, ERC + netlist
  round-trip verified.
- Unit: label-orientation math, power-symbol mapping, idiom geometry, layout-rev
  hashing.
- Golden `.kicad_sch` snapshots per primitive; existing ERC-clean integration tests
  stay green throughout.
- **Layout lint as oracle:** zero collisions on the BluePill and `validation/` examples.
- Round-trip: hints survive lift; layout-rev change re-places exactly the affected
  block; unchanged blocks keep user-moved positions.
- Dev visual harness renders all validation examples to PNG.
- Acceptance: BluePill demo generated end-to-end with the vision loop, judged against
  the human reference schematic.

## Risks

| Risk | Mitigation |
|------|------------|
| KiCAD power-symbol Value rename quirks | Day-one ERC-verified spike before dependent work |
| resvg fidelity on KiCAD SVG output | Fallback: shell out to `rsvg-convert` (installed) |
| Vision loop cost/latency | Bounded rounds (3), lint-first ordering, layout-only auto-approve |
| Idiom generality (novel circuits) | Idioms degrade to Phase 2 primitives + bbox grid; registry built for additions |

## Out of scope

- Multi-sheet / hierarchical schematics (single A4 sheet assumed).
- Auto-routed signal wires between blocks (labels remain the inter-block connectivity).
- Session resume (the `.autopcb/session/` directory is reserved for it, nothing more).
- PCB layout.
