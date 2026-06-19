# Plane-stitch via fanout — promising lever, blocked on a drill-aware obstacle model

## The recurring failure it targets

On a multilayer board, a high-fanout power/ground net becomes a copper PLANE; each of its
pads is connected to the plane by a **stitch via dropped on the pad** (`route_with_planes`).
On a fine-pitch part (TSSOP-20 0.65mm, QFN-32 0.5mm, dense BGA) a standard via has **no room
between neighbours**, so the stitch is SKIPPED and that power pin stays honestly unrouted —
the single most common "honest failure" across the board suite (e.g. `tssop20-4layer` fails
exactly `<N plane stitching vias>`).

## The lever: a dog-bone fanout

Instead of skipping, route a short trace from the pad OUTWARD to the first nearby open point
where a via clears, and stitch THERE. Standard PCB escape technique. Prototyped
(`stitch_via_clears` + `fanout_seg_clears` + an 8-direction × 4-step search in
`route_with_planes`), and it WORKS where the obstacle model is complete:

- `tssop20-4layer`: 1 failed → **0 (fully routed)**
- `bga-system50`: 5 failed / 6 unconnected → **4 / 4**
- `multi-ic-system`: 18 → 17 unconnected

## Why it was reverted (the blocker)

It introduced **DRC faults on 4 fine-pitch boards** (`qfn-thermal`, `bga100-fine`,
`lqfp144`, `mcu-bga-system`):

- `hole_clearance` — the fanout via landed too close to **footprint-internal thru-features**
  (an EP's thermal vias, a thru-hole pad's barrel). The engine's obstacle set
  (`RouteProblem.obstacles`) carries each pad's **copper rect but NOT its drill**, and
  footprint thermal/stitch vias are not extracted as obstacles at all — so the fanout
  search is blind to those holes and cannot avoid them.
- `clearance` — a secondary gap: `fanout_seg_clears` checked the trace against foreign pads
  and tracks but not foreign **vias** (fixable, but the hole-clearance blind spot is the
  real blocker).

A DRC regression on 4 boards to fix 2 is a net loss, and the fidelity gate (0 copper faults)
is non-negotiable — so reverted.

## Prerequisite before re-attempting

A **drill-aware obstacle model**: every obstacle carries its drill diameter (0 for SMD), and
footprint-internal thru-features (thermal-via arrays, mounting/thru pads) are extracted as
obstacles. Then a stitch/fanout via can be checked for hole-to-hole clearance against real
holes, and the fanout becomes shippable. This also benefits the via router generally (it
currently reasons about copper, not holes). Scoped, deferred — a real next lever once the
obstacle model carries drills.
