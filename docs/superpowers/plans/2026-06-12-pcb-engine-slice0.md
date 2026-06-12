# PCB Engine Slice 0: Model & I/O — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A `pcb-engine` crate with the `RouteProblem`/`RouteSolution` data
model (SimpleRouteJson-compatible JSON), a connectivity oracle, and
kicad-bridge `.kicad_pcb` read/write — so there is something to route and
lint against before any router exists.

**Spec:** `docs/superpowers/specs/2026-06-12-pcb-engine-design.md`

**Verified integration facts (read before coding):**

- `kiutils_kicad` 0.3 (`~/.cargo/registry/src/*/kiutils_kicad-0.3.0/src/pcb.rs`):
  `PcbFile::read(path) -> PcbDocument`; `doc.ast()` exposes typed
  `layers/nets/footprints/segments/vias/zones/graphics/setup`. `PcbFootprint`
  has `lib_id, layer, at: [f64;2], rotation, pads: Vec<PcbPad>, reference`.
  `PcbPad` has `number, pad_type ("smd"/"thru_hole"/"np_thru_hole"), shape,
  at (RELATIVE to footprint origin), rotation, size, layers, net:
  Option<PcbPadNet{code,name}>, drill, clearance`. `PcbGraphic.token` is e.g.
  `"gr_line"/"gr_rect"/"gr_arc"` with `layer` (board outline = `"Edge.Cuts"`).
- **KiCAD pad rotation convention:** the `rotation` stored on a pad in
  `.kicad_pcb` is the TOTAL rotation (footprint rotation already folded in);
  pad `at` offsets are in the footprint's UNROTATED frame and must be rotated
  by the footprint angle before adding the footprint position. KiCAD y-axis
  points DOWN; angles in the file are counter-clockwise positive.
- **`PcbDocument` cannot append segments/vias:** `ast_mut()` changes are
  rejected at `write()`; the only typed setters are title-block/properties.
  House precedent (`sch-engine/src/emit.rs` `SchematicWriter`): render
  s-expression TEXT ourselves and use kiutils only to validate the result
  parses. Write-back = splice `(segment …)`/`(via …)` text before the file's
  final closing paren, then `PcbFile::read` the result and assert counts +
  no new `diagnostics()`.
- `SimpleRouteJson` (tscircuit, MIT; repo `tscircuit/autorouting` archived —
  treat as frozen): camelCase; fields `layerCount: number`,
  `minTraceWidth: number`, `obstacles: [{type: "rect", layers: ["top"…],
  center: {x,y}, width, height, connectedTo: [connName…]}]`,
  `connections: [{name, pointsToConnect: [{x, y, layer}]}]`,
  `bounds: {minX, maxX, minY, maxY}`. Layer names are strings
  ("top"/"bottom"); units are mm.
- Workspace conventions: edition 2024, `workspace = true` deps, tests in
  `crates/<c>/tests/`, pure crates take no I/O deps (`sch-engine` pattern;
  serde/serde_json are fine). Error enums via `thiserror`.

---

### Task 1: crate scaffold + data model (`problem.rs`)

**Files:** `crates/pcb-engine/Cargo.toml` (members glob picks it up),
`src/lib.rs`, `src/problem.rs`.

- [ ] Deps: `serde` (derive), `serde_json`, `thiserror` — all workspace.
- [ ] Model, serde-compatible with SimpleRouteJson (`#[serde(rename_all =
      "camelCase")]`, extensions `#[serde(default)]`):

```rust
pub struct RouteProblem {
    pub layer_count: u32,
    pub min_trace_width: f64,
    pub obstacles: Vec<Obstacle>,
    pub connections: Vec<Connection>,
    pub bounds: Bounds,
    // ---- extensions (defaulted, absent from upstream fixtures) ----
    pub clearance: f64,          // default 0.2 mm
    pub via_diameter: f64,       // default 0.6 mm
    pub via_drill: f64,          // default 0.3 mm
}
pub struct Obstacle {            // type: "rect" | "oval" (treat oval as rect v1)
    pub kind: String,            // #[serde(rename = "type")]
    pub layers: Vec<LayerRef>,
    pub center: Point2,          // {x, y}
    pub width: f64, pub height: f64,
    pub connected_to: Vec<String>,  // connection names this copper belongs to
}
pub struct Connection { pub name: String, pub points_to_connect: Vec<RoutePoint> }
pub struct RoutePoint { pub x: f64, pub y: f64, pub layer: LayerRef }
pub struct Bounds { pub min_x: f64, pub max_x: f64, pub min_y: f64, pub max_y: f64 }
```

  `LayerRef` = newtype over `String` with helpers `top()/bottom()/index(layer_count)`
  (upstream uses string names; keep them, map to indices at solve time).
- [ ] `RouteSolution`: `traces: Vec<Trace { connection: String, layer: LayerRef,
      width: f64, path: Vec<Point2> }>`, `vias: Vec<Via { connection, at: Point2,
      diameter, drill }>`. Serde camelCase too (our own format).
- [ ] Tests (`tests/problem.rs`): parse a verbatim upstream-shaped JSON string
      (no extension fields) → defaults applied; serialize → parse round-trip
      equality; unknown fields REJECTED on our solution type but TOLERATED on
      `RouteProblem` (upstream files carry extras we don't model).
- [ ] `cargo test -p pcb-engine` green.
      Commit: `feat(pcb-engine): RouteProblem model, SimpleRouteJson-compatible`

### Task 2: JSON fixtures

**Files:** `crates/pcb-engine/fixtures/*.json`.

- [ ] `led-r.json` — hand-authored: 2 layers, one 2-point connection between
      two SMD pads plus a GND connection, a couple of foreign-net obstacle
      pads in the way. Small (≤ 30×20 mm).
- [ ] `quad.json` — ~6 connections crossing each other so naive routing must
      use both layers (forces vias in slice 1).
- [ ] `tscircuit-sample.json` — try fetching one real problem from the
      archived dataset (`github.com/tscircuit/autorouting`, raw files); if
      network/dataset shape doesn't cooperate, hand-author one matching the
      documented format EXACTLY (upstream field set only, no extension keys)
      and name it `tscircuit-shape.json` instead — the point is locking the
      parser to the upstream shape.
- [ ] Fixture-loading test asserting each parses and passes basic sanity
      (bounds non-empty, points inside bounds, referenced layers < layerCount).
      Commit: `test(pcb-engine): routing problem fixtures`

### Task 3: connectivity oracle (`connectivity.rs`)

The PCB analog of the schematic injectivity oracle — needed BEFORE any
router so slice 1 has its gate ready.

- [ ] `pub fn check(problem: &RouteProblem, solution: &RouteSolution) -> Vec<Violation>`
      with `Violation` enum: `Unconnected { connection, point_index }`,
      `CrossNetMerge { a, b }` (+ Display).
      Geometry: union-find over copper elements — a trace segment touches a
      point/pad if distance ≤ width/2 + ε on the same layer; vias join all
      layers at their position; obstacles with `connected_to` count as that
      connection's copper (pads). Two different connections' element sets
      sharing a union-find root = `CrossNetMerge`.
- [ ] Tests: hand-built tiny solutions — fully connected → empty; missing
      segment → `Unconnected`; trace touching a foreign pad → `CrossNetMerge`;
      via joining layers makes a cross-layer connection count as connected.
      Commit: `feat(pcb-engine): copper connectivity oracle`

### Task 4: kicad-bridge `.kicad_pcb` read (`pcb.rs`)

**Files:** `crates/kicad-bridge/src/pcb.rs` (+ `pub mod pcb;` in lib.rs),
`crates/kicad-bridge/tests/fixtures/two_res.kicad_pcb`,
`crates/kicad-bridge/tests/pcb_roundtrip.rs`. Add `pcb-engine` as a
dependency of kicad-bridge (it is pure; the dependency direction matches
`sch-engine → kicad-bridge` being forbidden — bridge depends on engine).

- [ ] Author the fixture board BY HAND: KiCAD-9-format `(kicad_pcb (version
      20241229) …)` with `(general)`, 2 copper layers, `(setup)`, a `(net 0 "")
      (net 1 "GND") (net 2 "SIG")` table, an `Edge.Cuts` `gr_rect` outline,
      and two 0805-style 2-pad footprints (one rotated 90°) with pads on the
      nets. Keep it minimal but VALID — `PcbFile::read` must report zero
      diagnostics (test asserts this).
- [ ] `pub fn read_problem(path: &Path) -> io::Result<BoardProblem>` where
      `BoardProblem { problem: RouteProblem, nets: …, pad_index: … }` maps:
      nets→connections (skip net 0 / unnamed), pad absolute positions
      (rotate pad `at` by footprint rotation — see verified facts — then
      translate; y-down), pads→`connectedTo` obstacles AND
      `pointsToConnect` (pad center, layer from pad layers), existing
      segments/vias/zones→obstacles, `Edge.Cuts` bbox→bounds, `setup`/dru
      defaults→trace width & clearance extensions.
- [ ] Tests: fixture parses; expected pad absolute coordinates (assert the
      rotated footprint's pads land where hand-math says); two connections
      with 2 points each; bounds match the outline rect.
      Commit: `feat(kicad-bridge): .kicad_pcb → RouteProblem`

### Task 5: kicad-bridge trace/via write-back

- [ ] `pub fn write_solution(path: &Path, solution: &RouteSolution, nets: &…) -> io::Result<()>`:
      render `(segment (start x y) (end x y) (width w) (layer "F.Cu") (net n)
      (uuid …))` and `(via (at x y) (size s) (drill d) (layers "F.Cu" "B.Cu")
      (net n) (uuid …))` text (uuid v5 from a fixed namespace + content, like
      `sch-engine/src/ids.rs` — deterministic output); splice before the
      final `)`; layer names from `LayerRef` ("top"→"F.Cu", "bottom"→"B.Cu",
      inner i→"In{i}.Cu").
- [ ] Validate after write: `PcbFile::read` re-parses with zero NEW
      diagnostics; `ast().segments/vias` counts grew by exactly the emitted
      number; original bytes before the splice point unchanged (lossless).
- [ ] Round-trip test: read fixture → hand-build a 2-segment + 1-via
      solution → write → re-read → re-extract `RouteProblem` → the new
      copper appears as obstacles `connectedTo` the right connection; run
      the connectivity oracle on the re-read copper.
      Commit: `feat(kicad-bridge): emit traces and vias into .kicad_pcb`

### Task 6: workspace hygiene

- [ ] `cargo test --workspace` green (known pre-existing failure: `cli_erc`
      on KiCAD < 8 — if the machine's KiCAD upgrade landed it must pass; do
      NOT mask it otherwise, it's a known-env issue, leave it failing).
      `cargo clippy --workspace` no new warnings. Update spec slice-0 row if
      scope shifted.
      Commit: `chore(pcb-engine): slice 0 wrap-up`

## Self-review notes
- The oracle (Task 3) lands before the router exists — slice 1's acceptance
  gate is ready on day one, matching the strict-oracle house rule.
- Write-back avoids kiutils' missing mutators by using the repo's existing
  "render text, validate by re-parse" pattern rather than CST surgery.
- Dependency direction: `pcb-engine` stays pure; only kicad-bridge knows
  about files, mirroring `circuit-lang`/`sch-engine`.
