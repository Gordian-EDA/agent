# PCB Copper Autorouter (`pcb-engine`) — Design

**Date:** 2026-06-12
**Status:** Approved (slices 0–1 complete; slice 2 next)
**Scope lock:** copper autorouting, built in-house in Rust, in this repo.

## Problem

Pillar 3 of the copilot design (specs/2026-06-09 §16) needs PCB routing. No
existing piece of this repo touches `.kicad_pcb`. External routers
(FreeRouting) are Java-bridge dependencies and conflict with the scope lock.

## The core bet

PCB autorouting is a two-phase problem (global routing → detailed routing)
with mature deterministic algorithms. tscircuit's contribution is not AI —
it is the capacity-mesh formulation that makes both phases tractable and
incremental. So: **the routing engine is deterministic; the LLM sits around
it, never inside it.** Same bet sch-engine makes for schematics, and the
only defensible one — LLMs cannot emit DRC-clean coordinates (§4 of the
copilot spec).

### LLM role (around the engine)

| Role | Mechanism |
|---|---|
| Placement / floorplan proposal | Model groups parts, proposes regions ("decoupling caps hug power pins, connectors at the edge"); a deterministic legalizer snaps to grid + courtyards. Highest-leverage role: routing success is dominated by placement. |
| Constraint authoring | Intent → net classes (power wider, diff pairs), layer hints, keepouts — DRC-style rules the router consumes. |
| Failure triage | Router reports congestion/failed nets with provenance; agent decides what to relax (nudge a part, add a layer, reorder priority). |
| Vision critique | Render routed board → LLM reviews against a manufacturability/quality bar → adjusts placement/constraints → re-route. The aesthetics spec's visual gate, automated. |

The router itself is pure geometry. No LLM coordinates, ever.

## Engine architecture (new `pcb-engine` crate)

```
RouteProblem (SimpleRouteJson-compatible: layers, design rules,
              obstacles/pads, nets, board bounds)
   │
   ├─ GLOBAL ROUTING ─────────────────────────────────────────
   │   CapacityMesh    quadtree subdivision; node capacity =
   │                   region size ÷ (trace width + clearance);
   │                   obstacles cut capacity
   │   Node graph      adjacency between mesh cells
   │   CapacityPathing A* per net over cells, congestion-costed,
   │                   rip-up & reroute on capacity overflow
   │
   ├─ DETAILED ROUTING ───────────────────────────────────────
   │   Segment→Point   distribute each net's crossings across
   │                   shared cell boundaries (no intra-cell clash)
   │   High-density    fine A* for hotspot cells (dense pin fields)
   │   Via insertion   on layer transitions between cells
   │
   └─ Stitch + simplify  →  copper polylines per layer + vias
                            →  DRC lint (strict oracle)
```

Maps 1:1 onto tscircuit-autorouter's stages (CapacityMeshSolver → node/edge
solvers → CapacityPathing → segment-to-point → high-density), the reference
to port from (MIT-licensed). The naive grid A* (slice 1) stays as the
always-correct fallback path forever.

### Data model decisions

- **Units mm, y-down** — KiCAD PCB native. Internally `f64`, like sch-engine.
- **Fixture JSON is serde-compatible with tscircuit `SimpleRouteJson`**
  (`layerCount`, `minTraceWidth`, `obstacles[]`, `connections[]`, `bounds`,
  camelCase) so the archived `tscircuit/autorouting` benchmark dataset and
  tscircuit-autorouter fixtures parse natively. Our extensions (clearance,
  via size/drill, net classes) are optional fields with defaults.
- **Obstacles carry net attribution** (`connectedTo`) — a pad is never an
  obstacle to its own net.
- **Pads normalize to rect/circle primitives at parse time** (roundrect,
  oval → conservative bounding primitive in v1); the DRC lint, not the
  router, is the precision authority.

## Crate layout

```
crates/
  pcb-engine/     # RouteProblem model, routers, DRC lint, SVG debug render
                  #   — PURE, no I/O beyond serde (mirrors sch-engine)
  kicad-bridge/   # extends: .kicad_pcb read (footprints, pads, nets,
                  #   outline, layers → RouteProblem) and trace/via
                  #   write-back via kiutils_kicad (pcb.rs, verified complete:
                  #   PcbPad/PcbFootprint/PcbSegment/PcbVia/PcbZone/PcbSetup)
```

## Oracle strategy (three layers)

1. **In-house DRC lint (the strict gate, always on):** clearance between
   all copper pairs of different nets, trace width ≥ min, copper in-bounds,
   and **net connectivity** (each connection's points joined by the emitted
   copper graph; no cross-net merges — the PCB analog of the schematic
   injectivity oracle). A violation must fail a test, never pass silently.
2. **`kicad-cli pcb drc` (external authority, version-gated KiCAD ≥ 8):**
   run on every `.kicad_pcb` we emit; skipped (visibly) on older KiCAD.
3. **tscircuit benchmark dataset (external benchmark):** solve-rate,
   via count, total wirelength on their problem set. Frozen upstream
   (repo archived 2025). **Slice-0 finding:** the archived repo ships no
   raw SimpleRouteJson files (its datasets are generated, and the checked-in
   fixtures are circuit-json envelopes), so the parser is locked to the
   documented shape via the hand-authored
   `crates/pcb-engine/fixtures/tscircuit-shape.json` instead. Benchmark
   problems must be generated via their tooling (or converted) when slice
   2–3 needs them — a small converter is acceptable; treat upstream format
   docs as the contract.

## Build order (slices, harness green at each)

| Slice | Deliverable | Acceptance gate |
|---|---|---|
| 0 — Model & I/O | `pcb-engine` crate + `RouteProblem`/`RouteSolution`; JSON fixtures (SimpleRouteJson-compatible); kicad-bridge `.kicad_pcb` parse + trace/via emit | Round-trip a placed board fixture; connectivity oracle scaffolding; parse a vendored tscircuit problem |
| 1 — Naive baseline | Sequential per-net grid A*, 2 layers + via cost; obstacle inflation by clearance; DRC lint; SVG debug render | Small boards (LED+R, handful of nets) fully routed, zero DRC violations, deterministic |
| 2 — Capacity-mesh global | Quadtree mesh, capacity model, congestion-costed A*, rip-up & reroute | Boards that defeat slice 1 get a feasible global plan; congestion reported |
| 3 — Detailed routing | Segment→point crossing assignment, high-density hotspot solver, via insertion, stitch + simplify | DRC-clean copper on medium boards; via count + wirelength reported vs benchmark |
| 4 — Placement (LLM lever) | Deterministic legalizer + force-directed seed; LLM proposes groupings/regions. **Prerequisite:** footprint index in kicad-bridge (analog of `symlib.rs`) + footprint-bearing fixtures | Route success rate ↑ vs fixed placement |
| 5 — Agent integration | Constraint authoring, failure provenance, vision critique loop, triage rip-up; new agent tools | Agent closes a board it failed first-pass by moving a part / relaxing a rule |

**Slice-1 findings (2026-06-12, gate passed):**

- Design constants as planned (pitch `max(0.1, (w_min + clearance)/2)` —
  0.225 mm on the fixtures; via 25 steps, bend 2, inflation
  `clearance + w_min/2`) **plus one addition**: marking only routed *cells*
  as a net's copper lets foreign nets route one pitch away, which the exact
  lint flags. The router therefore marks a Chebyshev **clearance halo**
  (radius `ceil((w_min + clearance)/pitch)` cells, net-owned so the net
  itself passes through freely) around every routed cell.
- **Oracle blind spot found & fixed:** the router briefly emitted
  bottom-layer traces as `inner1`; both in-house oracles passed (vias join
  all layers, clearance checks are layer-name-gated, so a phantom layer
  never collides). Caught by eyeballing the SVG render. Lesson: copper
  oracles don't validate layer *names* — a `LayerRef::index`-validity check
  belongs in the lint (slice 2 candidate); regression test added meanwhile.
- E2E on `two_res.kicad_pcb` against kicad-cli 10.0.3: in-house lint and
  KiCAD DRC **agree** — zero violations, zero unconnected. Only
  warning-severity `lib_footprint_mismatch` bookkeeping entries appear
  (fixture's inline footprints vs installed lib; pre-existing, copper-
  independent, documented carve-out in the e2e test).

## Reuse from this repo

- `sch-engine/route.rs`: Pt/Path model, collision-repair shape, `simplify()`,
  MST — scaffolding patterns for the detailed router (copy/adapt, don't
  couple the crates).
- `kicad-bridge`: kiutils plumbing, env discovery, snapshot, render path.
- The slice + spec/plan + strict-oracle workflow itself.

## Non-goals (v1) — explicit decisions, not omissions

- **No copper pour generation.** Real 2-layer boards route GND as a pour +
  stitching vias; v1 routes GND as traces and **parses** existing zones as
  obstacles only. Pours are the headline v2 item; benchmark comparisons
  against pour-based boards are noted as skewed until then.
- No length matching / diff-pair phase control (constraint *vocabulary*
  reserves the slots; the router ignores them in v1).
- No arc/any-angle routing; rectilinear in slice 1, 45° from slice 3.
- No interactive push-and-shove (candidate v2 swap for the detailed stage).
- No autoplacement of footprints from schematic in slices 0–3 (fixtures are
  pre-placed).

## Risks

- **Capacity model mis-tuned** → over/under-subdivision. Tune subdivision
  depth on the benchmark set (tscircuit's known pain point).
- **High-density regions** (BGA/fine-pitch) are where naive routers die —
  slice 3's hotspot solver is the hard part; slice 1's grid A* remains the
  always-correct fallback.
- **Placement dominates** — don't judge the engine on fixed bad placements;
  slice 4 exists for this reason.
- **KiCAD 7 on the dev box** (no `pcb drc`, no `sch erc` — `cli_erc` test
  fails today). Upgrade in progress; all external-oracle tests version-gate
  on `KicadEnv::cli_version` and skip visibly below 8.
- **Scope** — 6 slices. Slices 0–1 alone give a real, DRC-checked,
  end-to-end router (naive but honest), de-risking everything after.
