# PCB Engine Slice 3: Detailed Routing — Implementation Plan

> **For agentic workers:** execute task-by-task with subagents,
> SEQUENTIALLY (shared cargo target dir). Steps use checkbox (`- [ ]`)
> syntax for tracking.

**Goal:** Turn a slice-2 `GlobalPlan` into DRC-clean copper: assign each
net's cell-boundary crossings to concrete points (no intra-cell clash),
route each cell's interior with a fine octilinear (45°) A*, insert vias at
planned layer transitions, stitch per-cell paths into per-net polylines,
simplify, and verify with BOTH oracles. Adds the full pipeline entry point
with the slice-1 router as automatic fallback, and wirelength/via metrics
so routers can be compared.

**Spec:** `docs/superpowers/specs/2026-06-12-pcb-engine-design.md`
**Depends on:** slice 2 (`GlobalPlan`/`CellPath`/`Crossing`, `CapacityMesh`),
slice 1 (`grid.rs`/`astar.rs` may be REUSED here — the detailed stage is
their natural home; only `mesh.rs`/`pathing.rs` must stay independent of
them), slice 0 oracles.

**Acceptance gate (from spec, adapted):** `congested.json` — which defeats
slice 1 — gets **DRC-clean copper end-to-end** (global → detailed →
`lint()` empty); `led-r`/`quad` stay clean through the new pipeline; via
count + total wirelength reported for naive vs pipeline on all fixtures.
KiCAD `pcb drc` e2e stays green routing `two_res.kicad_pcb` through the
pipeline. (The tscircuit benchmark comparison stays DEFERRED per the
slice-0 finding — upstream ships no raw problems; note it visibly in the
spec, do not fake it.)

**Slice-2 contract reminder (from its author):** `CellStep { leaf, layer,
center, via, exit: Option<Crossing> }`; `Crossing { neighbor, edge, layer,
at }` where `at` is the shared-boundary midpoint (a DEFAULT the detailed
stage redistributes), `edge` indexes `mesh.edges` (recovers boundary
geometry + per-layer capacity); entry of step i+1 is step i's exit
reversed; last step has `exit: None`; `via: true` = layer change inside
that leaf, step's `layer` is entry layer, exit crossing's `layer` is
post-via.

**Design constants (define once in `detail.rs`):** detailed grid pitch =
slice-1 `grid::grid_pitch` (shared formula); crossing slot spacing along a
boundary = track pitch (`w_min + clearance`), slots centered on the usable
(unblocked) portion; diagonal move cost = ceil(step × √2) in integer cost
units; hotspot threshold: a cell is "dense" if free-area fraction < 0.5 or
pad count ≥ 4 (dense cells get the full-resolution window; sparse cells may
use 2× pitch for speed — only if tests stay clean, else everything runs
full-resolution).

---

### Task 1: crossing assignment (`crossing.rs` in pcb-engine)

- [x] `pub fn assign_crossings(problem, mesh, plan) -> CrossingAssignment`:
      for every mesh edge used by the plan, collect all (net, crossing)
      uses per layer; place concrete crossing points spaced ≥ track pitch
      along the unblocked portion of the shared boundary. Order nets along
      the boundary to minimize intra-cell crossing: sort by the projection
      of the net's OTHER anchor in each adjacent cell (entry-side and
      exit-side positions), deterministic tie-break by net name. Capacity
      respected by construction (slice 2 guarantees usage ≤ capacity; if
      usable length still can't fit the slots — boundary blocked unevenly —
      report it as an `AssignmentOverflow` failure, never squeeze below
      clearance).
- [x] `CrossingAssignment`: per net, per path, the ordered list of concrete
      entry/exit points (mm, layer) replacing the default midpoints —
      shaped so Task 2 can consume a per-cell work order: `CellJob { leaf,
      connection, layer(s), terminals: Vec<Terminal> }` where Terminal =
      pad point | entry point | exit point | via site. Build the via site
      placement here too: a `via: true` step needs a via location inside
      the leaf where a via fits (clearance to foreign copper, computed
      against problem geometry); default = leaf center, nudged
      deterministically (spiral over detailed-grid offsets) until it fits.
- [x] Serializable; determinism test (assign twice, byte-equal); tests on
      `congested.json` + `quad.json`: every plan crossing got a concrete
      point, spacing ≥ track pitch on every boundary, points lie ON the
      shared boundary within epsilon, via sites clear of foreign copper.
      Commit: `feat(pcb-engine): boundary crossing assignment`

### Task 2: per-cell detailed router (`detail.rs`)

- [x] Fine A* within a cell window: reuse `grid.rs`/`astar.rs` machinery
      scoped to the leaf rect inflated by one track pitch (so routes can
      hug boundaries), at the detailed pitch. Extend `astar.rs` moves with
      the 4 diagonals (cost ceil(√2 × step), corner-cutting forbidden:
      a diagonal requires both orthogonal neighbors free for the
      connection — keeps clearance honest at 45° corners). Keep the
      4-neighbor behavior available so slice-1 `route()` is unchanged
      (default costs/moves identical to before — its tests must not
      change). DONE: `RouteGrid::build_window` (true-board-edge blocking
      only); `astar::MoveSet::{Orthogonal,Octilinear}` + `DIAG_COST=2`,
      `AStarCosts.moves` defaults Orthogonal so slice-1 is bit-identical.
- [x] `pub fn route_cells(problem, mesh, assignment) -> CellRouteResult`:
      per cell (deterministic leaf order), per net (slice-1 net order),
      connect that cell's terminals (entry→exit→pads→via sites) on the
      cell-local grid; mark routed copper + clearance halo into the local
      grid as in slice 1 so later nets in the SAME cell avoid it. Failures
      are per-net `FailedNet` with cell id in the reason — honest, no
      panic. DONE in `detail.rs`. Endpoint exactness via terminal snapping.
      FINDING: led-r routes all cells; quad fails 4/45 jobs and congested
      fails 27/107 — central crossing cells where several top-layer nets
      converge on near-coincident boundary points exceed the greedy
      per-cell budget. This is the anticipated honest failure → Task 3
      fallback (slice-1) for affected nets; the crossing-assignment
      projection heuristic is slice-3.5 material, not a gate fudge.
- [x] Tests: single cell with 2 terminals routes straight; diagonal path
      is used when cheaper (assert a 45° segment exists); corner-cutting
      blocked test (diagonal through a blocked orthogonal pair is NOT
      taken); dense-cell pin field (hand-built tiny problem) routes all
      nets; determinism. DONE (astar: 3 diagonal tests; grid: 2 window
      tests; detail: 7 tests incl. endpoint exactness + honest-failure).
      Commit: `feat(pcb-engine): per-cell detailed router with 45° moves`

### Task 3: stitch + pipeline entry (`pipeline.rs`)

> **DEVIATION (current reality, from Task 2):** `route_cells` is clean on
> led-r but fails 4/45 jobs on quad and 27/107 on congested — the
> conservative per-cell Chebyshev clearance halo defeats dense
> boundary-crossing clusters at detailed pitch. The strict end-to-end gate
> (quad/congested clean through `route_detailed`) is therefore **deferred to
> the detail-fidelity-fix task** (the new Task 3.5 / the slice gate, Task 4).
> Task 3 tests assert *today's* honest behaviour: led-r clean through
> `route_detailed`; quad/congested report failures honestly and `route_auto`
> falls back to naive correctly. Affected tests carry the comment "After the
> detail-fidelity fix these fixtures must pass through route_detailed cleanly
> — tighten then." Baseline failed-net counts through `route_detailed`:
> led-r 0, quad 4 (all "cell" provenance), congested 27 (all "cell").

- [x] Stitch per-cell paths per net across crossings into continuous
      polylines (endpoints meet exactly at assigned crossing points), drop
      vias at via sites, simplify (extend slice-1 `simplify` to also merge
      collinear 45° runs — same epsilon discipline), emit
      `RouteSolution`. DONE in `pipeline.rs::stitch`/`join_polylines`:
      byte-exact (`quant` to ~1 nm) degree-2 endpoint joins; degree-≥3
      T-junctions kept as shared vertices (multi-point nets); reuses the
      45°-aware `detail::simplify` (made `pub(crate)`, not duplicated);
      vias deduped by exact position.
- [x] `pub fn route_detailed(problem) -> RouteResult`: global_route →
      assign_crossings → route_cells → stitch. Any stage failure folds
      into `RouteResult::failed` with provenance in the reason string
      (`"global: …"`, `"assign: …"`, `"cell …: …"`). DONE. A net failing
      anywhere contributes NO copper (dropped, reported failed). Global
      `final_overflow > 0` folds as a single board-level "global: …"
      pseudo-failure.
- [x] `pub fn route_auto(problem) -> RouteResult`: route_detailed; if any
      net failed, fall back to slice-1 `route()` and return whichever
      result has fewer failed nets (naive wins ties). DONE. Provenance is
      a serializable `RouterKind { Naive, Detailed }` enum on `RouteResult`
      (slice-1 `router::RouteResult` is a separate type, untouched).
- [x] `pub fn metrics(solution) -> RouteMetrics { wirelength, via_count,
      trace_count }` (serializable; wirelength = Σ polyline lengths). DONE.
- [x] Tests: led-r through `route_detailed` → 0 failed, `lint()` EMPTY,
      metrics sane; quad through `route_detailed` → ≥1 failed net with
      "cell" provenance; quad through `route_auto` → naive wins, 0 failed,
      lint clean; congested through `route_auto` → honest failures (naive's
      3), provenance naive; determinism byte-equal on `route_detailed(quad)`;
      partial-net rule (no trace/via of a failed net in the solution); plus
      stitch unit tests (degree-2 join, reversed join, T-junction kept
      separate, metrics sum). DONE (12 tests in `pipeline::tests`).
      Commit: `feat(pcb-engine): detailed routing pipeline with fallback`

### Task 3.5 — Fidelity fix: exact-geometry halos (`fix(pcb-engine): detail-stage clearance fidelity — exact-geometry halos`)

The over-conservative per-cell clearance model was replaced with exact-geometry
halos. **What changed:**

- **Euclidean clearance halo** (`grid::mark_net_halo_euclid`): a foreign cell is
  blocked only when its centre is *strictly* inside the legal centre-to-centre
  spacing `w_min + clearance` (= 0.45 mm), Euclidean — a foreign centreline at
  exactly the spacing is legal. Replaces the slice-1 Chebyshev radius-2 box (which
  blocked legal cells at axis distance 0.45 and over-blocked diagonals to 0.64).
  Slice-1's `route()` keeps its Chebyshev halo (a separate method) and is
  **bit-identical** (serialize-compared before/after on all three fixtures).
- **Finer detail grid** (`grid::build_with_pitch`, `RouteGrid::pitch/2`): the
  detailed stage routes on a half-design-pitch lattice (0.1125 mm), shrinking the
  cell-centre snap distortion the exact lint measures at dense crossings to ≤ a
  quarter of the design pitch. Slice-1 keeps the design pitch.
- **Shared full-board occupancy grid** for the detailed stage so a net keeps
  clearance from foreign copper routed in *abutting* leaves (per-cell windows are
  blind to each other); each job's A* is still **bounded** to its leaf window
  (`astar::search_bounded` + `CellBounds`), with an unconfined retry on failure.
- **Via barrels as first-class obstacles**: every assigned via site is reserved up
  front and every via gets a through-hole clearance halo on all layers; the A* via
  move checks a Euclidean clearance disc (`AStarCosts::via_clear_radius_cells`,
  `via_barrel_clear`) so a spontaneous via cannot strand its barrel near foreign
  copper. Slice-1 default radius 0 ⇒ unchanged.
- **Slot spreading** (`crossing::place_slots`): crossing slots spread across the
  usable boundary (with an end margin so two boundaries' slots never stack at a
  shared leaf corner) instead of minimal-pitch packing, giving saturated
  boundaries snap margin.

**Measured before/after** (failed nets through `route_detailed`; `lint()`
clearance/via violations through `route_detailed` in parentheses):

| fixture    | before        | after        |
|------------|---------------|--------------|
| led-r      | 0  (0 geom)   | 0  (0 geom)  |
| quad       | 4  (0 geom)   | 4  (0 geom)  |
| congested  | 27 (0 geom)   | 21 (0 geom)  |

**Geometry fidelity is fully fixed**: there are **zero clearance / via DRC
violations** in any fixture's `route_detailed` output (all remaining `lint`
entries are downstream `Connectivity::Unconnected` from the *dropped* failed
nets, not geometry defects). Slice-1 invariant; `global_gate` green.

**Honest residual (FINDING — the strict 0-failed gate is NOT reached):** the
remaining failures are routing-*completeness* gaps, not geometry:

1. **congested's wall is exactly saturated.** The top-layer wall has total
   crossing capacity **8** for **8** nets (relief gap = 5, central gap = 3),
   *zero* slack (measured per-edge). The nets funnel-and-turn through 1.15–1.95 mm
   gaps at exactly track pitch; the turns need room the saturated gap does not
   have. **Capacity calibration (avenue c) is off the table here**: any wall-edge
   derate makes the global plan infeasible, breaking `global_gate`'s feasibility
   assertion; and increasing capacity (floor+1) makes congested first-pass
   feasible, breaking the rip-up-engagement assertion. Confirmed both directions
   break the gate.
2. **quad's centre is physically over-converged.** Six nets cross within one tiny
   central quadtree leaf; on a single layer their crossings cannot all keep
   clearance, and the global plan's layer split + crossing-projection ordering does
   not de-conflict them enough for the per-cell A* to realise.

Reaching 0 failed nets needs a **rip-up/reroute detailed router** (negotiate the
saturated wall the way the global stage negotiates capacity) and/or a stronger
**crossing-assignment de-confliction** at over-converged centres — the slice-3.5
crossing-heuristic work the plan's self-review anticipated, materially larger than
a clearance-fidelity fix. Task 4's strict gate stays deferred on that finding.

### Task 4: the slice gate — congested fixture clean + KiCAD e2e + metrics

- [ ] Gate test (pcb-engine `tests/detailed_gate.rs`): `congested.json` —
      `route(…)` still fails (slice-1 defeat preserved) AND
      `route_detailed(…)` has 0 failed nets AND `lint()` is EMPTY on its
      solution AND ≥ 1 via OR 0 vias with justification (the fixture is
      single-layer-crossable after global negotiation — assert what's
      true, with a comment). Metrics comparison table printed via the
      test (naive-where-it-works vs pipeline on all fixtures) and asserted
      ≥ sane bounds (pipeline wirelength within 3× of naive on fixtures
      where both succeed — honesty bound, tune from observation, comment
      actuals).
- [ ] KiCAD e2e (kicad-bridge `tests/pcb_route_e2e.rs` extension): route
      `two_res.kicad_pcb` through `route_detailed` as well; write, DRC:
      zero violations / zero unconnected (same carve-out for the known
      `lib_footprint_mismatch` warnings). In-house lint also clean —
      cross-oracle disagreement is a bug to chase, not suppress.
- [ ] Render `congested-detailed.svg` (existing `render_svg` on the
      pipeline's solution) in the render-all helper; EYEBALL it.
      Commit: `feat(pcb-engine): detailed routing gate — congested board clean`

### Task 5: wrap-up (inline, main loop)

- [ ] `cargo test --workspace` green; clippy clean on touched crates;
      render + eyeball ALL fixture PNGs (incl. congested-detailed);
      spec slice-3 row + findings (constants actually used, hotspot
      threshold behavior, benchmark deferral note); memory update.
      Commit: `chore(pcb-engine): slice 3 wrap-up`

## Self-review notes

- Crossing assignment is where "no intra-cell clash" is won or lost; the
  projection-ordering heuristic is deliberately simple — if cells still
  fail to route, the honest failure surfaces in `route_cells`, falls back,
  and the finding goes in the spec (that's slice-3.5 material, not a
  reason to fudge the gate).
- Diagonal corner-cutting rule (both orthogonals free) is conservative;
  the lint re-verifies everything in exact geometry anyway. The lint is
  unchanged this slice on purpose — it must stay an independent oracle.
- `route_auto` keeps the spec's promise that slice-1 remains the
  always-correct fallback forever; provenance tagging keeps "which router
  produced this" out of the guessing business.
- 2× pitch in sparse cells is an OPTIONAL optimization gated on clean
  tests — correctness first, speed only if free.
