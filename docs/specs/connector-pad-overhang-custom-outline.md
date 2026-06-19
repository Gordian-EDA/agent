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

## FIDELITY HALF FIXED (commit pending) — is_legal now checks the copper extent

DONE: `is_legal` now requires each part's ROTATED PAD (copper) bounding box, grown by the 0.5mm
edge clearance, to be inside the outline — not just its centre (a new `rotated_copper_half` is
precomputed alongside the courtyard half). A connector whose pad overhangs is now an ILLEGAL
placement, so the engine reports place=false (honest) instead of shipping a copper_edge_clearance
fault. The COURTYARD may still overhang (only copper is checked), preserving the mounting-hole
allowance. Verified: full harness 0 copper faults / 68 boards, NO regression (every existing
custom-outline board stays place=true — their copper was already clear); unit test
`is_legal_rejects_pad_overhang_on_custom_outline` guards the check. The contract now holds for
this class: place-illegal, never a shipped fault.

## QUALITY HALF REMAINING (deferred) — seat the connector INSIDE so it routes

With the fidelity half done, an edge-seeking connector whose copper would overhang now makes the
board `place=false` (honest) rather than shipping a fault. The remaining work is to let the placer
actually SEAT such a connector copper-inside so the board routes instead of failing:

- The connector edge-seek targets the nearest **bbox** edge; on a custom outline it should target
  the outline edge inset by the edge clearance (and the legalizer clamp to it), so the placer
  produces a copper-inside placement the (already-stricter) `is_legal` accepts.

This touches the force-layout + legalizer, which the project memory + CLAUDE.md flag as fragile (a
global change rebalances every board), so it is a deliberate change gated on the full harness — not
a rushed edit. Until then the engine stays honest in BOTH ways: it rejects the placement
(`place=false`), and were a fault ever to slip through, the export DRC still reports
`copper_edge_clearance`. No committed board manifests it, so no harness guard is added (a guard
would have to ship the fault or place-fail); the unit test
`is_legal_rejects_pad_overhang_on_custom_outline` guards the check.
