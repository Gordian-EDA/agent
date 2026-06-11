# Layout Grammar: Structural Rules Instead of an Idiom Library

**Date:** 2026-06-10
**Status:** Approved design, pending implementation plan
**Supersedes:** Phases 3–4 of `2026-06-10-schematic-readability-design.md` (the idiom
registry + DSL hints and the vision-loop phase). Phases 1–2 of that spec are
implemented and unchanged; this spec covers everything still to build.

## Problem

Phase 2 produces ERC-clean sheets whose parts all *float*: every connection is a
net-label pair or an isolated power symbol, so a pulldown reads as "a resistor with a
temporary label" instead of a wired sub-circuit, decoupling banks render as N
independent cap-and-two-symbols islands, and nothing on the sheet is visually joined.

The prior answer was an idiom *library* (`regulator`, `decoupling_bank`,
`led_indicator`, …). Studying real professional sheets (MCP1703 power entry, 555
blinker, divider + filter, UART level translator — see
`docs/validation/references/`) showed the library approach scales badly: every new
sub-circuit pattern needs a new hand-written idiom, and the four-example sample
already demanded idioms nobody had planned (crystal, switch-pull, series fuse,
pullup farm).

The insight: all those idioms are special cases of a tiny structural **grammar**.
Professional schematics follow general rules — series elements chain along the
signal flow, shunt elements hang vertically between their node and a rail, parallel
shunts collect onto shared buses, small networks attach to the IC pin they serve. An
engine that classifies the netlist graph by those rules generates the professional
layout directly, including patterns nobody wrote down.

## Decisions (from brainstorming)

- **Roles are inferred, never assigned.** Classification reads only the netlist
  graph (pin counts from symbols, rail-ness from `rails:`). The LLM's levers stay
  what they are: parts, blocks, rails, `sheet:`/`title:` hints, the `place:` escape
  hatch, and the vision loop. No new YAML syntax (YAGNI; a `flow:` override can be
  added later if inference proves insufficient).
- **Cluster is the placement unit.** Wired sub-circuits move as rigid groups.
  Whole-group drags in KiCAD survive reconciliation; dragging one member of a
  cluster gets re-normalized on the next emit. No router exists or is planned.
- **Pin-anchored clusters.** A cluster whose external nets land on pins of one
  anchor is placed against that anchor and joined by short straight wires —
  this is what makes hand-drawn sheets read as connected.
- **Inter-cluster connectivity stays label-based.** Matches hand practice
  (sub-circuits joined by named nets) and the logic-board reference, where labels
  beat wire spaghetti. The contract: labels appear once per net per cluster
  *endpoint*, not once per pin.
- **Acceptance fixtures are real schematics**: the four reference images are
  reproduced from circuit-YAML and judged in the loop.

## Invariants preserved

- Same YAML → byte-identical `.kicad_sch`.
- Layout never changes the netlist (grammar may move every coordinate, never a net).
- ERC-clean output.
- All generated geometry is decoration: regenerated every emit, invisible to lift.

## The grammar

### Role classification (per block, mechanical)

- **Anchor** — any component with ≥3 pins (ICs, connectors, transistors, pots), or
  any component with `place:`. Placed and preserved individually.
- **Chain element** — every 2-pin component.

`PinGeom` gains the pin **electrical type** (input/output/power_in/passive/…),
parsed from the symbol library; it is present in the s-expression but unparsed
today. Used for flow direction below.

### Chain formation

Chains form by walking nets that join **exactly two chain-element pins**. Extra
taps on a net (shunt chains, labels, anchor pins) do not break the walk; they
attach at that node. A chain terminates at: a rail, an anchor pin, or an *open*
net (cross-block, or any net that doesn't continue the chain).

A **chain classifies by its two endpoints**:

| Endpoints | Rendering |
|---|---|
| rail → rail | vertical run: positive rail top, GND bottom (divider, LED string, decoupling cap) |
| signal/pin → rail | hangs vertically from its node toward the rail (filter cap, pulldown) |
| signal/pin → signal/pin | horizontal series run, flow left→right (fuse, inline resistor) |

- **Bank**: parallel chains sharing the same node pair (decoupling caps). Subsumes
  the existing decouple-cluster logic; banks synthesized from `decouple:` keep
  their parent association and are placed beside the parent anchor even though
  their nets (being rails) never pin-anchor.
- **Cluster**: a chain plus its attached hangs/banks/taps — the rigid unit. A
  standalone bank or single chain with no attachments is a cluster by itself.
- **Flow direction** (horizontal chains): the end driven from an `output`-type pin
  goes left, `input`-type right; ties broken by net name. `between: [A, B]` order is
  the documented final tiebreak (netlist-irrelevant, layout-relevant).
- **Cycles** (feedback) are broken at the lexicographically smallest refdes; that
  link degrades to a label pair.

Worked examples (the fixtures):

- *MCP1703 power entry*: chain `5V_BUS → F1 → U1.VI` with bank {C1,C2} on its node;
  chain `U1.VO → 3V3` with bank {C3,C4}; vertical chain `3V3 → D1 → R2 → GND`; U1
  anchors both sides.
- *Divider + filter*: one vertical chain `VCC → R7 → R8 → GND`; C3 and the `OUT`
  label tap the middle node.
- *555 blinker*: R1, R2 are chains terminating at anchor pins; C1, C2 hang to GND;
  `Q → R3 → D1 → GND` is a pin→rail chain (L-exit). The TR–THR same-anchor tie and
  the reset-to-rail tie degrade to labels / power symbols, counted by lint.
- *Level translator*: inline 62R resistors are pin→pin chains anchored at both ends;
  10K pullups hang to VCC; C14/C16 are rail→rail banks. Conscious simplification:
  same-rail pullups each get their own rail symbol in v1 (no cross-net bus merging).

## Cluster geometry generation

All geometry is closed-form arithmetic in cluster-local coordinates — no search, no
routing.

- **Horizontal chain**: elements in a row on the spine, wires joining consecutive
  elements, pitch = element bbox + ~5 mm. Tap-bearing nodes get a junction dot.
- **Vertical chain**: positive-rail symbol top, elements stacked downward, GND
  bottom.
- **Hangs**: straight down to GND / straight up to a positive-rail symbol.
- **Banks**: side-by-side at ~7.62 mm pitch, shared top and bottom **bus wires**,
  junctions along the bus, **one** power symbol per bus.
- **L-exits** (anchor pin → rail chains): leave the pin horizontally one grid run,
  turn 90°, run vertically to the rail.
- **Rail risers**: a spine node on a positive rail gets one riser + symbol above the
  spine.
- **Open endpoints**: oriented net label at the node (existing machinery).
- Refdes/value text on the side the wiring doesn't use (existing field placement).

### Pin anchoring

A cluster whose external nets land on pins of one anchor is placed against it:
spine aligned to the pin's y, joined by a short straight wire. Multi-pin anchoring
applies when the pins are on the same side in compatible order (crystal → XIN/XOUT;
inline resistors → B1/B2). Infeasible geometry (occupied slot, wrong side, order
mismatch) falls back to label connection and free placement. Anchoring priority is
deterministic: most anchored pins first, then refdes order.

## Emitter changes (`emit.rs`)

1. **`(junction …)` elements** with content-derived stable UUIDs (same pattern as
   wires).
2. **Net-aware occupancy.** Cluster wires register their net identity in the
   stub-retraction occupancy model. A same-net touch is a *deliberate join*
   (junction when ≥3 ends meet) instead of a collision; foreign-net touches keep
   the retract-and-lint behavior. This is the load-bearing change — today the pass
   treats any touch as an accident.
3. **Bus wires are plain wires**, emitted with stable ordering so byte-identical
   output holds.

Lint: same-net joins stop counting as collisions; new counters for degradations and
a sparseness warning (block envelope ≫ sum of content bboxes) so the vision loop
spends rounds on judgment, not mechanics.

## Macro placement

Within a block, in priority order: **anchors** (deterministic order, largest
first) → **pin-anchored clusters** into their slots → **free clusters** packed
around them. Packer proportionality fixes: clearance only on sides that carry
labels/stubs, vertical centering within rows, envelopes from real content bboxes.
Block-to-sheet keeps the prior spec's surviving parts: `sheet:` 3×3 grid with the
title-block area reserved; band stacking as the no-hint fallback.

## Reconciliation — three ownership layers

- **Anchors**: individually preserved; user drags win (unchanged).
- **Clusters**: identity = hash of sorted member refdes list. The cluster *origin*
  is preserved across emits: the representative member (lexicographically smallest
  refdes) keeps its prior position and the group translates with it. Whole-group
  drags survive; partial drags re-normalize on next emit.
- **Decoration** (wires, junctions, buses, power symbols, labels, fields):
  regenerated every emit.

`ap_layout_rev` now hashes cluster membership + chain structure + block hints; a
change re-places that block. The `relayout:` escape hatch and apply-gate relayout
callouts are unchanged. Lift is unaffected (all new geometry is netlist-invisible).

## Degradation ladder (explicit, linted, never blocking)

1. Ambiguous link (net joining 3+ chain pins; cycle) → break deterministically →
   label pair.
2. Pin-anchor infeasible → free cluster + labels.
3. Unclassifiable component → Phase 2 cell + stub + label.

Each step increments a lint counter, e.g. `block power: 2 links degraded to labels`.

## DSL surface

Frozen. `idiom:` and `orient:` are removed from the plan (never implemented).
Surviving hints: `title:`, `note:`, `sheet:` (3×3 grid, `edge:` alias), `place:`
(component becomes a fixed anchor). `between: [A, B]` order is the direction
tiebreak.

## Vision critique loop (carried over, unchanged in design)

After an approved apply, the agent renders, judges against the layout rubric, and
edits hints/YAML via `edit_design`; ≤3 rounds; deterministic lint runs first;
layout-only diffs can auto-approve in the TUI. The rubric gains the grammar's
vocabulary (chains wired, banks bused, single power symbol per bus).

## Testing

- **Unit**: chain extraction + endpoint classification as table tests (divider and
  555 are rows), cycle-break determinism, bank grouping, L-exit/riser geometry
  math, junction UUID stability, net-aware occupancy (same-net → junction,
  foreign → retract).
- **Fixtures** (checked in beside the reference PNGs in
  `docs/validation/references/`): circuit-YAML for `mcp1703-power-entry`,
  `555-blinker`, `divider-filter`, `uart-level-translator`. Each gets a golden
  `.kicad_sch` snapshot, netlist-matches-YAML oracle, ERC clean, zero foreign
  collisions, plus structural assertions (one power symbol per bus, junction
  counts, bank pitch, divider renders as a single vertical chain).
- **Render loop**: the existing validation render harness produces a PNG per
  fixture next to its reference; acceptance is vision-judged comparison against the
  references. `logic-board-spaghetti.png` is the negative control: its
  anchor-to-anchor nets stay labels by design.
- **Regression**: BluePill + existing validation examples stay ERC-clean with zero
  collisions; netlist invariance across the grammar change.

## Risks

| Risk | Mitigation |
|------|------------|
| Same-net-touch whitelist accidentally legalizes a real short | Foreign-net behavior unchanged; netlist-invariance oracle on every fixture; ERC stays a gate |
| Weird graphs (analog meshes, feedback) classify poorly | Explicit degradation ladder to Phase 2 behavior, linted, never blocking |
| Cluster re-normalization surprises users who drag one part | Apply-gate diff calls out re-placements; whole-group drags survive; documented contract |
| Flow inference wrong on exotic pin types | Deterministic tiebreaks; `between:` order as final say; `flow:` hint reserved as future escape hatch |

## Out of scope

- Inter-cluster / anchor-to-anchor routed wires (labels remain).
- Cross-net bus merging (shared VCC bus over different signal nets — pullup farms).
- Multi-sheet schematics; session resume; PCB layout.
- Any router or search-based placement.
