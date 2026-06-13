# PCB Engine Slice 5: Agent Integration — Implementation Plan

> **For agentic workers:** execute task-by-task with subagents,
> SEQUENTIALLY (shared cargo target dir), staying on `main`. Steps use
> checkbox (`- [ ]`) syntax for tracking.

**Goal:** The agent can drive the PCB engine end-to-end: search
footprints, declare a board (parts + nets), place (with LLM hints), route
(with rich failure provenance), SEE the board (vision render), author
constraints (rules + keepouts), triage failures by moving parts or
relaxing rules, and export a DRC-clean `.kicad_pcb`.

**Spec gate:** "Agent closes a board it failed first-pass by moving a
part / relaxing a rule" — proven deterministically with the mock-loop
harness (scripted triage), plus a creds-gated live smoke.

**Spec:** `docs/superpowers/specs/2026-06-12-pcb-engine-design.md`
**Depends on:** slices 0–4 (`route_auto`, `placement`, `footlib`,
`placefp`, lint, congestion reports), agent crate house patterns.

**Locked design decisions (main loop):**
- New tools live in `crates/agent/src/tools_pcb.rs`, merged into
  `Tools::defs()`/`run()` (tools.rs stays the schematic file). Same
  conventions: `ToolDef` JSON schemas, free `fn(input, ctx) -> Result<Value>`,
  `require_str`-style arg handling, errors as `{"error": …, "suggestions": …}`
  values (recoverable) vs `bail!` (programmer error).
- Board state follows the DRAFT pattern: a `BoardDraft` JSON persisted in
  the workspace (`.autopcb/board.json`): parts (reference, footprint
  lib_id, per-pad nets, optional locked position/rotation), board bounds,
  design rules (clearance, min_trace_width, via sizes), keepouts, hints,
  and the last placement. Tools mutate the draft; `place_board`/
  `route_board` read it. The LLM NEVER emits trace coordinates; placement
  positions enter only via `move_part` (a triage lever, snapped/legalized
  by the engine on the next place/route).
- Constraint vocabulary v1 = what the engine honors TODAY: board-level
  design rules + rectangular keepouts (layers). Net classes get schema
  slots but `set_constraints` REJECTS them with "not yet supported by the
  router" (honest, reserves the vocabulary).
- `route_board` does NOT auto-place: explicit `place_board` first
  (missing placement → recoverable error telling the model what to do).
  Failure provenance: per-net reasons verbatim from the engine
  (`global:`/`assign:`/`cell N:`/`finisher:` + naive fallback note),
  plus congestion hotspots (top edges, loads) when the global stage ran,
  plus `RouterKind` provenance and metrics.

---

### Task 1: board draft + part/footprint tools (`tools_pcb.rs`)

- [x] `BoardDraft` (serde, in agent crate or a small module): bounds,
      rules { clearance, min_trace_width, via_diameter, via_drill },
      parts: Vec<DraftPart { reference, footprint: String lib_id,
      pad_nets: map pad# → net, locked: Option<{x,y,rotation}> }>,
      keepouts: Vec<{rect, layers}>, hints: PlacementHints (serde
      reuse from pcb-engine), last_placement: Option<Vec<Placement>>.
      Persisted at `.autopcb/board.json` via the workspace (mirror the
      schematic draft's load/save conventions exactly).
- [x] `ToolCtx`: lazy `FootprintIndex` (mirror the symbol index's
      OnceCell-or-equivalent pattern; building it scans 155 libs — do it
      once), `pcb_path()` = project_dir/<stem>.kicad_pcb next to sch_path.
- [x] Tools: `search_footprints { query, limit }` (index.search →
      `{hits: [{lib_id, pad_count}]}`); `get_footprint_info { lib_id }`
      (pads with number/offset/size/technology/layers, courtyard, bbox;
      unknown → error + suggest()); `create_board { bounds, parts,
      rules? }` (resolve every footprint via footlib + placefp::
      part_from_footprint semantics — reuse, don't duplicate, promote the
      enclosing-courtyard rule into a shared fn if needed; validate nets ≥2
      pins or warn; overwrite flag like create_design); `get_board`
      (current draft + a derived summary: part count, net count, pin
      counts per net, whether placed/routed).
- [x] Tests (crates/agent/tests/tools.rs pattern + unit tests; KiCAD-env
      gating where the index needs real libs, with the vendored-fixture
      fallback where possible): draft round-trip, create_board resolves
      vendored footprints, search/get mirror symbol-tool behavior.
      Commit: `feat(agent): board draft and footprint tools`

### Task 2: place / route / constraints / triage tools

- [x] `place_board { }` — PlaceProblem from draft (parts, rules,
      keepouts → placement no-go via region? v1: keepouts affect ROUTING
      only; document), hints from draft; run `placement::place`; persist
      placement; return { legal, hpwl, overlaps_resolved, per-part
      positions }.
- [x] `set_placement_hints { groups }` — replace draft hints (validated:
      members must be known references; unknown → recoverable error).
- [x] `set_constraints { rules?, keepouts?, net_classes? }` — rules/
      keepouts update the draft (keepouts become BLOCKED_ALL obstacles in
      to_route_problem path — extend the draft→RouteProblem conversion);
      net_classes → honest rejection (vocabulary reserved).
- [x] `move_part { reference, x, y, rotation? }` — set locked position in
      draft (the triage lever; engine legalizes on next place_board; if
      the part would sit out of bounds → recoverable error now).
      `unlock_part { reference }` to release.
- [x] `route_board { }` — needs placement (else recoverable error);
      draft → PlaceProblem → to_route_problem (+ keepout obstacles) →
      `route_auto`; persist solution summary in workspace (full
      RouteSolution JSON at `.autopcb/route.json` for export); return
      { router, failed: [{connection, reason}], metrics {wirelength,
      vias, traces}, lint_summary (count by kind — should be 0; if not,
      THAT is surfaced loudly), congestion { iterations, hotspots } when
      detailed ran }.
- [x] Tests: scripted draft → place → route on a small board (vendored
      footprint dims, no KiCAD needed if FootprintIndex is bypassed by a
      test ctx — follow detect_for_test gating); keepout actually blocks
      (route differs / fails with vs without); move_part round-trip.
      Commit: `feat(agent): place, route, constraint and triage tools`

### Task 3: vision — `render_board`

- [ ] `render_board { view? "placed"|"routed" (default routed-if-routed) }`
      — placed: `svg::render_placement`; routed: `svg::render_svg` of the
      stored solution (+ failed-net highlights already built in);
      `render::svg_to_png`, save under workspace renders, attach via
      `IMAGE_PATH_KEY` (mirror render_schematic exactly, including the
      no-board-yet recoverable error).
- [ ] Test: tools.rs-style — render after place returns ok + png path;
      image bytes are PNG.
      Commit: `feat(agent): board vision render`

### Task 4: `export_board` — agent-flow .kicad_pcb emission

- [ ] kicad-bridge: `synthesize_board(parts: &[(reference, Footprint,
      pad_nets)], placements, bounds) -> String` — text-assembled
      `.kicad_pcb` (KiCAD-9-loadable, version header like the existing
      fixtures): skeleton (paper/layers/setup/nets) + each footprint's
      `.kicad_mod` body spliced in with `(at x y rot)`, per-pad
      `(net N "name")` bindings, refs on F.Fab (slice-4 coherence
      pitfalls: silk-free, closed courtyards left as-is from the lib
      footprint, thru-hole nets-as-leaves is the AGENT's concern not the
      writer's). Edge.Cuts rect from bounds. Then `write_solution` copper
      on top. Unit-test the synthesis against `read_problem` round-trip
      (parse what we wrote; pads land where placed, nets bound).
- [ ] agent tool `export_board { path? }` — requires routed state; writes
      via synthesize_board + write_solution; runs `kicad-cli pcb drc`
      when available (env-gated; skip note otherwise) and returns the
      DRC counts in the result. Default path = ctx.pcb_path().
- [ ] E2E test (gated): create→place→route→export on the 4-part circuit
      from the slice-4 e2e; kicad DRC zero violations/unconnected
      (footprint-mismatch carve-out only).
      Commit: `feat(kicad-bridge,agent): board synthesis and export tool`

### Task 5: the gate — triage loop + prompt

- [ ] System prompt (agent.rs): add the PCB workflow section mirroring
      the schematic workflow docs: search → create_board → hints →
      place → render(look!) → route → triage (read failure reasons; move
      parts or relax rules; NEVER invent coordinates except move_part
      nudges informed by render + part positions from place_board output)
      → export. Include the failure-provenance cheat-sheet (global/
      assign/cell/finisher meanings).
- [ ] Gate test (mock-loop harness, tests/loop_mock.rs pattern):
      scripted conversation on a board crafted to fail first pass —
      e.g. draft with a keepout wall + parts locked on opposite sides
      (reuse congested.json's defeat idea at draft level) so route_board
      reports failed nets; script then calls move_part (or removes the
      keepout via set_constraints) + place_board + route_board → 0
      failed. Asserts the full tool sequence works and the final state is
      clean — the spec gate, deterministic.
- [ ] Live smoke (creds-gated like llm_smoke.rs): short real-LLM run on
      the same scenario with max-turns cap; assert it reaches 0 failed OR
      skip visibly without creds. Tolerant assertions (the gate is the
      mock test; this is a reality probe).
      Commit: `feat(agent): pcb triage loop — slice 5 gate`

### Task 6: wrap-up (inline, main loop)

- [ ] `cargo test --workspace` green; clippy clean on touched crates;
      render + eyeball; spec slice-5 row + findings; the spec's build-
      order table is now fully delivered — note v2 backlog (pours,
      detailed rip-up, net classes, auto-rotation, benchmark dataset);
      memory update.
      Commit: `chore(pcb-engine): slice 5 wrap-up`

## Self-review notes

- The mock-loop gate keeps the spec promise testable forever without
  creds or model nondeterminism; the live smoke is a canary, not the gate.
- Tool results stay JSON-small: full solutions live in the workspace,
  summaries go to the model (mirror of the draft pattern). render_board
  is the model's eyes; route_board's lint_summary must be 0 — a non-zero
  count reaching the model means an engine bug escaped the oracles, and
  the tool says so explicitly.
- set_constraints' net-class rejection is deliberate vocabulary
  reservation — the schema documents the future without lying about the
  present.
- synthesize_board is the riskiest piece (text-assembled board); its
  oracle is read_problem round-trip + kicad DRC, both already in place.
