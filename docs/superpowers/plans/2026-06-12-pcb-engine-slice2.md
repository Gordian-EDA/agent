# PCB Engine Slice 2: Capacity-Mesh Global Router — Implementation Plan

> **For agentic workers:** execute task-by-task with subagents,
> SEQUENTIALLY (shared cargo target dir). Steps use checkbox (`- [ ]`)
> syntax for tracking.

**Goal:** A global routing stage that survives boards which defeat the
slice-1 greedy router: quadtree capacity mesh over the board, node graph
between leaves, congestion-costed per-net A* over cells with negotiated
rip-up & reroute, producing a `GlobalPlan` (per-net cell paths + boundary
crossings) and an honest congestion report. The plan is *not* copper —
slice 3 turns it into copper. Slice 2's product is consumed as data and
verified for feasibility.

**Spec:** `docs/superpowers/specs/2026-06-12-pcb-engine-design.md`
**Depends on:** slices 0–1 (`RouteProblem`, slice-1 `router.rs` as the
fallback and as the "defeated" baseline for the gate fixture).

**Acceptance gate (from spec):** a fixture that provably defeats slice 1
(≥ 1 failed net under `router::route`) gets a *feasible* global plan
(every net planned, no mesh edge over capacity); congestion is reported,
never silently absorbed.

**Design constants (tunable, define once in `mesh.rs` / `pathing.rs`):**
track pitch = `min_trace_width + clearance` (capacity unit); max quadtree
depth such that leaf size ≥ 4 × track pitch; subdivide a cell while it
contains an obstacle boundary and depth < max; edge capacity =
`floor(shared boundary length / track pitch)` minus obstacle-blocked
portion; congestion cost = base 1 + penalty `(usage/capacity)^2 × K`,
K ≈ 8; rip-up: PathFinder-style history cost, increment ≈ 1 per overflow
iteration, max iterations ≈ 40.

---

### Task 1: quadtree capacity mesh (`mesh.rs` in pcb-engine)

- [x] `CapacityMesh::build(problem) -> CapacityMesh`: quadtree subdivision
      of `bounds` per layer-agnostic XY (capacity is per-layer inside the
      cell). Subdivide while a cell intersects an obstacle edge and depth
      < max depth (constant above). Leaves store, per layer: free area
      fraction and a capacity estimate = `floor(min(w,h) / track_pitch)`
      scaled by free fraction, 0 if fully covered by foreign copper.
      Keepouts/foreign copper cut capacity; a net's own pads do NOT cut
      capacity for that net (store blocking connection indices like
      slice-1's grid, coarse per-cell list is fine).
- [x] Leaf adjacency: edges between leaves sharing a boundary segment
      (handle the quadtree T-junction case: one big leaf ↔ several small).
      Edge capacity per layer = `floor(shared boundary length /
      track_pitch)`, reduced by the obstacle-covered portion of that
      boundary. Deterministic leaf ids (Morton/path order) and edge order.
- [x] `cell_at(point) -> LeafId` lookup for pad → cell seeding.
- [x] Tests: empty board → single leaf (or shallow tree) with sane
      capacity; one central obstacle forces subdivision around it and cuts
      capacity; T-junction adjacency correct; own-net pad does not cut its
      own capacity; determinism (build twice, identical serialization);
      `quad.json` and `led-r.json` build sane meshes.
      Commit: `feat(pcb-engine): quadtree capacity mesh`

### Task 2: congestion-costed pathing + rip-up (`pathing.rs`)

- [x] Per-net A* over the leaf graph (state = (layer, leaf); layer change
      inside a leaf costs via penalty and requires capacity on both
      layers). Edge traversal cost = euclidean center distance × (1 +
      congestion penalty + history cost). Heuristic: euclidean distance to
      nearest target cell — admissible. Deterministic heap tie-break (cost,
      leaf id, layer).
- [x] Multi-point nets: route point 0's cell to nearest of the remaining
      target cells, then grow the tree (same point-to-tree approach as
      slice 1, over cells).
- [x] Negotiated rip-up & reroute (PathFinder): route all nets (order:
      slice-1's half-perimeter ascending, tie-break name); while any edge
      usage > capacity and iter < max: bump history cost on overflowed
      edges, rip up ONLY nets crossing overflowed edges, reroute them
      (deterministic order). Track per-iteration overflow total.
- [x] `pub fn global_route(problem) -> GlobalRouteResult` with
      `GlobalPlan { nets: Vec<NetPlan> }`, `NetPlan { connection, paths:
      Vec<CellPath> }` (cell sequence + entry/exit boundary segments +
      layer per step), plus `CongestionReport { iterations, final_overflow,
      edge_hotspots (top-N by usage/capacity), unrouted: Vec<FailedNet> }`.
      All serializable. Feasible ⇔ `final_overflow == 0 && unrouted.is_empty()`.
- [x] Tests: two nets through a one-track channel — second net detours or
      reports honestly; deliberately impossible (zero-capacity cut) →
      unrouted reported with reason, no panic, no infinite loop (iteration
      cap hit visibly); determinism (two runs serialize byte-equal);
      both slice-1 fixtures get feasible plans.
      Commit: `feat(pcb-engine): congestion-costed global pathing with rip-up`

### Task 3: gate fixture + acceptance test (`fixtures/congested.json`, tests)

- [x] Author `fixtures/congested.json` (hand-written or via a small
      deterministic generator test-helper, checked-in JSON either way): a
      2-layer board with a pin-field / crossing pattern where greedy
      slice-1 ordering walls off later nets (e.g. ≥ 8 nets forced through
      a narrow channel between keepouts with capacity for fewer on one
      layer). Requirements: valid per the slice-0 model, parses, and is
      MINIMAL enough to debug by eye in the SVG render.
      Final geometry: 60×60 mm 2-layer board; 3 mm vertical wall at x=30,
      bottom layer SOLID (no via-relief), top layer with a narrow central
      gap (1.8 mm @ y=30, every net's natural crossing) + a far corner
      relief gap (2.6 mm @ y=4). 8 nets, vertically-reversed endpoints, all
      on top so they cross in the central gap.
- [x] The gate test, in `tests/` or `router`/`pathing` integration: assert
      `router::route(congested)` has ≥ 1 failed net (proving the fixture
      defeats slice 1 — if the naive router ever starts solving it, the
      fixture must be tightened, not the assertion deleted) AND
      `global_route(congested)` is feasible (0 overflow, 0 unrouted).
      `tests/global_gate.rs`: slice 1 fails 3 nets (N0/N1/N7); global is
      feasible. Comments forbid weakening the assertions.
- [x] Congestion report sanity on the fixture: hotspots non-empty during
      iteration (report the iteration count > 1 to prove rip-up engaged —
      capture via the report, not internal prints).
      iterations=3, overflow_history=[1,1,1,0] (first pass overflows the
      central gap by 1; negotiation routes a net to the corner relief),
      hotspots non-empty, serialize-twice byte-equal.
      Commit: `feat(pcb-engine): congested gate fixture for global routing`

### Task 4: mesh/plan SVG overlay (`svg.rs` extension)

- [x] `pub fn render_global_svg(problem, mesh, plan_result) -> String`:
      slice-1 board rendering underneath (reuse existing helpers), plus
      leaf boundaries (thin grey), per-leaf utilization heat tint (green →
      red by max-layer usage/capacity), net cell paths as translucent
      ribbons through cell centers, unrouted nets' endpoints highlighted.
      Pure string assembly, no new deps.
- [x] Tests: element-count assertions on a fixture; extend the
      render-all-fixtures helper to also write `*-global.svg` into
      `target/pcb-render/` for eyeballing.
      Commit: `feat(pcb-engine): global plan SVG overlay`

### Task 5: lint carry-over — layer-name validity

- [ ] New lint check `InvalidLayer`: any trace whose
      `layer.index(problem.layer_count)` is `None`, and any route point
      likewise (the slice-1 blind spot, institutionalized). Add to
      `DrcViolation`, wire into `lint()`, trigger-test it (a trace on
      "inner1" on a 2-layer board), assert fixtures stay clean.
      Commit: `feat(pcb-engine): lint validates layer names`

### Task 6: wrap-up (inline, main loop)

- [ ] `cargo test --workspace` green; pcb-engine clippy clean; render all
      fixtures incl. global overlays and EYEBALL THE PNGs (slice-1 lesson:
      a render catches what both oracles miss); tick spec slice-2 row /
      status; record tuned constants + findings in the spec; update memory.
      Commit: `chore(pcb-engine): slice 2 wrap-up`

## Self-review notes

- The plan's feasibility check (capacity accounting) is the mesh's own
  bookkeeping — slice 2 has no exact-geometry oracle for the plan itself.
  That is acceptable ONLY because the plan is not copper; slice 3's
  detailed router + the existing lint re-verify everything in exact
  geometry. Do not claim DRC-cleanliness for a global plan.
- The gate fixture must FAIL slice 1 by construction and that failure is
  asserted forever — it documents why slice 2 exists.
- Determinism discipline identical to slice 1: sorted/Morton orders
  everywhere, no HashMap iteration leaks, serialize-twice tests.
- Keep `mesh.rs`/`pathing.rs` independent of `grid.rs`/`astar.rs` — the
  fallback path must stay untouched and always-correct.
