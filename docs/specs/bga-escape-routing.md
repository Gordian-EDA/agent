# BGA escape routing (inner-ball signal escape)

## The finding (reframe)

"BGA signals don't route" has been attributed to the **microvia / fine-pitch limit**.
A controlled diagnostic shows that is only half the story. `bga25-route` —
`BGA-25_6.35x6.35mm_Layout5x5_P1.27mm` with the **inner 3×3 balls as signals** going to
an edge header, outer ring as power planes, 4-layer — fails **all 9 inner signals**
(`traces=0, vias=16`; the 16 vias are outer-ring power-to-plane stitching only). The
render shows the inner 3×3 as bare pads: not one escape was attempted.

Yet at **1.27 mm pitch the escape is geometrically possible** with ordinary through-vias:
a via at the diagonal centre of four balls sits `1.27/√2 − pad_r ≈ 0.7 mm` from each pad
edge, comfortably clearing `via_r + clearance ≈ 0.5 mm`. (At 1.0 mm it is borderline;
at ≤0.8 mm it genuinely needs microvias — that remains a separate frontier.)

**Conclusion:** the engine has **no BGA inner-ball escape routing at all**. The naive
router is single-layer (F.Cu) so a surrounded ball is simply blocked; the detailed
router does not generate a dog-bone escape (pad → short F.Cu stub → via → inner/bottom
layer → route out). This is the #1 thing for "BGA 50+ perfect", and it is more bounded
than microvias: solvable with through-vias for pitch ≥ 1.0 mm.

## Structure to exploit

A BGA escape is **local and regular**: each ball escapes to the nearest *free* routing
channel, drops a layer, and routes out radially. The outer 1–2 rings escape on the top
layer with no via; each inner ring needs a via to a layer with a clear radial path. So
the cost/search should be **per-ball-local** (which channel, which layer) feeding a
**radial global** route out of the ball field — not a flat whole-board maze per net.

## Design (incremental — one verified step per loop)

1. **Plane-aware routable layers.** Signal routing must use only signal layers
   (F.Cu, B.Cu on a 4-layer board); In1/In2 are planes. Confirm the detailed router
   excludes plane layers from signal search (today it iterates `0..layer_count`).
   *Gate:* the 31 existing boards stay 0-fault.

2. **Fine grid / via-fit near pad fields.** The routing grid near a BGA must resolve the
   diagonal channel so a via can be placed between balls (cell ≤ ~⅓ pitch). Verify a via
   can legally seat in a 1.27 mm channel.

3. **Dog-bone escape generation.** For each signal pad that is blocked on its own layer
   (interior of a pad field), emit a dog-bone: a short stub to the nearest free diagonal
   channel + a via to the next free signal layer; register the via's far-layer cell as
   the net's new routing terminal. *Gate:* `bga25-route` inner signals route; 0 faults.

4. **Radial layer assignment.** Assign escape layers ring-by-ring (outer→top, next→
   bottom, deeper→inner signal layers when a 6-layer stack exists) to avoid B.Cu
   congestion. *Gate:* a denser routable BGA (e.g. BGA-100 P1.0mm) routes a high fraction.

5. **Capacity honesty.** When a ball cannot escape (channels/layers exhausted, or pitch
   too fine for a through-via), leave it honestly unrouted — never a short. `route_auto`
   already drops unconnected copper; the escape pass must respect the same oracle.

## Non-goals here

- Microvias / HDI stacks (≤0.8 mm pitch full escape) — separate spec; needs inner signal
  layers (6-layer) + blind/buried via model.
- Changing the schematic engine.

## Test vehicle

`crates/agent/examples/pcb_circuits/bga25-route.json` (this is the reproducing case;
inner 3×3 signals). Success = its 9 inner signals route, 0 DRC faults, and the full
`board_harness` stays fidelity-clean.
