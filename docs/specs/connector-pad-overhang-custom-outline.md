# Connector pad overhang on a custom (non-rect) outline (found Jun 19, integration test)

## The finding

A composition test — exercising THIS session's fixes together (custom outline + keep-out +
4-layer planes + 35 parts) — found a real fidelity bug the individual feature tests missed. A
BGA-64 + 30 caps + **4 edge-seeking 1×4 pin headers** on a **hexagonal** outline routes/exports
with KiCAD reporting **2 `copper_edge_clearance` faults**: `PTH pad 3/4 of J1` sits at the hex's
right vertex (x = 87.0 = centre 45 + R 42), <0.5 mm from the Edge.Cuts.

## Root cause — `is_legal` checks the part CENTRE, not the placed-pad copper

`pcb_engine::placement::is_legal` (placement.rs ~1504) tests `point_in_polygon(&pos[i], outline)`
— the part's CENTRE — against the outline. The comment is deliberate: a mounting hole's
*courtyard* may overhang a notch while its copper stays inside, so it checks the centre, "trusting
the routing grid to hold copper to the outline." But that trust is wrong for **placed pads**: a
connector's pads are copper that the router never places, and a 1×4 header's pads reach ~3.8 mm
from its centre. So J1's centre is comfortably inside the hex while pad 4 overhangs the edge —
`is_legal` passes it, and the fault ships.

Why an inset alone can't fix it (tried + reverted): I added an `inset_polygon` that shrinks the
place/route outline by the 0.5 mm edge clearance. But the centre-check + a FIXED inset can't
absorb a PER-PART pad reach that varies from ~0.1 mm (an 0402) to ~3.8 mm (a 1×4 header) to more
(a 2×N header). The connector's centre stayed inside the inset hex while its far pad still
overhung. Reverted as the wrong tool.

## Not caught earlier because it needs the COMPOSITION

The rect-outline edge bug (fixed last round via the bbox `routing_bounds` inset) and the
keep-out / plane / scale features each passed alone. This needs *custom non-rect outline* +
*edge-seeking connector with extended pads* together — exactly what an integration test exercises
and a single-feature test doesn't. (The committed custom-outline boards are sparse or use small
parts, so none manifests it.)

## The fix (deliberate — fragile placement engine, deferred)

Check the part's **rotated pad (copper) bounding box**, not its centre, against the outline:
- Thread the part rotation into `is_legal` (currently only the rotated courtyard half-extent
  `half[i]` is available; the raw pad offsets need the angle to rotate).
- For each part, require its rotated pad-bbox corners inside the outline (copper inside), while
  STILL allowing the courtyard to overhang (preserve the mounting-hole-in-notch allowance the
  current centre-check protects). A rotation-INVARIANT circular `copper_radius` is too
  conservative — it false-rejects an edge-PARALLEL connector whose copper is actually clear.
- The placer/legalizer must then actually place connectors copper-inside (edge-seek toward the
  inset outline, legalizer clamp to it) so the stricter `is_legal` is satisfiable rather than
  just turning the board `place=false`.

This touches the force-layout + legalizer, which the project memory + CLAUDE.md flag as fragile (a
global change rebalances every board), so it is a deliberate change gated on the full harness — not
a rushed edit. Until then the engine stays honest: the fault surfaces in the export DRC
(`copper_edge_clearance`), so the agent sees it and can give more edge margin or move the
connector. NOTE: no committed board manifests it, so no guard is added (a guard would have to ship
the fault, which the harness forbids).
