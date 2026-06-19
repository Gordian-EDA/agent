# Schematic layout quality & the critic ceiling

A map of what moves the VLM critic score on agent-generated schematics, what doesn't, and
why — so the dead-ends below are not re-explored, and a future investment targets the one
direction with real headroom. Distilled from an exhaustive exploration sweep.

## The goal and the honest ceiling

Goal: reach a 9+ critic score (`tools/schematic_critic.py`) on 20+ e2e agent-generated
circuits across topics (MCU, BGA, dense, analog). **Current state: corpus critic mean
~6.5–7; ~1 board reaches 9. Uniform 9+ including dense boards is unreachable by engine
means.** This is not a metric artefact — two independent blind reviewers scored a board
spread *harsher* than the critic on sprawled boards (c01 3 vs critic 5; c03 5 vs 8), so the
critic is **accurate-to-generous**, and the layouts are genuinely the limit.

The single dominant defect across **every** topic is **SPRAWL** (modules far apart, large
empty regions, long interconnects). The critic penalises sprawl more than the extra wire
crossings that compaction would cost (measured: forcing a compact layout scored c01 6→7,
c20 5→6 *despite* more crossings). So the lever that would move the critic is compaction —
but it is unachievable cleanly (see below).

## What WORKS: agent-side modular blocks (shipped)

The corpus circuits were all generated as ONE flat `main` block, forcing the engine to
grid-pack every part → sprawl. The engine already lays each BLOCK out as a module and flows
blocks left→right. So the lever is upstream: the agent system prompt now partitions designs
into signal-flow blocks (power-entry / main IC + support / each peripheral).

- Controlled A/B (same c01 netlist): single-block→multi-block lifted median critic 5→6.
- Fresh e2e MCU board (STM32+IMU): auto-partitions into usb_power/mcu/imu, scored median 8.
- Effect is **organisation, not density**, so it sidesteps the compaction wall. Caps MCU
  boards ~7–8 (variable: board ordering / per-board luck), not a uniform 9.

This is the one real upward lever found. It is shipped (agent prompt) and is the right place
to keep pushing for MCU/digital boards.

## ★ The ceiling-breaker: let the LLM do the GEOMETRY (VLM floorplan via coordinate overlay)

The "unreachable" conclusion is about the ENGINE's placement ALGORITHM (sprawl-capped). It
does NOT apply to a vision LLM doing the placement. A VLM can do the global, semantic spatial
reasoning the algorithm lacks — "the LDO is marooned bottom-right, move it next to the MCU" —
which is exactly what de-sprawls a board. The enabler is a COORDINATE OVERLAY so the model can
reference and specify positions.

Working flow (proven):
1. Render the board; overlay a labelled (col,row) grid — `tools/coord_overlay.py IN OUT C R`.
2. A sub-agent (vision) reads the gridded render and returns a compact signal-flow floorplan
   as `{refdes: [col,row]}` for the major parts (power-in left → IC(s) centre → peripherals
   right, connected parts adjacent, tight span).
3. Apply it as the per-block `layout:` grid (cells map directly) and re-compile.

Proof: sprawled c01 (STM32 board) → VLM floorplan `{U1:[3,2],J1:[1,2],U2:[2,2],J2:[4,2],
JP1:[4,1]}` → re-renders as a clean J1→U2→U1→J2 left-to-right chain (2 crossings, 0 warnings)
vs the original marooned-LDO sprawl (independent reviewers 3-5). KEY: a NAIVE authored grid had
HURT (c01_banded=5) — the VLM's *intelligent* floorplan is what makes the authored-grid path
win. On an already-tidy board (c11) it's neutral; the lever helps most where sprawl is worst.

This is the genuine path through the ceiling. REMAINING WORK:
- **Automate the loop** inside the agent (or a dedicated sub-agent): after `apply_design`,
  render+overlay, call the VLM placer, re-apply the `layout:` grid, optionally iterate against
  the critic. This is the "sub-agent handles the complicated scenarios" pattern.
- **Satellites**: the VLM places ANCHORS; decoupling/indicator satellites still auto-place and
  can still scatter. Extend the floorplan to satellite GROUPS, or improve their clustering once
  the anchor frame is fixed.
- **Validate scores**: the critic gateway was 401 (credits) during the proof, so c01 was judged
  visually + on objective crossings/warnings; re-run `schematic_critic.py --samples 3` to confirm
  the expected 5→7-8 lift when the gateway is back.

## What does NOT move the critic (do not re-explore)

- **Compaction at the engine level** — the only thing that would move the sprawl cap, but
  unachievable cleanly. Four methods all fail: SA + bounding-box penalty, block-gravity,
  force-directed anchor placement, force-directed BLOCK placement. ROOT: packing modules
  tighter collides their SATELLITE FANS (decoupling/taps/indicators) → decongest re-expands
  + text collisions, OR wires reroute through IC bodies. A dense schematic's inter-module
  space is genuinely *needed*; sprawl is partly necessary.
- **Cheap router-free crossing proxies** (bbox-overlap area, signal-only bbox, trunk-segment
  intersection) — all made crossings WORSE; no cheap proxy is faithful because real
  crossings depend on multi-pin Manhattan routing topology.
- **Engine block-bands** — forcing per-block column bands scored WORSE than letting the
  engine's connectivity placement handle modular blocks (c01 banded 5 vs blocks-no-grid 6).
  The modular-block benefit is the component ORDER feeding `order_anchors`, not spatial bands.
- **Per-topic engine fixes for critic gain** — e.g. multi-unit op-amp clustering (sibling
  cohesion + per-unit offset seed + refdes-aware bypass) cut op-amp body crossings 24→15
  but did NOT move the analog critic: a fresh op-amp board still scored 5, capped by the
  same universal sprawl. Real, safe quality wins, but critic-neutral.
- **Critic recalibration for higher scores** — the critic is already fair/generous; making
  it accurate would LOWER scores. The one legitimate gain shipped is `--samples N` (median
  of N) to cut its ±1–2 run-to-run noise — use it for any gating/A-B.

## Real wins shipped this sweep (engine quality, validated)

Distributed-power rail inference + distributed local power symbols; stranded-cap seating;
duplicate-power-label merge; text-gap (kills "10kGND"); connectivity-aware anchor ordering;
wire-crossing metric; **route-aware refinement** (true-router polish of the fast-lane winner
— real IC-body-crossing reductions: c20 38→14, bga 110→95, costs the ≤5s budget); multi-unit
cohesion/offset (analog ic 24→15). Plus the agent-modular prompt. All gate on: byte-identical
reference snapshots, both netlist oracles truthful (`LAYOUT_SEARCH=anneal floorplan_netlist`),
ERC-clean. Two research crates exist as foundations: `crossmin` (layered — wrong model for
schematics) and `forceplace` (force-directed — right model, but compaction collides fans).

## If a future session wants 9+ (all options are multi-session, payoff uncertain)

1. **Co-placement** of anchors AND their satellites/labels as one compact unit (not anchors
   then satellite-tapping) — the only thing that could compact without colliding fans. Even
   then, satellite room caps the tightness; may not reach 9 on dense boards.
2. **Multi-unit support** (analog): key `place` by `(refdes, unit)` so per-unit seeds are
   independent + correct; add a dual-supply (VPLUS/VMINUS) decoupling matcher in
   `circuit-graph`. NOTE: confirmed this would NOT move the analog *critic* (sprawl-capped),
   only the objective ic/organisation — pursue only for objective quality, not the score.
3. **Critic-driven agent refine loop** — generate→critic→revise block structure→re-render.
   Limited leverage: the agent controls netlist/blocks/ports, NOT placement (the cap).
4. **Accept the validated state.** The engine is at its critic-ceiling; the agent-modular
   path is the shipped win for MCU/digital boards.
