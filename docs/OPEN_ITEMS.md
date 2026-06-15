# Open items

Living backlog for the floorplan engine + circuit-lang. Pre-release; breaking
changes are welcome when they make things cleaner. Last swept 2026-06-15.

Validate any engine change with the harness (render `render_targets` →
`/tmp/renders/ours-*.png`, judge with fresh sub-agents vs
`docs/validation/references/*.png`, gate on the netlist oracle). See CLAUDE.md
*Visual review* and the `floorplan-validation-harness` memory.

Design docs:
- `docs/superpowers/specs/2026-06-15-circuit-lang-v2-design.md` — v2 YAML
  (`power`/`ports`/polarity/`layout:` grid).
- `docs/superpowers/specs/2026-06-15-layout-engine-grid-redesign.md` — the engine
  side of the grid (the contract, rigid refine, deletions).

## Status (2026-06-15)

**Done this session:** `layout:` 2D grid (`941c9df`), `positive:`/`negative:`
polarity + enforcement (`f956568`), `power:` list replacing `rails:`/`nets.power`
(`415a755`), and an `anchor_tap` fix so a satellite whose rail leg also lands on
its IC flanks the signal pin instead of scattering (mixed-signal blockers gone,
grid skeleton honored). All gated byte-identical on the 4 references + oracle.

**Still open:** the INFER-quality items below (§2 + §5), `ports:` author section,
engine rigid-refine/anneal cleanup (§1.2), and the tracked bugs (§3).

---

## 1. The grid redesign — biggest item, do it as one arc

The author writes a coarse 2D `layout:` grid; the engine fills in fine geometry.
The engine is **already** a grid placer (`LayoutIr.place: refdes → Cell{col,row}`,
`assign_cells`/`apply_cells`), so this is mostly *exposure + deletion*, not new
machinery. Implement as one coherent change (language + engine + fixtures):

### 1.1 circuit-lang v2 YAML
Per the v2 design doc:
- **model + parser** for `power:` (power nets as first-class symbol glyphs,
  replaces `rails:` + `nets:{power:true}`), `ports:` (`<net>: <edge>`), and the
  top-level **`layout:` 2D array** (`Vec<Vec<Option<String>>>` of block/refdes
  names; `~` = hole).
- **polarity terminals** `positive:` / `negative:` for 2-pin polarized parts →
  mapped to the symbol's anode/cathode (or `+`/`−`) pins, with the
  between-on-polarized / positive-on-symmetric **enforcement errors**.
- **delete** the old hint surface: `Edge`, `LayoutHint`, `Block.layout`,
  `Component.layout` (the per-block/part `edge` hint from `6b6e81d`/`86d5bd4`),
  and `near`. No replacement — position is the grid or declaration order.

### 1.2 Grid in the engine
Per the engine-redesign doc:
- **`grid_from_layout`** — parse `layout:` → `place` cells (col = index in row,
  row = row index); a block cell expands to its parts; a cell appearing ≥2× →
  add to a new `LayoutIr.float` set, anchor at the centroid.
- **`infer_ir` → `infer_grid`** — same inference rules, but output understood as a
  grid; **default order = block declaration order** (one left→right row), refined
  by connectivity only where it helps. **Delete `order_anchors` + `band_rank`**
  (they only existed to turn edge hints into a column order).
- **compose** authored ∪ inferred: a partial `layout:` pins some anchors;
  ungridded parts attach to their most-incident gridded anchor (adjacency rule,
  now load-bearing) or take a spare column.
- **`refine_cells` becomes grid-rigid**: gridded anchors freeze their `(col,row)`
  (orient/mirror still free); float anchors move only within their cell-span;
  ungridded satellites keep full freedom. Narrower search = faster + can't wander
  a reference into an ugly-cheap basin.
- **cost simplification**: drop `spread` + the anchor-ordering bias; keep the
  local-geometry terms (corners, body crossings, foreign taps, overlap, orient,
  spine, supply-pin pull). The grid removed the global DOF that made one cost
  whack-a-mole across circuits.
- **retire anneal**: delete `anneal_cells` + the `ANNEAL` flag (off by default,
  never wins, and a rigid grid makes global search moot).

### 1.3 Fixtures + gate
- migrate all 8 fixtures to v2 (`rails:`→`power:`, polarity rewrites, drop
  `nets.power`); the 4 references carry **no** `layout:` grid → they exercise
  `infer_grid`'s declaration-order default and **must stay byte-identical**.
- add a **grid fixture** (the J1/MCU/USB shape) asserting: gridded anchors land in
  grid order; a repeated anchor floats between its rows; ungridded caps land
  beside their anchor.

---

## 2. Inference quality — now load-bearing

The grid is sparse and the default is declaration-order, so `infer_grid` does more
of the work. The ranked plan from the gap-analysis workflow
(`tasks/wvtpl2fpe.output`, run `wf_6c827ceb-234`):

- **Adjacency placement (was `near` + Batch A3/A4)** — ungridded parts must
  auto-cluster next to the gridded anchor they wire to: decoupling caps flank the
  IC supply pin (not `spare_col`), crystals sit on the OSC pins, series passives
  along the flow. This *replaces* the `near` hint and is how block-cell expansion
  places satellites. Loosen `anchor_tap` to resolve on a single distinct **anchor**
  (kills 555 sprawl).
- **A1 ports** — promote named multi-pin signal nets to ports (divider `OUT`, uart
  `TXD1`/`RXD1`); prefer the structural signal over the name heuristic; skip
  `N$`/anonymous. (Now partly handled by the explicit `ports:` section — keep the
  inference for un-declared single-pin nets.)
- **A2** geometry-driven port side (not the `net_is_input` substring).
- **A5** decide `mirror` before satellite placement (uart A/B side).
- **F1-overlaps-field** — INFER mcp1703 warns `F1 overlaps field U1`; clear it.
- **Batch B** — re-add IC field-text reservation in `item_rect` (reverted in round
  4 for perturbing the anneal; anneal is going away, so re-validate all 4 and
  keep it).

---

## 3. Tracked correctness bugs (`KNOWN_TRUTHFULNESS_BUGS`)

The oracle's challenge tier carries these as documented-failing; it auto-fails if
one silently starts passing (remove it from the list when fixed).

- **#21 / `bga-fpga-ice40`** — many/multi-unit power pins don't all reach their
  single rail. The FPGA's GND balls span 5 units; rail routing leaves GND
  fragmented into ~12 nets, one of which shorts onto `1V2`. **Fix:** rail routing
  must connect *every* power-net pin across *all* units to its one rail. (Not a
  positional merge — units render in a non-overlapping row.)
- **`bedrock-selfrepair-bluepill`** — `baseline_ir` adjacent-rail short.

---

## 4. Cleanup / hygiene

- **`lift` `ap_*` vestige** — `crates/sch-layout/src/lift.rs` still reads the
  hidden `ap_block`/`ap_role`/`ap_parent`/`ap_index` reconciliation tags the
  modern `emit` no longer writes. Dead on modern input, but `lift` is live
  (`apply_design` diff) → focused follow-up (drop the reader or re-emit the tags).
- **Challenge oracle is slow (~9 min)** — polish is bounded (3 iters / 2
  free_nudge rounds); references still render in ~1.3 s. The grid's narrower refine
  search should *help* here — re-measure after 1.2.
- **uart clutter (review T1/T4)** — J1 `Conn_01x05` text overlaps the pin-3/4
  no-connect X markers (cheap win in `solve_text_positions`); the R13/R15
  termination cluster is busier than the human reference.
- **Pre-existing, NOT an engine bug** — `cli_netlist` fails on this KiCAD 9.0.2 env
  (old-format `rc_pair.kicad_sch` won't load). Leave as-is or regenerate.

---

## 5. Fresh sub-agent critique findings (2026-06-15)

Ranked from a parallel adversarial review of the 4 reference renders + the
INFER-path renders (grid-demo, rf-lna, mixed-signal).

**RESOLVED this session** (3 commits): the worst INFER blockers — collapsed
components, scattered timing parts (`anchor_tap` distinct-anchor, `42073c2`); the
bypass-cap pile + its vertical label-through-bodies (`same_pin` fan-out,
`fa90347`, caps fan into columns so no tall stack/mid-span label remains); and
connectors tucked under an IC's GND pin (rail-only-tap guard — a coax touching the
chip only through GND is no longer flanked there). rf-lna went from a 1900px-tall
broken column to a clean professional schematic (U1 centred, J1/J2 at the edges,
bypass caps a bottom row). mixed-signal blockers cleared.

Remaining, by impact (follow-up review of the improved renders):

- **Wires through 2-pin / connector bodies (INFER + refs — careful).** A rail
  riser can pass straight through a connector or cap body (grid-demo: the J1
  connector). `count_body_crossings` prices 2-pin axes and `count_ic_body_crossings`
  prices IC bodies, but a connector with pins in one column has a near-zero-width
  bbox that the `r[2]-r[0] < EPS` guard SKIPS — so its body isn't an obstacle.
  Fix: give a degenerate (narrow) 2-pin/connector body a minimum obstacle width
  so risers route around it. **Risk:** this changes refine/routing, which also
  produces the byte-identical reference output — validate the 4 refs don't move
  (and re-tune sidecars if they do). The single highest-value visual defect left.
- **Multi-unit op-amp placement (INFER).** A multi-unit IC's units are separate
  Items sharing a refdes; `anchor_tap` counts them as distinct anchors, and
  `AIN_B` between two MCP6002 units becomes a label-teleport instead of a short
  wire (mixed-signal). Group hits by REFDES so a part tapping one unit resolves;
  keep units of one package clustered and wire pin-to-pin on the same sheet.
- **I2C pull-up / decap rail pitch (INFER).** mixed-signal strings 6 parts along
  the 3V3 rail with closely-spaced vertical taps (near-overlapping junctions).
  Spread rail taps to ≥2-grid pitch; place pull-ups next to SCL/SDA, not
  interleaved with decoupling.
- **Vertical net labels over symbols (INFER, deferred — risky).** A long vertical
  net's label can still overprint symbols. The fix lives in `solve_text_positions`
  / `add_cluster_label` (label angle from stub `dir`), which ALSO produces the
  byte-identical reference output — so it needs care to not move reference labels.
- **Compactness gap (BOTH paths).** Every reference reviewer flagged the render
  as ~2–3× too sparse vs the human drawing (wide margins, large label-to-symbol
  gaps, full-height indicator legs). A global compaction / auto-fit-to-content
  pass would close most of the visible gap. (Sidecar references are byte-frozen,
  so this is an INFER + emit-framing change, validated against the human refs.)
- **mcp1703 / 555 series-spine alignment (reference gap).** Reviewers want the
  main power/series spine on one Y aligned to the IC pins, rail taps as vertical
  branches off it. This is the "match references / retire sidecar" track.
