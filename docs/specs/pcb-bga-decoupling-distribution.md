# BGA decoupling-cap distribution (the #1 placement-quality frontier)

## Problem

On boards with a large IC and MANY decoupling caps (a BGA + 16+ bypass caps), the
caps **scatter to the board edges** instead of ringing the IC. This (a) reads as
unprofessional, (b) wastes board area, and (c) blocks the mounting-hole corners
(the corner post-pass then can't seat them). Confirmed on `bga169-scale` and
`mcu-bga-system` (caps mean ~19 mm from a 9×9 mm BGA centre; a ring would be ~5–8 mm).

## Why the obvious fixes don't work (measured, Jun 2026)

The placement SA cohesion (`SA_COHERE_W`) pulls each cap to its nearest anchor power
pad. Two attempted reframes were **tried and reverted** — both failed for a
*structural* reason, not a tuning one:

1. **Distinct-pad assignment** (`decoupling_assignments`: assign each cap a power pad
   spread by angle so they ring instead of piling). Alone it does nothing: the SA
   relocate move is a small perturbation, and a plane-net cap (VCC/GND) has **no net
   spring** in the force-directed seed, so it starts wherever it floats and the
   perturbation-limited anneal can't drag it across the board to the IC.

2. **Seed decouple springs + anneal** (a combined variant so the springs pull caps
   close, then the anneal rings them). Also no change — and here is the real blocker:

### The structural blocker — placement fights the routing model

Decoupling caps connect **only through the plane stitching vias** (VCC pad → VCC
plane via, GND pad → GND plane via). When caps cluster *near* the BGA, their stitching
vias **crowd the BGA's own power vias** and get skipped (the connectivity-honest
`route_with_planes` drops a via that can't clear its neighbours). More skipped vias =
more honest-unconnected = **more "faults"** in the `place_best` ranking key
`(faults, layout_cost, hpwl)`. So the faults-primary selector *prefers the scattered
layout* — the scattered caps' vias have room, so they connect, so that layout has
fewer faults and wins. Clustering is penalised exactly because it's congested.

**Cap-distribution is therefore a joint placement+routing problem, not a placement
tweak.** (Contrast the mounting-hole corner-seek, which WAS a clean placement
post-pass: a mounting hole has no signal net, so moving it can't change routing.)

## The real fix (future work)

Route near-IC decoupling caps **directly to the IC's power pads/balls** with short
traces, instead of relying on a plane stitching via at the cap. Then placing a cap
adjacent to the IC *reduces* its connection length and does NOT add via congestion —
so clustering stops increasing the fault count, and the placement is free to ring the
caps. Sketch:

- In `route_with_planes`, before adding a plane stitching via for a cap pad, try a
  short direct trace from the cap pad to the nearest same-net IC pad/ball that routes
  cleanly; only fall back to a stitching via if no short direct route exists.
- Once clustering no longer costs faults, the distinct-pad assignment + a directed
  "snap cap toward its assigned IC-edge slot" SA move (analogous to the mounting-hole
  corner snap) will pull the ring together.

Until then the engine is **correct** on these boards (0 DRC faults, power on planes) —
this is purely a layout-aesthetics frontier, and the caps DO hug the IC on the common
1–2-cap case (the SA cohesion wins when caps don't crowd).
