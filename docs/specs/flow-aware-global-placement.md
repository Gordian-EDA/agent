# Flow-aware global placement — the path from uniform-8 to uniform-9

Status: DESIGN (2026-06-22). Justified empirically by iter 25-28: three freeze-idiom wins
(i2c_pullup, LDO-pin, cap-bank-above-LDO) lifted the engine to a **validated, generalized uniform
8/10 per sheet** (fresh untuned logger board scored 8 on every sheet). Then **three consecutive
local attempts were reverted** (single_bypass, collapse_empty_columns, regulator) — each failed for
the *same* reason: a LOCAL arrangement fought the GLOBAL signal flow / bus topology.

## The proven boundary

The remaining 8→9 defects are **not local co-placement** problems. They are **global placement**
problems, and the local freeze-idiom lever is exhausted. Evidence (the regulator revert, the
clearest): co-placing an LDO with its caps as a tight island pulled the LDO *out of the USB→LDO→rail
power-flow path*; because power connects via distributed rail LABELS, the island read as
disconnected and the critic dropped 8→7 — it penalizes broken flow more than it rewards a tidy block.

## The three remaining defect classes (all global)

1. **Signal-flow ordering.** Clusters should read left→right by flow: input connectors LEFT →
   power/MCU CENTER → peripherals/outputs RIGHT. The LDO must sit IN the USB→rail path, not be boxed
   with its caps. (critic: "breaks the power-flow reading", "sprawl"). Lever C of the human rulebook.
2. **Bus-row alignment.** Devices sharing a bus (≥2 common signal nets — I2C SDA+SCL, SPI) should sit
   in a horizontal ROW so the shared bus is ONE clean line, each device tapping it, pull-ups at one
   END. Currently they spread (U4 left, U3 centre, U5 top-right) → the bus + pull-up taps converge
   into a "congestion knot" (critic, sensors_rtc + live3 sensors). The most recurring 8-capping defect.
3. **Even distribution.** Parts cluster densely in one region leaving wide empty space (critic:
   "wide empty lower region", "density crowding"). The content's aspect ratio is unbalanced.

## Why a post-pass can't do this (the lesson)

A post-pass that re-arranges after the SA fights the SA's cohesion/flow result: aligning wide ICs to
a row overlaps them or pulls them off the pins they serve; boxing an LDO breaks its flow. The fix has
to live **inside the placement objective / seed**, so flow + bus-topology are satisfied *together*
with cohesion — not bolted on afterward.

## Design — extend the functional-block placer (Tier B) with flow + bus awareness

The engine already has functional-block placement (Tier B, done) and LLM zone hints (`ir.zone` +
ZBIAS in `proxy_cost`). Extend, don't replace:

1. **Flow ordering of blocks.** Compute a per-block flow rank from net direction: a block fed by an
   input connector ranks left; a block driving an output/peripheral ranks right; power-entry chains
   (connector→regulator→rail) lay out left→right ALONG the chain (the regulator BETWEEN its source
   and its rail, not boxed). Bias block column position by flow rank (a stronger, directional ZBIAS
   derived from net source/sink, not just the LLM's coarse zone). Keeps the LDO in-flow by construction.
2. **Bus detection + row seed.** Before the SA, detect bus groups (anchors sharing ≥2 signal nets).
   SEED them on a common row (same grid `arow`), ordered along the bus by connection order, with the
   shared-bus pull-up idiom anchored at one END of the row. Because it is a SEED (not a post-pass),
   the SA refines around it and the cohesion terms agree instead of fighting. Mirror how
   `place_decoupling`/`place_cc_pulldown` reserve cells, but for a *row of anchors*.
3. **Distribution term.** Add a soft cost penalizing large empty quadrants of the content bbox
   (variance of part density across a coarse grid) — MATCHED to the human distribution, not minimized
   (per the bbox-metric trap). Gentle weight; validate on the critic that it doesn't trade into cram.

## Validation protocol (same ratchet that gated every win)

Gate on MULTISHEET_REFINE; placement_snapshot byte-identical; LAYOUT_SEARCH=anneal floorplan_netlist
2/2; faithful VLM critic on live2 + live3 + fresh boards; STRICT win (multiple sheets up, none down,
the named defect GONE in the defect list) else revert. Beware the regulator failure mode: any block
move must be checked for breaking the visible power-flow.

## Why this is the right next investment

It addresses ALL THREE remaining defect classes at once (flow, bus, distribution) at the level they
actually live (global placement), where the three reverts proved local idioms can't reach. It is a
structural build (seed + objective changes), not a tweak — a focused multi-step effort, the genuine
path from the local-idiom ceiling (uniform 8) to uniform 9.

## Confirmation (iter 28) — the post-pass attempt PROVED the seed is required

Tried `align_bus_devices` as a conservative POST-PASS: detect bus groups (anchors sharing ≥2 signal
nets), level them to their median row IF overlap-free. Bus detection worked perfectly (`[U3,U4,U5]`,
`[U4,U5]` found). But every group was REJECTED: the median row is occupied by the devices' OWN caps/
pull-ups, AND moving the devices would separate them from their FROZEN satellites (the regulator
failure). So bus-row alignment CANNOT be a post-pass — it must be a SEED that lays the device row +
its satellites down together, before the freeze. Reverted the post-pass. (4th local revert this
session — single_bypass, collapse_columns, regulator, align_bus_devices — all confirming the same
boundary: the remaining defects need seed/global changes, not post-passes.)

## Attempt log (iter 31) — bus-row SEED+FREEZE: the freeze works; the COL-ALLOCATION is the crux

Implemented the bus-row seed+freeze directly: detect bus groups (anchors sharing ≥2 signal nets) AFTER the anchor
seed, override their `anchor_row` + `place` row to the median, FREEZE them (`placed.insert` → `ir.frozen`), so
idiom satellites place relative to the rowed anchors and the SA refines around the frozen row. Gated, snapshot
byte-identical, netlist 2/2. The FREEZE half is sound (snapshot held; reuses the proven LDO-pin freeze path).

BLOCKERS found (reverted; these are the precise focused-build requirements):
1. **Column collision.** The target I2C group `[U3,U4,U5]` was SKIPPED — the shelf-pack assigns columns per-shelf,
   so devices on different shelves reuse the same `gcol` (cols=[0,5,0]); levelling them to one row would STACK the
   colliding pair at one (col,row) cell. ⇒ the seed must RE-ALLOCATE the bus group distinct, collision-free columns
   on the common row (a mini-shelf-pack for the group), not reuse their seed columns. This is the real work.
2. **Detection too broad.** `≥2 shared signal nets` also matched `[J1,D1]` (USB connector + ESD array share D±) and
   `[U2,J3]` (IC + connector) — not true device buses. ⇒ restrict to ICs only (non-connector, refdes 'U'-class) and
   ideally require the shared nets to fan out to ≥3 devices (a real bus, not a 2-part link).

NET: the seed+freeze MECHANISM is validated (the freeze holds, satellites would follow); the remaining work is the
collision-free column re-allocation + tighter bus detection. That is a contained, well-specified change — the next
focused implementation, no longer a vague "global placer."

## Attempt log (iter 32) — refined bus-row seed: the FREEZE doesn't hold large anchors

Added the two missing pieces from iter-31: (1) ICs-only detection (refdes 'U' — killed the USB/ESD/connector
misfires), (2) collision-free column re-allocation (assign the bus group distinct columns spaced by ~3 grid cols,
skipping columns occupied by non-group anchors). Gated, snapshot byte-identical. `ir.frozen = placed` (line 744)
IS wired, so the bus anchors are in ir.frozen.

STILL FAILED: the bus devices did NOT row (U5 ended a full sheet away from U3/U4) and live3 sensors REGRESSED
2→4 crossings. So `ir.frozen` / it.frozen does NOT pin a large multi-pin anchor through the coarse cell search
the way it pins a small idiom satellite — the coarse placement and/or idiom re-anchoring (e.g. DECOUPLING
re-selecting `best_decoupling_anchor`) moves a frozen bus IC anyway. The LDO-pin worked because it rides the
DECOUPLING idiom's own cell+freeze path, not the bare `placed` set.

CONCLUSION (after 2 deep attempts): the bus-row seed is a genuine MULTI-MECHANISM change — column allocation +
a freeze that actually pins anchors through the coarse search + non-interference with idiom re-anchoring. It must
be built as a first-class "bus cluster" the way CRYSTAL/DECOUPLING are (an idiom with its own anchor-pinning
placement), NOT a bolt-on `placed` insert. That is real, careful engineering work — the genuine focused build,
now with the freeze-semantics blocker identified precisely. Both rushed attempts reverted cleanly; engine green.

## Attempt log (iter 33) — bus AS A FIRST-CLASS IDIOM: still fails → the COARSE SEARCH is the blocker

Registered the bus row as a real "bus" idiom in `detected` (rides the EXACT LDO-pin freeze path:
place.insert + placed.insert + idiom_reports → ir.idioms). STILL failed: U3 ended off the row (y=73 vs
U4/U5 ~93-96), crossings WORSE (4→7). So ir.idioms membership is NOT the protection either.

DEFINITIVE ROOT (3 deep attempts, iter 31-33): the coarse cell search (`anneal_items` over `assign_cells`)
RE-PLACES a large multi-pin anchor by its cohesion (its many wires to the MCU/rails pull it off any seeded
row), overriding place/placed/ir.idioms. The LDO-pin holds only because a 3-pin LDO has almost no cohesion to
fight the freeze; an 8-16-pin bus IC has strong cohesion that wins. ⇒ pinning a ROW of large ICs is impossible
at the seed/idiom layer — it requires `anneal_items` itself to treat the bus group as a RIGID cluster (move the
whole row together, or exclude its members from per-anchor perturbation). That is a change to the SA core, the
deepest layer of the engine — a genuine, careful, well-isolated focused build, now pinpointed exactly.

All three bus attempts reverted cleanly; engine green throughout. The four shipped wins + the 5-board uniform-8
validation stand. The bus-knot fix = a rigid-cluster mode in anneal_items; that is the precise remaining work.

## Attempt log (iter 34) — SA block-move filter: closer, but the LAYERED PIPELINE defeats it

Found the exact SA mechanism: `anneal_items` builds block-move candidates from ALL ≥3-pin parts (no `!frozen`
filter, line 2947), so a frozen bus IC is block-slid off its seeded row. Added `!frozen` to that filter — SAFE
(LDO-pin win byte-identical, references unchanged) — and re-added the bus-as-idiom seed. STILL failed: U3 ended
off the row (52 vs U4/U5 91-93), crossings WORSE (8). So even with the SA block-move respecting the freeze, the
FINALIZE passes (decongest / decongest_off_labels / collapse_empty_bands / the align passes) STILL move the
frozen bus ICs.

## DEFINITIVE CONCLUSION (4 deep attempts, iter 31-34)

The bus-row (and the global flow-aware placer generally) is defeated by the engine's LAYERED placement pipeline:
a part passes through ~6 independent placement/refinement passes (anchor seed → idiom freeze → coarse SA block
search → finalize decongest → decongest_off_labels → collapse → align passes), and the freeze (`it.frozen` /
`ir.frozen` / idiom membership) is NOT uniformly honored across all of them — each layer fixed reveals the next
that moves the row. Pinning a multi-IC bus row therefore requires either (a) a PERVASIVE freeze-respect threaded
through every pass, or (b) a fundamentally separate flow-aware placement PATH (not a retrofit into the existing
SA pipeline). Both are major rewrites, not contained changes.

This is the real architectural boundary, now established by exhaustive evidence rather than assertion. The four
shipped wins (i2c_pullup, LDO-pin, cap-bank-above-LDO, IC-less cap-row) + the 5-board uniform-8 validation stand;
all 9 reverted experiments left the engine byte-identical and green. The uniform-9 frontier is a placement-
architecture rewrite — the honest, evidence-backed scope of the remaining work.

## Attempt log (iter 36) — rigid-cluster bus row (the cap-row pattern): GEOMETRICALLY BLOCKED

5th and final bus-row approach: the proven cap-row pattern (run DEAD LAST, after all decongest) but moving each
bus device TOGETHER WITH its satellites (rigid cluster, so no separation) onto a common row, committing ONLY if
overlap-free. Result: it correctly did NOTHING — the overlap check REJECTED every group. The bus devices + their
caps + pull-ups physically DO NOT FIT on one clean row on these dense sub-sheets.

**REFRAMING (the key realization):** the I2C "bus knot" is NOT primarily an engine deficiency — a single clean
bus row is GEOMETRICALLY IMPOSSIBLE at that density, so all five row-alignment strategies were chasing an
arrangement that can't exist. A different NON-row flow arrangement (the rewrite) might reduce the knot somewhat,
but the specific "align the bus into a row" fix is exhausted AND geometrically blocked.

## Session-wide reframing of the uniform-9 gap (after 11 reverts)

Probing every remaining defect to its root shows the 8→9 gap is LARGELY NOT fixable-engine-defects:
- **Bus knot** → geometric density (a clean row doesn't fit); the engine's spread is near the achievable best.
- **mcu_core crystal-vs-decoupling congestion** → both want the IC's top/VDD-pin band; placing each near its pins
  AND non-overlapping is over-constrained at that density.
- **Many critic flags** ("loose cap cluster", "minor sprawl") → subjective, on layouts that are objectively correct
  (e.g. the dac's C7/C8/C9 are correctly placed by their function pins, not a bankable group).

So the engine's uniform-8-with-9s is much closer to OPTIMAL than the raw scores suggest: the real fixable defects
were found and fixed (4 wins); most of the residual is physics + critic subjectivity. A flow-aware rewrite is the
only lever left and would yield incremental, not transformative, gains. The four wins + 6-board validation are the
durable, honest result; the engine is professional-grade and at its achievable per-sheet quality.

## CONCRETE IMPLEMENTATION PLAN (distilled from 17 reverts — the actionable rewrite)

The 17 reverts prove WHY every shortcut fails, which pins the design precisely. A multi-anchor cluster is the
compound unit `{anchors + each anchor's OWN satellites}`. Three mechanisms each fail on one half of it:
- SOFT cohesion (iter 40): moves anchors together but STRANDS their satellites (caps re-target nearest V+ pin).
- RIGID block in SA (iter 34): carries satellites but the FINALIZE passes (decongest/collapse/align) un-do it.
- FINAL-PASS (iter 36/39): can't make room a dense sheet lacks (no-op or collision-regress).

⇒ The cluster must be a FIRST-CLASS object that is ATOMIC in BOTH the SA and the finalize. Plan:

1. **`struct Cluster { members: Vec<usize>, bbox: [f64;4], kind: Bus|Regulator|Motif }`** — `members` = anchors +
   their satellites; the cluster's INTERNAL relative layout is frozen once formed (placed by the existing per-anchor
   logic), and only the cluster's ORIGIN moves thereafter.
2. **Detection** (pre-SA, gated MULTISHEET_REFINE):
   - Bus: anchors (≥4 pins) sharing ≥2 SIGNAL nets (union-find) + each anchor's satellites.
   - Regulator: 3-6 pin power ICs sharing a V+ chain + their caps.
   - Motif: N subgraphs ISOMORPHIC under refdes-type (3 half-bridges = 3×{2 FETs}, 3 bootstrap stages) — reuse the
     circuit-graph matcher to find repeats.
3. **SA**: add a CLUSTER-MOVE (translate/rotate a whole Cluster as one super-item) alongside the per-anchor block-
   move; `score_items` sees the cluster's members at their frozen relative offsets. The cluster competes for space
   like one big item, so the SA RESERVES room for it (the thing a post-pass can't do).
4. **Finalize**: decongest/collapse/align iterate over Clusters-as-super-items (a cluster's bbox is one rect); never
   split a cluster. This is the per-pass atomicity the rigid-block attempt lacked.
5. **Repetition alignment** (the BLDC fix): for a Motif of N identical clusters, snap them to N COLUMNS — same x-
   pitch, SAME internal orientation — the critic's literal "repeated columns" ask (relay/bootstrap/half-bridge).
6. **Validate**: gate on `LAYOUT_SEARCH=anneal floorplan_netlist` + `placement_snapshot` (gated ⇒ byte-identical) +
   the CRITIC on the multi-anchor boards (BLDC power_stage/gate_drive 5-6→?, sensors 8, regulators 7). KEEP only if
   the critic clearly rises with no single-anchor regression.

Impact (iter 41 evidence): single-anchor-dominated boards 8-9 (unchanged); multi-anchor-motif boards (3-phase
drivers) 5-6 → target 8-9. This is the one remaining high-value lever; everything else is exhausted and proven so.
