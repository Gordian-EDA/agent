# PCB locality-aware placement (cluster anneal)

Status: **design** (2026-06-18). Engine: `crates/pcb-engine/src/placement.rs`
(`place_best`, `place_variant`, `force_layout`, `legalize`). Gate: `place_best`
variant selection (faults primary), the `board_harness` fidelity gate
(`copper_errors == 0`), and `tools/pcb_critic.py`.

## Why

PCB placement is the same **local + global** mix the schematic side exploits
(see [`locality-aware-placement-search.md`](./locality-aware-placement-search.md)).
Connectivity induces a hierarchy:

- A **cluster** = a dense anchor (BGA / QFP / regulator) + its **decoupling caps**
  (`decoupling_pairs`: a 2-pad part whose both nets are on the anchor) + its
  **series elements** (an R/L/cap between an anchor signal pin and a downstream
  connector pin). Its power nets are now PLANES (so intra-cluster power coupling
  is via stitching vias, near-free) — what remains local is the cap↔ball proximity
  and the signal escape to the cluster boundary.
- Clusters join other clusters / connectors by a **few boundary nets** (the escaped
  signals). Connectors `edge_seek` to the board edge.

The current force placer + legalizer ignores this hierarchy: caps and series
resistors are independent point masses pulled by net springs, so a many-cap anchor
gets a scattered cloud, not a hugging halo.

## What was tried and why it's not enough (Jun 18 loop — all reverted)

On `bga64-stress` (BGA + 32 decoupling caps + 32 series resistors):

1. **Decoupling-proximity term in `place_best` cost** — selected the spring variant; critic 5→4.
2. **Per-pad-target decouple spring** (pull each cap to a distinct power ball) — too weak vs other forces; caps stayed ~12 mm out.
3. **Deterministic halo snap** (ring caps around the anchor after force) — **improved routing** (bga unconn 11→8, tqfp64 33→28, 0 fidelity regression) because it cleared caps off the escape channels, BUT the visual regressed (critic→3): the anchor sat near an edge so the ring clamped, and the **32 series resistors compete for the same anchor-adjacent ring**, so the legalizer spread the caps back out.
4. **Anchor-centering clamp** — lost the routing win (8→10); caps still ~11 mm.

**Lesson:** each per-part heuristic fixes one metric and breaks another, because the
anchor + caps + resistors are **one cluster that must be placed jointly**. The halo's
routing win also shows the cost must value **channel-clearing**, not just proximity.

## The key property: a cluster moves as a unit

Once power is on planes, a cluster's internal cost (cap↔ball proximity, cap/resistor
vs anchor-courtyard overlap, escape-channel occupancy) is **invariant under a rigid
translation of the whole cluster**. So:

- An **intra-cluster** move (swap two cap ring slots; nudge a series R toward its
  header side) re-costs only that cluster → O(cluster).
- An **inter-cluster** move (slide/swap a whole cluster) re-costs only the boundary
  (escaped) nets and the destination overlap → O(boundary).

Neither is O(board) — the same unlock as the schematic anneal.

## Design

### 1. Cluster extraction
- Anchors = parts with ≥ N pins (the plane-net fan-out parts) or any part that is a
  `decoupling_pairs` anchor. Assign each decoupling cap to its anchor; assign each
  series 2-pad part to the anchor it shares a signal net with (its other net goes to
  a connector — that's the escape direction).

### 2. Deterministic cluster seed (replaces the bare halo)
For each cluster, lay it out **locally** in its own frame, then place the frame:
- Anchor at the cluster origin.
- **Decoupling caps**: concentric rings hugging the anchor courtyard, each cap on the
  side of the power ball it bypasses (use the assigned pad offset's quadrant), pitch =
  cap footprint + clearance so the ring is overlap-free (the legalizer must only nudge).
- **Series elements**: placed OUTSIDE the cap rings, on the **azimuth of their escape
  net** (toward the connector that net reaches) — this keeps the cap ring intact and
  pre-aligns the escape, the channel-clearing win from experiment 3 without the
  resistor/cap collision.
- Place the whole frame so it fits in bounds (anchor centred enough that the outer ring
  is on-board), then `legalize`.

### 3. Cluster anneal (two-level), temperature-coupled
| Phase | Moves | Explores |
|-------|-------|----------|
| Hot | slide/swap whole clusters; rotate a cluster 90° | global arrangement + board aspect |
| Cool | swap cap ring slots; move a series element along its escape azimuth; jitter the anchor | intra-cluster polish |

Cost = `route_faults` (hard, primary) `+ w1·escape_wirelength + w2·decoupling_penalty
+ w3·channel_occupancy + w4·overlap`. `decoupling_penalty` already exists
(distance cap→nearest anchor power pad); `channel_occupancy` = count of non-anchor
copper sitting in the anchor's fan-out corridors.

### 4. Gating (unchanged safety model)
Run as a `place_best` variant. Faults stay the PRIMARY key, so the anneal can NEVER
pick a worse-routed board; `board_harness` fidelity (0 copper faults across all boards)
+ `pcb_critic` score gate the merge. Keep the baseline force placer for non-cluster
boards (it scores 8/10 on realistic boards already — don't regress them).

## Staging
1. Cluster extraction + the deterministic cluster seed (step 2) behind the existing
   `decouple` variant — measure critic + routing vs baseline on bga64 / tqfp64.
2. Add the cluster anneal (step 3) only if the seed alone doesn't reach a clean halo.
3. Incremental cost (the O(cluster)/O(boundary) deltas) only if the whole-board re-route
   per move is too slow at scale.

## Stage-1 seed: TRIED TWICE, INSUFFICIENT (Jun 18 loop — reverted)

Implemented the cluster seed (caps inner rings + series elements outer rings, anchor
centred to fit). Two blockers, both pointing past a mere seed:

1. **The generic legalizer spreads the rings.** Even with the ring pitch set clear of
   the legalizer's overlap threshold (`2·part_r + 2·margin + 0.25`), the inner ring
   tightened (cap min 6.5 mm — it CAN hug) but the mean stayed ~11 mm: `legalize`'s
   spiral-resolve nudges the dense rings and cascades them outward. **The anneal must
   OWN legalization for cluster parts** (exempt them from the generic spread, or
   resolve overlaps by rotating/rescaling the ring, not by shoving parts to free space).
2. **A large rigid cluster regresses routing.** Ringing all 64 cluster parts around the
   BGA raised unconnected 11→14 — the tight cluster blocks signal escape. So the seed
   must be **co-designed with escape**: leave radial channels in the ring aligned to the
   escape corridors (the step-2 "series on escape azimuth" idea, not concentric rings),
   and the cost must include `channel_occupancy` from the start, not as a later refinement.

Net: a deterministic seed is not enough; this genuinely needs the cluster anneal with
cluster-aware legalization + an escape-aware cost. Don't re-attempt seed-only. Engine
left at known-good (bga 11 unconn, 5/10; realistic boards 8/10; 17 boards 0 faults).
