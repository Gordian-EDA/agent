# PCB Engine Slice 1: Naive Grid Router — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** An end-to-end, DRC-checked autorouting pipeline: sequential
per-net A* on a fine grid, 2 layers with via cost, strict DRC lint, SVG
debug render, and `kicad-cli pcb drc` as external oracle. Naive but honest —
small boards route fully with zero violations; failures are reported, never
silent.

**Spec:** `docs/superpowers/specs/2026-06-12-pcb-engine-design.md`
**Depends on:** slice 0 (`RouteProblem`/`RouteSolution`, connectivity
oracle, kicad-bridge pcb I/O, fixtures).

**Environment fact:** dev box has KiCAD 9.0.9 — `kicad-cli pcb drc --format
json --exit-code-violations` works. Version-gate DRC tests at `cli_version`
major ≥ 8 anyway (CI/other machines), printing a visible skip message.

**Design constants (tunable, define once in `router.rs`):** grid pitch =
`max(0.1 mm, (min_trace_width + clearance) / 2)`; via cost ≈ 25 grid steps;
bend cost ≈ 2 steps; obstacle inflation = `clearance + min_trace_width/2`.

---

### Task 1: grid model (`grid.rs` in pcb-engine)

- [ ] `RouteGrid::build(problem) -> RouteGrid`: per-layer occupancy bitmaps
      over `bounds` at the pitch above. Rasterize each obstacle inflated by
      the inflation constant into every layer it occupies. Cells store the
      blocking connection name index (or BLOCKED_ALL for keepouts/foreign
      copper with no net) so lookups can answer "free for connection c?" —
      a pad never blocks its own connection. Board edge: cells whose
      inflated disc leaves `bounds` are blocked.
- [ ] Coordinate mapping helpers (`cell ↔ mm center`), deterministic
      (floor-based, no float accumulation: `x = min_x + (i as f64)*pitch`).
- [ ] Tests: a pad blocks neighboring cells within inflation on its layer
      only; own-net query passes over it; out-of-bounds blocked; mapping
      round-trips.
      Commit: `feat(pcb-engine): routing grid with net-aware occupancy`

### Task 2: A* core (`astar.rs`)

- [ ] 3-D state `(layer, ix, iy)`; moves: 4-neighbor same-layer (cost 1 +
      bend cost when direction changes), layer change (via cost) allowed only
      where ALL layers' cells are free-for-this-connection (via barrel).
      Heuristic: Manhattan distance / nothing fancy; admissible. Deterministic:
      BinaryHeap keyed `(cost, state)` with total tie-break order.
- [ ] Multi-target: A* to the NEAREST of a target set (heuristic = min over
      targets) — enables point-to-tree routing.
- [ ] Tests: straight route on empty grid; detour around a wall; via hop when
      a layer is fully walled; unreachable → None; determinism (twice, equal).
      Commit: `feat(pcb-engine): grid A* with bend and via costs`

### Task 3: per-net router + solution assembly (`router.rs`)

- [ ] Net order: ascending by bounding-box half-perimeter of its points
      (short local nets first), tie-break by name — deterministic.
- [ ] Per connection: seed targets = cells of point 0; for each further
      point, A* from its cells to the routed-tree cell set; on success mark
      path cells as that connection's copper (becomes obstacle for later
      nets via the grid's name index) and record path. Convert cell paths →
      mm polylines, split at layer changes (via at the transition point),
      merge collinear runs (reuse the simplify idea from
      `sch-engine/route.rs`, fresh copy — crates stay decoupled).
- [ ] `pub fn route(problem) -> RouteResult { solution, failed: Vec<FailedNet> }`
      — `FailedNet { connection, reason }`; a failure never panics and never
      silently drops a net.
- [ ] Tests: `led-r.json` routes fully; `quad.json` routes fully (asserting
      ≥ 1 via used); connectivity oracle empty on both; determinism
      (serialize solution twice, byte-equal).
      Commit: `feat(pcb-engine): sequential naive grid router`

### Task 4: DRC lint (`lint.rs`) — the strict oracle

- [ ] `pub fn lint(problem, solution) -> Vec<DrcViolation>` checks, exact
      geometry (segment/segment, segment/rect distances — NOT grid-based;
      the lint must be independent of the router's model):
      `ClearanceTraceTrace` (different connections, same layer, gap <
      clearance), `ClearanceTraceObstacle` (foreign or unowned copper),
      `ClearanceViaAny`, `TraceWidthBelowMin`, `OutOfBounds`, plus the
      slice-0 connectivity violations folded in (one report).
      Brute-force O(n²) pairs is fine at this scale; structure so a spatial
      index can slot in later.
- [ ] Tests: hand-built violating solutions trigger each variant exactly;
      router outputs on both fixtures lint CLEAN (the gate); a deliberately
      too-close pair of parallel traces fails.
      Commit: `feat(pcb-engine): strict DRC lint`

### Task 5: SVG debug render (`svg.rs`)

- [ ] `pub fn render_svg(problem, solution) -> String`: board outline, pads
      (grey, foreign copper darker), traces (top red, bottom blue, 60%
      opacity so overlaps read), vias (ringed circles), failed nets' points
      highlighted. Pure string assembly (house style — no new deps).
- [ ] Test: output contains expected element counts; write rendered fixtures
      to `target/pcb-render/` in a `#[test]` helper for eyeballing (like the
      sch render harness).
      Commit: `feat(pcb-engine): SVG debug render`

### Task 6: end-to-end through KiCAD + external DRC oracle

**Files:** `crates/kicad-bridge/tests/pcb_route_e2e.rs`, extend
`crates/kicad-bridge/src/cli.rs` with `pub fn drc(&self, pcb: &Path) ->
io::Result<DrcReport>` mirroring `erc()` (`["pcb", "drc", "--format",
"json", "--all-track-errors", "--exit-code-violations"]`, severity counts +
violation list incl. `unconnected_items`).

- [ ] E2E test (version-gated ≥ 8, KiCAD-installed-gated like existing cli
      tests): read `two_res.kicad_pcb` → `read_problem` → `route` → assert
      no failed nets → `write_solution` → `kicad-cli pcb drc` on the result
      → **zero violations and zero unconnected items**. This is the slice's
      acceptance gate.
- [ ] In-house lint runs on the same solution and must also be clean —
      disagreement between the two oracles fails the test and is
      investigated, not suppressed.
      Commit: `feat(kicad-bridge): pcb drc oracle + routed-board e2e`

### Task 7: wrap-up

- [ ] `cargo test --workspace` + clippy clean; render all fixtures; check
      spec slice-1 row; note any tuning of the design constants in the spec.
      Commit: `chore(pcb-engine): slice 1 wrap-up`

## Self-review notes
- The lint (Task 4) deliberately re-measures with exact geometry instead of
  trusting grid bookkeeping — the router can be wrong, the oracle may not be.
- Two oracles (in-house + kicad-cli) cross-check each other in Task 6; the
  plan treats disagreement as a bug to chase, which is how the schematic
  side caught its early emitter bugs.
- Via barrel check ("free on ALL layers") is conservative for 2-layer
  boards and correct for the v1 scope (no blind/buried vias).
