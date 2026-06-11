# Schematic Aesthetics Overhaul — Design

**Date:** 2026-06-11
**Branch:** feat/layout-grammar (builds on the layout-grammar slice)
**Status:** Approved

## Problem

Engine output is electrically correct but visually unacceptable next to the
hand-drawn references in `docs/validation/references/`:

1. **Floating islands.** Wires are drawn only *inside* clusters. Every anchor
   pin gets a stub + net label, so the IC and its support circuitry are
   visually disconnected. The reference draws real wires with junctions.
2. **Text collisions.** Net labels land on pin names (`N_RST` over `RST`),
   adjacent power labels merge (`VCCDVCC3V3`), values print over rotated
   refdes (`62R15`). Stub retraction only handles stub-vs-net collisions.
3. **Column stacking.** Clusters stack in one vertical column below the
   anchor with large gaps, instead of arranging around the IC the way a
   human draws it (power up, ground down, signal flow left→right).

## Goals

- Local connectivity drawn as real wires; labels reserved for power/global
  and inter-block nets.
- Zero text collisions in fixture renders (strict overlap-lint oracle).
- Anchor-centric, idiomatic placement; corpus templates make common
  subcircuits come out the way a human would draw them.
- Agent-visible layout provenance and a narrow, declarative override.
- **Acceptance bar: iterate until every fixture render is judged — by
  side-by-side vision review against its reference — to match the quality of
  a professional human-drawn schematic.** "Lints pass" is necessary, not
  sufficient; the gate is visual.

## Non-goals

- No coordinate-level LLM control over layout (no position hints in
  circuit-lang). The override selects *which* template applies, never edits
  geometry. If neither template nor heuristics looks right, the fix is
  drawing a new template.
- No vision feedback loop for the agent this slice (renders are reviewed by
  the developing agent/human, not the authoring agent).
- No grid A*/global routing optimizer; the elbow+repair router is the design.
- No multi-sheet or hierarchical-sheet support changes.

## Design

### 1. Wire/label policy (governing rule)

| Net kind | Rendering |
|---|---|
| Power/global | Power symbols, as today. Hard convention: GND variants point down, V+ variants point up. |
| Local (within a block) | Real wires via the router. No stub+label pairs. |
| Inter-block | Net labels, as today. |
| Router failure | Fall back to today's stub+label pair for that net + lint warning. A bad route never aborts an emit. |

### 2. Anchor-centric block placement (`place.rs`)

The anchor IC is the center of its block. Each cluster is assigned a side
from its anchor-tap pin's exit direction: East pin → cluster right of the
IC, West → left; clusters whose far end is a V+ rail go above; ground-heavy
clusters go below. This generalizes the existing single-tap anchor-slot
optimization (place.rs:337) into the default strategy. Overlap resolves by
greedy outward push along the assigned side. Multi-anchor blocks: anchors
ordered left→right by signal flow (derived from chain direction), then
per-anchor side assignment. All positions stay on the 1.27 mm grid.

### 3. Elbow router (new `route.rs`)

Per local net:

1. Terminals = anchor pin endpoints + cluster tap points.
2. Minimum spanning tree by Manhattan distance picks which terminal pairs
   get drawn wires.
3. Per MST edge: orientation-aware elbow (first segment leaves the pin along
   its outward direction ≥ 2.54 mm), then a repair loop — find first
   obstacle collision (symbol bboxes, routed wires, placed text), generate
   candidate shifts as midpoints between pin/obstacle/prior obstacles,
   shift one interior segment, visited-set dedupe, expand
   shortest-Manhattan-first, accept first collision-free path. Iteration cap
   → degradation per the policy table.
4. Junction dots where ≥ 3 same-net segment ends meet. Different-net
   crossings emit nothing.

Rationale: elbow+repair yields 2–4 segment wires that read as hand-drawn,
and stays cheap. (Approach adapted from tscircuit's schematic-trace-solver.)

### 4. Label & field placement solver (new pass in `emit.rs`)

Runs after symbols and wires are fixed. Spatial index over: symbol bodies,
**pin name/number text extents** (from pin length + glyph-width estimate —
the missing obstacle class today), wire segments, placed text. Then:

- **Refdes/value fields:** candidates above/below/left/right of body,
  KiCAD-conventional first; best collision-free candidate by distance from
  the conventional spot.
- **Net labels** (inter-block + degraded): candidates enumerated along the
  net's own wire/stub segments, oriented along the wire, scored by clearance.
- No collision-free candidate → keep conventional position + lint warning,
  so the strict overlap oracle fails visibly in tests instead of silently.

Text metrics reuse the existing 1.1 mm/char × 1.6 mm model (emit.rs lint).

### 5. Corpus: template-matched placement

- **Authoring:** hand-drawn `.kicad_sch`, one template per file, in
  `crates/sch-engine/corpus/`. Drawn in KiCAD; the file is its own visual
  ground truth.
- **Compilation:** a `corpus build` step (runs where kicad-cli exists, i.e.
  dev/test machines) extracts geometry via the `parse_prior` path and
  connectivity via the `lift` netlist path, emitting a checked-in
  `corpus.json`. Runtime never needs kicad-cli.
- **Granularity:** one template = one block-sized subcircuit. circuit-lang
  blocks are already human-partitioned, so no partitioning stage is needed
  (simplification vs. tscircuit).
- **Matching:** Weisfeiler-Leman color refinement over a box-pin graph (pin
  colors: power class / signal / passive-2pin / anchor), Jaccard distance
  over color bags, linear scan. Distance below threshold → template wins.
- **Adapt v1:** assign components to template boxes by pin-color + degree;
  matched components inherit template positions/orientations; unmatched
  extras place heuristically beside their connected neighbor. No template
  editing — the router redraws all wires either way, which is what makes a
  loose adapt acceptable.
- **Initial corpus (~6):** 555 astable, RC divider+filter, LDO power entry,
  UART level translator, decoupling bank, LED+resistor — the reference set,
  so references double as templates and oracles.
- **Miss →** anchor-centric heuristics. Router and label solver run
  identically on both paths.

### 6. Agent steerability

- **Provenance reporting:** `EmitOutput` gains per-block layout provenance:
  `matched template "555-astable" (distance 0.12)` or `no match (best:
  "ldo-entry" at 0.61, threshold 0.35) — heuristic placement`, surfaced
  alongside lint warnings so the authoring agent can react.
- **Override directive:** per-block `layout.template: <name>` (force,
  bypass scoring) and `layout.template: none` (force heuristics) in
  circuit-lang. Unknown template name → compile error with strsim
  did-you-mean (per project convention).
- The override stops there — see Non-goals.

## Pipeline integration

```
grammar → cluster geometry → corpus match | anchor-centric placement
        → reconcile positions → route → label solve → retraction
        → lint → finish
```

Routed wires and solved labels are regenerated on every emit (never
reconciled from prior); symbol positions still honor `parse_prior`, with a
`layout_rev` bump since cluster geometry semantics change.

## Testing & acceptance

1. Existing fixture render harness extends to the new pipeline; strict
   overlap-lint oracle stays strict.
2. New wiring oracle: every local net is fully wired or has a recorded
   degradation note — no silent label fallbacks.
3. Netlist injectivity oracle (existing) remains the correctness backstop:
   layout changes must never change connectivity.
4. Router/label-solver unit tests on synthetic obstacle fields (collision-
   free guarantee, determinism, iteration-cap degradation).
5. Corpus round-trip test: each template, fed back as a fixture, must match
   itself at distance ~0 and reproduce its own positions.
6. **Visual gate (the bar):** render every `docs/validation` fixture and
   side-by-side review against `docs/validation/references/`. Iterate —
   tuning placement, router costs, label scoring, or adding/redrawing
   templates — until each render is judged professional-grade: fully wired,
   collision-free, idiomatic arrangement, sensible density. Done means
   "proud of it", not "tests pass".

## Build order

Each step independently landable with the harness green:

1. **Label & field solver** — kills text collisions even before routing.
2. **Elbow router** — local nets become real wires (biggest visual jump).
3. **Anchor-centric placement** — blocks arrange like the references.
4. **Corpus** — match/adapt + initial templates + provenance + override.
5. **Polish loop** — iterate per the visual gate until acceptance.

## Risks

- **Router obstacle model too coarse** → ugly detours. Mitigation: padded
  bboxes tuned on fixtures; degradation path keeps output correct.
- **Side assignment fights reconciliation** (prior positions pin clusters to
  stale sides). Mitigation: `layout_rev` bump forces fresh placement once.
- **WL matching too eager/shy** at corpus size ~6. Mitigation: threshold
  tuned on fixture set; provenance reporting makes mistakes visible; the
  override is the escape hatch.
- **Scope** — this is four subsystems. Mitigation: build order above is
  strictly incremental; each lands alone.
