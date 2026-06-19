# Negotiated-congestion routing — design, result, and the capacity finding

## Motivation (the hypothesis)

The hard BGA boards (`bga-system50`, `bga-mega90`, `dual-bga-bus`, `dual-bga-system`,
`bga64-stress`, `tqfp32-mcu`) all route DRC-clean but leave a few nets unrouted. The
greedy slice-1 router (`router::route` + `route_iterated`) hard-blocks each routed net's
copper, so two nets contending for one corridor cannot both route — one fails, and a
priority-retry only swaps *which* fails. **Hypothesis:** the unrouted nets are mutual
*ordering*-congestion, and a negotiated-congestion (Pathfinder) router would route them.

## The router (built + validated, then reverted — see Result)

A self-contained candidate added to `route_auto`, **keep-best** (adopted only if it has
strictly fewer faults than the naive/detailed winner; gated to run only when those left
failures). Design that worked:

- **Grid:** reuse `RouteGrid::build` (pads + board edge hard-block). Do **not** mark
  traces — foreign traces stay passable, so nets can share at a cost.
- **Congestion A\*** (own, isolated — no change to the shared `astar`): cost of entering a
  cell = `STEP + present[cell]*pp + history[cell]*P_HISTORY`; vias skip plane layers
  (`plane_mask_for`). `is_free_for` still hard-blocks pads/edges.
- **Growing present penalty** (the key Pathfinder ingredient): `pp = P_PRESENT*(1+iter)`,
  so early passes tolerate overlap to find good paths and late passes make it ruinous —
  this rising pressure is what forces convergence. (A *fixed* penalty never converges.)
- **Halo-aware congestion** (the second key fix): record each trace into `present` over
  its **clearance halo** (Chebyshev radius `≈ (min_trace+clearance)/pitch ≈ 2`), not just
  its exact cell — two nets in adjacent cells share no cell but still violate clearance.
  Convergence (no cell `present>1`) then means every trace pair clears by the rule, so the
  result survives `reconcile`.
- **Loop:** iterate route-all-with-congestion, accumulate `history` on over-used cells,
  stop at no-overuse (converged) or `MAX_ITERS`. Final pass routes with the learned
  history and `emit_path`s the copper.

## Result — the frontier is CAPACITY, not ordering

Measured on the suite:

- **Without the clearance halo**, the router *converges* to all-nets cell-disjoint
  (e.g. `bga-system50` 32/32 in 17 iters).
- **With the clearance halo** (the physically-correct constraint), it does **not**
  converge — it can only route all nets by *overlapping within clearance*, which
  `reconcile` then drops. Post-reconcile it routes **no more DRC-clean nets than the
  naive** (~27/32), so `route_auto` never adopts it (`route=naive` everywhere).

**Conclusion:** the inter-BGA / bus corridors are at **clearance-capacity**. The greedy
naive router already routes *near that capacity*. The unrouted nets are not an ordering
problem a smarter same-layer router can fix — there is simply no clearance-respecting
placement of all of them in the available corridor.

## What actually moves the metric (do these instead)

1. **More copper layers** — `rules.layers: 4` already; an inner *signal* layer (not just
   planes) or 6-layer adds corridor capacity directly.
2. **Finer pitch / microvias (HDI)** — for ≤0.8 mm parts the escape is via-seat-limited,
   not congestion (see `bga-escape-routing.md`).
3. **Placement that shortens / widens the corridor** — spread the bus, move the two BGAs
   closer or rotate them so fewer nets share one channel; route the overflow *around*.
4. **Per-net width awareness in capacity** — already have per-net-accurate clearance; a
   capacity model that bills wide nets more would place them first.

A negotiated router only helps an *ordering*-limited board (corridor has room but the
greedy order wastes it). None in the current suite is. Re-introduce this design (it is
correct and keep-best-safe) if such a board appears — but expect ~16 s/board, so keep it
gated on failures and consider capping iterations by board size.
