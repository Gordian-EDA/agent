# BGA escape routing (inner-ball signal escape)

## RESOLVED (Jun 2026) — the corpus blocker was a BOUNDS-CLAMP bug, not escape

The dense 6/8-layer BGA boards (bga64-8l/8layer/stress, 37 failed each) were diagnosed —
here and in `pcb-engine-routing-gap.md` — as "inner balls enclosed on F/B, need an inner
signal layer." Measuring it proved that diagnosis a **bounds-clamping artifact**: the
fan-out placer expands the working frame to fit the parts (bga64 → ~91 mm), but
`to_route_problem` kept the board's *declared* 46 mm bounds, so every pad past 46 mm was
**clamped onto the grid edge** and piled up unroutable. The field only *looked* enclosed
because it was crushed against the grid wall.

Fix (`route_board`, `fit_bounds_to_obstacles`): grow the routing bounds per-axis to
enclose any obstacle the placer put outside the declared frame (a strict no-op for a board
already inside its bounds, so no in-bounds board's grid shifts). **Corpus: 617 → 432
failed nets, zero regressions, `copper_errors = 0`.** bga64 37 → 2; tqfp64-6l 40 → 12;
bga64-fattrace 39 → 13; bga64-bigvia 39 → 17. With the clamp gone, F.Cu/B.Cu suffice for
these (depopulated) fields — the inner-layer escape is *not* what fixes them.

## The structured inner-layer escape (built, kept, exercised by `bga49-dense-escape-8l`)

The inner-layer escape is still the right lever for a TRUE dense full array — it just had
no such board in the corpus (every other dense BGA here is 4-layer, whose inner pair are
GND/VCC planes with no signal layer to escape onto). Implemented and proven on the new
`bga49-dense-escape-8l` vehicle (full 7×7 0.8 mm, inner 5×5 all signals, 8-layer):
`7% → 33%` complete, `copper_errors = 0`.

- `RouteProblem.escape_layers` (net → assigned inner signal layer), set by the agent's
  `assign_inner_escape` for a dense fine-pitch ball field (`pitch ≥ 0.75 mm` so a 0.6 mm
  via-in-pad clears its neighbour ball; `(col + 2·row) mod n_inner` colouring so no two
  adjacent balls share a layer — adjacent escaped barrels one pitch apart leave only a
  0.2 mm gap, unroutable on one layer).
- The grid router (`router::via_in_pad_escape`) drops a through-via ON an enclosed ball's
  pad (same net → clears its own copper; the pitch gate guarantees the neighbour balls)
  and seeds the routed tree on the assigned inner layer; `AStarCosts.layer_mask` restricts
  the net's search to `{top, bottom, escape}` (a 3-layer problem — a free all-layer maze
  self-blocks on the via field and blows up runtime, measured 37→39 / 60 s).
- `route_with_planes` routes BOTH with and without the escape and keeps the one that
  connects more real nets, so the escape is a strict capability ADD: it can only help.
- Known ceiling: at 0.8 mm only ~1 trace fits between barrels, so each inner layer drains
  ~2 rings; the deep interior of a large array still won't fully escape (33% on the 7×7).
  The next lever is an angular-corridor escape (per `docs/specs/bga-hdi-escape.md`), not a
  fault.

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

## Root cause, refined (Jun 18 — measured)

Two stacked blockers, found by instrumenting `bga25-route` (`reason = "no grid path
from point 1 … (enclosure)"`):

1. **DONE — signals routed onto planes.** The A* searched `0..layer_count` and took
   the cheapest hop F→In1 onto the GND plane; that copper shorted and was dropped, so
   no signal escaped. Fixed: `AStarCosts.plane_mask` + `plane_mask_for(layer_count)`
   (4-layer ⇒ inner layers are planes); the via step skips plane layers, so escape goes
   F→B (through-via). Safe: 32 boards stay 0-fault.

2. **OPEN — B.Cu enclosure by the power-via ring.** With (1), an inner ball must escape
   to B.Cu, but `route_auto` routes the outer power balls first (16 stitching vias), and
   their B-side pads + clearance halos form a ring that **encloses** the inner balls'
   B-escape. Even the lenient (no via-scan) variant fails: the via seats but the radial
   B route can't thread out past the via ring. This is a **fanout/ordering** problem.

   Levers to try next (gated on the 32-board fidelity): route signal escapes BEFORE
   power stitching (or co-plan them); a dedicated radial escape pass that assigns each
   ring an outward corridor; thinner escape traces in the pad field; and verify the
   through-via anti-pads in In1/In2 once a signal actually reaches B.

3. **REFINED with `escape-min` (Jun 18).** A *minimal* case — ONE center ball as the
   only signal, surrounded by no-connect obstacle balls, 2-layer (no planes, no
   congestion) — STILL fails. So escape is not congestion/ordering; it's the maze
   router itself on an enclosed pad. Instrumented chain:
   - The A* *does* find a path (`path=true`) but reconcile drops it → `Unconnected`.
   - `emit_path` emits the via at layer changes; the connectivity oracle treats a via
     on a pad as connected — so neither of those is the gap.
   - A smaller via (0.45/0.25) does **not** help → not via clearance.
   - **Leading hypothesis:** the A* takes the *cheaper* F-only path that threads the
     sub-clearance channel between balls (1.27 mm pitch − 0.75 mm pad = 0.52 mm gap vs
     ~0.65 mm needed for trace+2×clearance) instead of paying the via cost (25); that
     illegal squeeze then gets dropped by the geometry lint as unconnected, and the
     via-escape alternative is never explored because a "path" already succeeded.
   - **Fix direction:** the grid's obstacle halo must mark sub-clearance channels as
     blocked (so the only legal path is the via-escape), via `grid::obstacle_inflation`
     — but that is global and must be gated on all 32 boards; OR a dedicated escape pass
     that bypasses the maze router for enclosed pads. `escape-min` is the minimal metric.

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
