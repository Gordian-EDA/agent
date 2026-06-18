# PCB Engine Slice 4: Placement — Implementation Plan

> **For agentic workers:** execute task-by-task with subagents,
> SEQUENTIALLY (shared cargo target dir), staying on `main`. Steps use
> checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deterministic placement: a force-directed seed + legalizer in
pcb-engine that turns a bag of footprint-shaped parts + logical nets into
legal positions on the board, optionally steered by LLM-authored
`PlacementHints` (groups, regions, edge affinities). Placement feeds the
existing routing pipeline (`route_auto`), and the slice's gate is the
spec's: **route success goes UP vs a fixed bad placement.**

**Spec:** `docs/superpowers/specs/2026-06-12-pcb-engine-design.md`
**Depends on:** slices 0–3 (`route_auto`, lint, fixtures discipline),
footprint index in kicad-bridge (`footlib.rs`, done — `Footprint { pads,
courtyard, bbox }`).

**Locked design decisions (main loop):**
- Placement engine is PURE and deterministic, lives in pcb-engine
  (`placement.rs`). The LLM NEVER emits coordinates; it emits
  `PlacementHints` (serializable data). The engine must produce a legal
  placement with EMPTY hints (hints improve, never gate).
- v1 rotation: 0/90/180/270, taken ONLY from hints or locked parts; the
  engine does not auto-rotate (recorded non-goal; auto-rotation is a v2
  lever).
- No RNG anywhere: deterministic initial layout (parts sorted by ref on a
  grid), fixed iteration count, serialize-twice tests.

**Design constants (define once in `placement.rs`):** placement grid =
0.5 mm (legalizer snap); courtyard margin = max(clearance, 0.25 mm)
between courtyards; force iterations ≈ 200 with cooling factor 0.9 every
20 iters; spring constant on nets normalized by net pin count; repulsion
active only on (margin-inflated) courtyard overlap — short-range, not
global n-body.

---

### Task 1: placement model + engine (`placement.rs` in pcb-engine)

- [x] Model (all serde, camelCase, deny_unknown_fields, like problem.rs):
      `PlaceProblem { bounds, clearance, parts: Vec<Part>, nets:
      Vec<LogicalNet> }`; `Part { reference, courtyard: Rect-like (w,h
      around origin), pads: Vec<PartPad { number, offset, size, layer(s),
      net: Option<String> }>, locked: Option<LockedAt { at, rotation }> }`;
      `LogicalNet { name, pins: Vec<(reference, pad_number)> }` (pins
      resolve to pad offsets at runtime — net membership may also be given
      directly on pads; keep ONE canonical source: pads carry net names,
      LogicalNet is derived — pick the simpler and document it).
      `PlacementHints { groups: Vec<GroupHint { name, members, region:
      Option<Rect>, edge: Option<Edge {N|S|E|W}> }> }` — empty hints must
      be valid.
- [x] `pub fn place(problem, hints) -> PlaceResult { placements:
      Vec<Placement { reference, at, rotation }>, legal: bool, report:
      PlaceReport { overlaps_resolved, out_of_bounds_clamps, hpwl } }`:
      deterministic force-directed seed (net springs toward connected
      centroid, short-range courtyard repulsion, group cohesion springs,
      region containment pull, edge affinity pull, bounds clamp) then
      legalizer (snap to placement grid; resolve residual courtyard
      overlaps by deterministic spiral search for the nearest legal cell,
      parts processed area-descending; clamp in bounds). Locked parts
      never move. `legal` true ⇔ no courtyard overlap (with margin) and
      all in bounds — verify by exact geometry at the end, not by trusting
      the algorithm.
- [x] `pub fn to_route_problem(problem, placements) -> RouteProblem`:
      pads at placed+rotated positions become net-attributed obstacles +
      `connections` (multi-pin nets → points_to_connect), board bounds
      carried over, design rules from a `DesignRules`-ish field or
      defaults consistent with existing fixtures. This is the
      placement→routing handoff and must produce problems the slice-3
      pipeline accepts unchanged.
- [x] HPWL metric (half-perimeter wirelength over net bounding boxes) in
      the report — the cheap placement-quality number.
- [x] Tests: empty hints on a small problem → legal, deterministic
      (serialize twice); locked part doesn't move; two connected parts end
      closer than two unconnected ones; group with region hint lands its
      members inside the region; edge-affinity part touches its edge band;
      overlap resolution (start everything at one point → legal, no
      overlaps); to_route_problem produces a parseable RouteProblem whose
      connectivity oracle accepts pad geometry (pads on nets, points on
      pads).
      Commit: `feat(pcb-engine): force-directed placement with legalizer`

### Task 2: placement fixtures + the slice gate

- [x] Fixture `fixtures/place-charger.json` (or similar small board, ~8–14
      parts): hand-authored PlaceProblem using REAL footprint dimensions
      (crib courtyard/pad numbers from kicad-bridge's vendored fixture
      footprints: R_0603, SOT-23, PinHeader_1x02 — copy the numbers, the
      JSON stays pure pcb-engine). Include a connector (edge-affinity
      candidate) and a few decoupling-style 2-pin parts.
- [x] Fixture `fixtures/place-charger-fixed.json`: the SAME parts with a
      deliberately bad FIXED placement (all locked) that routes poorly:
      iterate until `route_auto(to_route_problem(fixed))` has ≥ 1 failed
      net OR ≥ 2× the HPWL (prefer failed nets — the spec gate is success
      RATE; tune geometry like the congested fixture until the defeat is
      structural, e.g. connected pins on opposite corners with blockers
      between).
- [x] Gate test `tests/placement_gate.rs`: (i) engine placement (empty
      hints) on place-charger → legal; route_auto → 0 failed nets, lint
      EMPTY; (ii) fixed placement → the defeat asserted (failed nets or
      the documented HPWL/wirelength regression — assert what is true,
      with the investigate-don't-tweak comment); (iii) hinted placement
      (author hints in the test: group decouplers with their part, edge
      hint for the connector) → legal, routes clean, and HPWL ≤ the
      unhinted run (hints must not hurt; assert ≤ with small epsilon,
      record actuals in comments).
- [x] `render_placement` in svg.rs: board, courtyards (outline + ref text),
      pads colored by net, region hint rects dashed; add placement
      fixtures to the render-all helper (place → render placed state;
      also render the routed result via existing render_svg). EYEBALL.
      Commit: `feat(pcb-engine): placement fixtures and routing-uplift gate`

### Task 3: kicad-bridge e2e — placed board to DRC-clean .kicad_pcb

- [x] `kicad-bridge`: build a `PlaceProblem` from real footprints via
      `footlib` (`part_from_footprint(footprint, reference, net_map) ->
      Part` — courtyard + pads with nets), and write a placed+routed board
      out. For writing, PREFER the simpler path: start from a template
      `.kicad_pcb` containing the footprints (author a small fixture board
      in-test or check one in), MOVE footprints to placed positions
      (update `at`/rotation via kiutils), then `write_solution` the routed
      copper. Full footprint construction from scratch is acceptable if
      kiutils makes it easy — author's call; document which path was taken.
      DONE (`src/placefp.rs`): TEMPLATE+MOVE path — `placed_template.kicad_pcb`
      carries the 3 footprint types with nets; `move_footprints` rewrites each
      footprint's `(at …)` by text-splice (kiutils footprint edits don't
      round-trip through `write()`, same limitation `pcb::write_solution`
      documents). Courtyard-enclosing rule: origin-SYMMETRIC courtyard whose
      half-extent on each axis = max |coord| over BOTH the `F.CrtYd` and the pad
      bbox — guarantees it encloses the pads even when KiCAD's courtyard is
      asymmetric (e.g. PinHeader y -1.77..4.32) or under-covers them.
- [x] E2E test (version/install-gated like the others): footlib parts →
      place (empty hints) → to_route_problem → route_auto → 0 failed →
      write placed+routed board → `kicad-cli pcb drc` → zero violations,
      zero unconnected (same lib_footprint_mismatch carve-out if it
      appears); in-house lint clean on the same solution.
      DONE (`tests/placed_board_e2e.rs`): J1(PinHeader thru-hole)+U1(SOT-23)
      +R1+R2(R_0603); nets VIN{J1.1,U1.1} GND{4} VOUT{U1.3,R1.1,R2.1}. KiCAD
      DRC: 0 copper violations, 0 unconnected, 4 tolerated lib_footprint_mismatch.
      COHERENCE FINDING for slice-5 board auto-gen: keep thru-hole pads OFF the
      interior of multi-pin nets — a 3-pin VIN through J1's drilled pad forces a
      bottom-layer detour that vias UP at the pad → KiCAD `hole_to_hole`. Also
      author template silk-free (Ref/Value on F.Fab) so the compact empty-hints
      cluster never trips `silk_over_copper`; courtyards must be CLOSED shapes
      (a closed `fp_rect`, not disjoint `fp_line`s) or DRC flags
      `malformed_courtyard`.
      Commit: `feat(kicad-bridge): placed-board e2e through kicad drc`

### Task 4: wrap-up (inline, main loop)

- [x] `cargo test --workspace` green; clippy clean on touched crates;
      render + EYEBALL placement PNGs (placed state must look like a sane
      board: connector at edge if hinted, no overlaps, decouplers near
      their parts); spec slice-4 row + findings; memory update.
      Commit: `chore(pcb-engine): slice 4 wrap-up`
      (521 workspace tests, 0 failures. PNGs eyeballed: legal cluster, no
      overlaps, routed clean; empty-hints corner bias noted in spec
      findings along with the courtyard invariant and template-coherence
      pitfalls.)

## Self-review notes

- The gate is honest only if the fixed-placement defeat is structural
  (like congested.json's wall) rather than a tuned accident — prefer
  failed nets over metric deltas, and pin the defeat with the
  tighten-don't-delete rule.
- `legal` is verified by exact geometry at the end (the placement analog
  of the lint), never assumed from the algorithm. If the legalizer can't
  find a legal cell within the board, `legal: false` + report — never
  panic, never overlap silently.
- Hints are data, so slice 5's LLM integration is a prompt/serde problem,
  not an engine change. If hint vocabulary needs growing later (net
  classes, keepouts), the serde shape leaves room (optional fields).
- to_route_problem is the contract seam: slice-3's pipeline must accept
  its output unchanged — any impedance mismatch found there is a real
  finding about the models, fix at the source.
