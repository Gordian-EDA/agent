# Locality-aware placement search (two-level annealing)

Status: **design + staged implementation** (2026-06-17). Engine: `crates/sch-layout/src/floorplan.rs`
(`anneal_items`, `Anneal::search`, `layout_cost`). Gated on the premium oracle
(`LAYOUT_SEARCH=anneal floorplan_netlist`) + the VLM critic.

## Why

Schematic placement is a **mix of local and global** structure. Connectivity induces a natural
hierarchy:

- A **cluster** = an anchor (IC) + its tap satellites + the frozen idioms it anchors (already
  computed as the `blocks` map in `anneal_items`). Its nets are mostly *internal*.
- Clusters are joined to each other by only a **few global nets** — inter-IC signals and the V+
  rail. (Distributed local grounds — `rail_locals` — *sharpen* this: GND stops being one global net
  touching every cluster, so the only remaining cross-cluster coupling is real signals + V+.)

The current SA ignores this: **every move re-routes the whole sheet** (`build_writer` + `layout_cost`
over all nets, O(board) per move), which forces the `iters ≤ 420_000/pins` cap and starves dense
boards. And the only "global" operator — the block move — is trapped at a `step(1)` radius, so a
coherent idiom can never migrate across a congested region (the crystal/reset-cluster congestion).

## The key property: locality makes BOTH local and global moves cheap

HPWL (a net's bounding-box half-perimeter) is **invariant under a rigid translation of all its
pins**, and so is the crossing pattern among wires that move together. Therefore:

- A **local** move (a satellite inside its cluster) re-costs only that cluster's *internal* nets → **O(cluster)**.
- A **global** move (slide/swap a *whole* cluster) leaves every intra-cluster net and intra-cluster
  crossing unchanged — only the cluster's **boundary nets** (inter-cluster signals/rails) and its
  crossings with the destination region re-cost → **O(boundary)**.

Neither is O(board). This is *why* incremental cost is the right unlock here — the problem's locality
is exactly what bounds the per-move delta.

## Design

### Two-level move set, coupled to temperature

| Phase | Dominant moves | Explores |
|---|---|---|
| Hot | **Global**: relocate a whole cluster to a clear band; swap two clusters; reorient/mirror a cluster | board topology (the crystal-congestion fix) |
| Mid | **Boundary**: migrate a satellite to a neighbor cluster; flip a cluster's side | interface tightening |
| Cold | **Local**: fine relocate / orient / swap *within* settled clusters | exploitation |

"Range" becomes *hierarchy level*, not just distance — a generalization of VPR-style range limiting.
The current block move is the seed of the Hot row but with a Cold radius; lift its radius (range-limited
by anneal progress) and it becomes the cluster-relocate operator.

### Locality-decomposed cost (the data structure)

Cache cost in two tiers:

- per-cluster **internal** cost — recomputed only when that cluster is internally perturbed;
- **boundary** cost — the inter-cluster nets + cross-cluster crossings — recomputed on any move.

A local move dirties one cluster's internal + its boundary; a global (rigid) cluster move dirties only
boundaries. The boundary cost is literally the min-cut between partitions; it is what keeps clusters
from being optimized in dishonest isolation.

### Per-move proxy, periodic verify (safe staging)

The cheap per-move objective is a **geometric proxy**: HPWL over the moved cell's incident nets +
exact-incremental geometric correctness (overlaps 1500, grid_order 1200, spread — the high-weight terms
stay **exact**, never approximated). The expensive routed terms (crossings/corners/congestion/body-cross,
which need the router) are paid **only on a new proxy-best** and at the existing candidate pick. The
`Anneal::search` candidate pick (re-asserts the real `warning_count`, ships strict-min on
`(warnings, premium-cost)`, greedy is always `candidates[0]`) **already re-asserts truth twice
downstream**, so proxy ↔ true-cost drift degrades *optimization only*, never the ship-≥-greedy /
electrical-truthfulness contract.

### Parallel local refinement (bonus)

Clusters are near-independent (coupled only through the boundary), so their **local** anneals can run
concurrently (rayon, already the harness): a block-coordinate / Jacobi step — anneal each cluster's
interior in parallel, then a short **global** pass reconciles the boundaries. Scales with cluster count
on dense boards, unlike the current "3 whole-board chains."

## Staged implementation

1. **Incremental geometric proxy + cluster-cost cache** (this unlocks everything). Add `proxy_cost`
   (HPWL via `inc` + incremental overlap/grid/spread). Route the per-move inner loop through it; pay the
   full `build_writer`+`layout_cost` only on a proxy-best. Leave `Anneal::search`'s candidate pick
   untouched. Start **additive** (a new anneal path that uses the proxy with a larger iteration budget)
   so the tuned full-cost paths are unrisked, then promote once it's shown to win/tie.
2. **Large cluster-jump move** — lift the block move's radius (range-limited by anneal progress) so a
   whole cluster can relocate to a clear band; optional cluster-swap branch. Rides on the cheap cost.
3. (Later) cluster-swap + mirror-anchor moves; LAHC acceptance for schedule robustness; parallel
   tempering behind a flag.

## Validation (hard gates)

- **Premium oracle**: `LAYOUT_SEARCH=anneal cargo test --release -p sch-layout --test floorplan_netlist`
  — a prettier/faster render that breaks connectivity is a regression.
- **VLM critic** on the tuned fixtures (`555`/`uart`/`mcp1703`/`divider`) — confirm no tidiness
  regression; aim for consistently 9+ on dense circuits. `--engine-clean` only when the engine
  `body_crossings`/`ic_crossings` ground truth is 0.
- Keep the **free (greedy) path bit-identical** if shared code is touched (re-parenthesizing the base
  cost sum flips chaotic SA acceptances — a measured regression).

## Sources

VLSI/FPGA placement annealing: TimberWolf (Sechen & Sangiovanni-Vincentelli — HPWL+overlap cost,
range windows, cluster moves, all rotations+reflections); VPR / Betz & Rose (incremental bounding-box,
adaptive range limit tied to the ~0.44 acceptance ratio); multilevel placement (mGP/IMF); Late-Acceptance
Hill Climbing (Burke & Bykov, EJOR 2017); parallel tempering / replica exchange (Earl & Deem). Full
research synthesis: workflow `sa-design-research` (this session).
