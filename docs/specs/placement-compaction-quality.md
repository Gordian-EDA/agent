# Placement compaction / cluster cohesion — the recurring "wasted area" defect

Found by running the VLM critic (`tools/pcb_critic.py`) across the board suite (Jun 19).
Well-configured boards score 8–9 (ldo 9, bga-decoupled 9, power-buck 8 after the
SA_COHERE_W fix), but a CLASS of boards scores 5–6 with the SAME root complaint:
**wasted board area / parts not clustering** — multi-ic-system (5–6), mcu-board (5),
dual-bga-6layer (5), mechanical-tht (6).

## What it is NOT (ruled out by diagnosis)

- **NOT a content_bounds bug.** The outline DOES tighten to copper+1mm (verified: multi-ic
  outline = 92×42, not the 130×80 budget). The waste is INSIDE the tight outline.
- **NOT fixable by smaller bounds.** Verified: multi-ic at 100×55 vs 130×80 stays ~5–6/10 —
  the parts spread regardless of the budget, so the outline tightens around a sparse interior.
- **NOT (only) the decoupling-cap case** — that specific sub-case (a 2-pad cap whose nets are
  both on an IC) is now handled by `decoupling_pairs` + SA_COHERE_W=8 (commit 060e42c).

## What it IS — two coupled placement-quality gaps

1. **Edge-seek connectors strand far from their parts.** A connector edge-seeks to its
   *currently nearest* edge (SA_EDGE_W, min of the 4 edge distances). Its only net pull is
   often a PLANE net (VIN/GND) whose centroid is the whole board → effectively no directional
   spring. So it sticks to whatever edge it seeded near, even if the IC it feeds is on the
   opposite side — stretching the board with an empty middle. (mcu-board: J1+C11 stranded
   far-left while the MCU cluster is far-right; multi-ic: J1/J2 dropped to the bottom edge
   while all ICs sit in a top band.)
2. **Flat anneal doesn't keep clusters cohesive.** An IC + its decoupling + its support parts
   should form a tight local cluster, with clusters then arranged globally. The current SA is
   FLAT (all parts in one cost), so a small IC (U3) + its caps drift apart from the main IC
   (U2) instead of forming one block — leaving a sparse interior the outline can't tighten away.

## The structural lever (deliberate, NOT a constant tweak)

This is the PCB analog of `locality-aware-placement-search.md` (the schematic reframe):
placement is a MIX of local (tightly-coupled IC+caps clusters) and global (a few nets joining
clusters + edge connectors). The fix is a **two-level / locality-aware placement**, not a
global SA-weight bump — the project memory + CLAUDE.md both flag global force-spring tuning as
fragile (it rebalances every board). Concretely:

- Group each IC with its decoupling/support parts into a CLUSTER (extend `decoupling_pairs`
  to a full cluster membership), anneal each cluster's internal layout tightly, then anneal the
  CLUSTERS as rigid-ish blocks (the existing cluster block-move is a seed of this).
- Make edge-seek **net-aware**: a connector picks the edge nearest the *non-plane* parts it
  connects to (fall back to nearest edge only when it has no directional net), so a power
  connector lands on the IC's side, not a random edge.

## Why deferred (not a cron-tick change)

Both touch the force-layout / SA, which the memory explicitly flags as fragile (a global tweak
rebalances every board). The safe path is the locality reframe done deliberately and gated on
the full 59-board harness (DRC) **and** the critic across the suite (quality), not a
speculative weight bump. The boards are all DRC-clean and functional today; this is a
layout-QUALITY refinement, not a correctness gap.
