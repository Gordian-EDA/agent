# Signal-label distribution (long signal nets → net labels, not long wires)

Status: **design** (2026-06-21). Engine: `crates/sch-layout/src/floorplan.rs`
(`wire`, `route_signal`, `rail_should_distribute`, `layout_cost`). Gated on the
netlist oracle (`LAYOUT_SEARCH=anneal floorplan_netlist`) + the VLM critic.

## Why

The #1 recurring critic defect on BOTH simple and dense circuits is a **long detour
wire** dragging a part's connection across the sheet (555 cross-sheet detour;
dense-STM32 `S1 RESET → NRST` long right-edge rail; `BOOT` L-path along the bottom).
The critic consistently *praises* the opposite — "power symbols used so global nets
need no drawn wires." **Professional engineers label long / global nets** instead of
drawing a wire across the sheet; short local nets stay wired.

Today the engine does the opposite: `route_signal` routes an MST of elbow edges and
emits a label ONLY when the router *fails* (`roots.len() > 1`, line ~5147). A label
is treated as a failure-fallback and `layout_cost` penalises `signal_label_count` at
**1000** (correctness-wall weight). So the engine draws a wire however long, and the
cost is biased exactly opposite to humans.

## The precedent (already in the engine, for RAILS)

`wire()` already distributes a *spread rail* into per-pin local power symbols instead
of one long spanning trunk:
```
let distribute = ir.rail_locals.contains(net)
    || (pin_total > FAST_PINS && rail_should_distribute(eps));   // span gate, board-size gated
```
`rail_should_distribute` = ≥3 eps AND bbox half-perimeter span > `RAIL_DISTRIBUTE_SPAN`.
This shipped as the fix for "scattered caps / congested rail knot / long detour rails."

## Design — the signal-net analog

For a SIGNAL net (non-rail, non-port-only) whose MST contains an edge far longer than
a local hop, **don't draw that edge**; leave its endpoints split so the existing
bridge logic (line ~5147) names each side with a net label. Net stays connected (same
local-label name → KiCAD nets them) → oracle green.

Two staged decisions:
1. **Edge-level (preferred):** in `route_signal`, skip an MST edge whose Manhattan
   length > `SIGNAL_LABEL_SPAN`; the split component is then label-bridged. Keeps
   short edges wired and only the long hop becomes a label pair (minimal label count).
2. **Net-level (simpler):** mirror `rail_should_distribute` — if a signal net's eps
   span > threshold, label every terminal (a la distribute). Risk: label soup on
   medium nets; prefer (1).

### Safety staging (critical)
- **Finalize-only.** Gate the policy on `fan_risers == true` (the existing finalize
  flag), exactly like riser fanning and body jogs. The per-move scorer passes
  `fan_risers=false`, so SA trajectory + every reference/snapshot stays byte-identical.
  Only the final shipped render swaps long wires for labels.
- **Board-size gate** `pin_total > FAST_PINS` (mirror the rail policy) so small tuned
  references (≤34 pins: divider/555/uart/mcp) are untouched. (Revisit: the 555 detour
  is a small board — may want the policy there too, behind its own smaller gate, once
  the dense case is proven.)
- **Cost interaction.** Because the policy is finalize-only, per-move cost is
  unchanged. The candidate *pick* re-asserts truth on the finalized writer; confirm
  the 1000 fallback weight doesn't flip the pick against the (now cleaner) labelled
  finalize. If it does, split `signal_label_count` into *forced* (route failed → keep
  1000) vs *policy* (deliberate long-net label → small tax ~3–5) so deliberate labels
  read as good. The forced/policy distinction is a flag set at emit time.

## Validation (hard gates)
- `LAYOUT_SEARCH=anneal cargo test --release -p sch-layout --test floorplan_netlist` — connectivity unbroken.
- VLM critic on dense battery (`dense-stm32`, …) — long-wire defects gone, no label-soup regression; aim 9+. Re-critic references for no regression.
- Free (greedy) path + snapshots bit-identical (finalize-only gating guarantees this).

## Open questions
- `SIGNAL_LABEL_SPAN` value (sweep): too small = label soup; too large = no effect.
- Should a net already carrying a meaningful name (NRST/SWDIO) be preferred for
  labelling over an auto-named node? (Humans label *named* nets; auto nodes stay wired.)
- Interaction with `route_local_tee` (clustered terminals → one trunk): only apply to
  the non-local, spread case.
