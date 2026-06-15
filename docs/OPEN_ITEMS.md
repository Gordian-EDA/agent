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

**Done earlier this session:** `layout:` 2D grid (`941c9df`), `positive:`/`negative:`
polarity + enforcement (`f956568`), `power:` list replacing `rails:`/`nets.power`
(`415a755`), an `anchor_tap` fix, and the **placement-search refactor** (one seeded
search over mm geometry; `PlacementStrategy` Greedy/Anneal; SA picks fewest-warnings
candidate → anneal 7/7 on the fast fixtures).

**Done in the latest push:**
- **ALL 10 FIXTURES ELECTRICALLY TRUTHFUL** (`3dc7ab1`) — `KNOWN_TRUTHFULNESS_BUGS`
  is empty. The last two (bga GND/1V2, selfrepair VBUS/3V3) were collinear rail
  risers merging; `plan_riser_offsets` fans them apart (finalize-only repair).
- **OpenAI-compatible backend** (`18b76e7`) — `OpenAiClient`; default switched off
  Bedrock to the respan.ai gateway (`anthropic/claude-opus-4-8`). Verified live.
- **Polish-aware SA candidate pick** (`3da87a2`) — judge each candidate THROUGH
  polish+decongest (was: raw state), so the SA never ships > greedy. oneshot 4→1.
- **Pin-budget anneal cap** (`32a6a0f`) — bound `iters * pins` so big-board SA is
  practical (oneshot 3m47s→2m21s); tuned fixtures (≤58 pins) untouched.
- **Agent e2e validated** — 9 diverse OpenAI-driven scenarios (LDO, op-amp, I2C,
  Sallen-Key, 555, BJT relay driver, buck, Pierce crystal, thermistor) ALL emit
  **0 layout-warnings + 0 ERC errors** through the SA engine. The "diff scenarios"
  bar is met for realistic circuits.

**Remaining for literal "0-warnings on all 10":** only the 3 deliberately-extreme
fixtures — `bedrock-oneshot` (anneal **1**), `bedrock-selfrepair` (265 pins),
`bga-fpga-ice40` (671 pins). Dominant class: net-label vs pin-text collisions on
dense connectors + multi-unit/decoupling clustering. The SA reduces these a lot
(oneshot greedy 21 → anneal 1) but literal-0 on a 671-pin BGA needs deeper work
(faster incremental cost for more SA iterations, and/or a dense-connector label
strategy). Also open: the INFER-quality items below (§2 + §5), `ports:` author
section, engine cleanup (§1.2).

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
- **KEEP the SA (`anneal_cells`)** — an earlier "retire anneal" recommendation
  here was wrong and the removal was reverted. The "SA never wins" measurement
  was on the hand-tuned references (no room to improve); on the INFER frames that
  are now the production default, a global escape from local minima is exactly
  what's needed. The real task (their #20): make a *properly built* SA win —
  price the cost's blind spots (so ugly==expensive) and fold align/compact/
  decongest into one cost-scored search — likely making SA the default. See the
  `keep-the-sa-optimizer` memory.

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

- **#21 / `bga-fpga-ice40`** — SHARPENED DIAGNOSIS (2026-06-15): GND does NOT
  fragment — it **merges/shorts into 1V2**. In the exported netlist there is *no*
  GND net at all; `1V2` carries 48 nodes (GND's pins folded in). So a GND rail
  wire/riser collides with a 1V2 element. (`emit_rail` + `split_wires_at_nodes`
  are each correct in isolation; the "3 bare #PWR" are power lib-symbol DEFAULTS,
  a false alarm.) The search's cost (`count_merges`/`count_shorts`/`foreign_taps`)
  does NOT catch this multi-unit GND→1V2 collision — that's the blind spot to
  fix: price it, OR make `emit_rail` route GND/1V2 risers so they can't touch.
  Iterating needs the slow bga (use `NO_REFINE=1 layout_spike` to render it fast,
  then export the netlist and check `1V2` node count vs a real GND net).
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

---

## 6. Placement-search refactor — findings (2026-06-15)

The mm-flip (search optimises real geometry) made the **SA decisively beat greedy**
on the hard fixtures: mixed-signal 2→0, bedrock-oneshot 21→0 layout warnings.
The SA is the quality tier and it works.

- **IC value/MPN text reservation in `item_rect` is net-negative — do NOT add it.**
  Reserving the IC's MPN band fixes rf-lna (ADL5542-over-cap, greedy 2→0) but
  perturbs GREEDY on multi-IC sheets into worse minima (mixed-signal 2→14,
  bedrock 21→25). The SA handles the bigger ICs fine. The right home for IC field
  text is a **cost term the SA optimises** or smarter MPN placement in
  `solve_text_positions` — NOT the overlap rect that every greedy move is gated on.
- **`boxes_overlap` needed an EPS** (fixed): a sub-micron float edge-touch between
  padded bboxes was a phantom overlap warning (collinear R7/R8; pull-up above an IC).
- **Snapshot baselines were `*.kicad_sch`-gitignored** (fixed): force-tracked under
  `crates/sch-layout/tests/snapshots/` so the gate is durable.
- **Still to reach "perfect on all 10":** drive the remaining INFER label/text
  overlaps to 0 (bedrock/mixed via the SA + targeted `solve_text_positions`), and
  the 2 tracked truthfulness bugs (`#21` bga GND fragments, selfrepair rail short).

### 6.1 Scoreboard (`sa_e2e`, fast fixtures, 2026-06-15)

```
fixture                  greedy  anneal
divider-filter              0       0
mcp1703-power-entry         0       0
555-blinker                 0       0
uart-level-translator       0       1   <- SA REGRESSED
grid-demo                   0       0
rf-lna-frontend             2       2
mixed-signal-adc-frontend   2       0   <- SA fixed
```

**Key finding — the SA needs a WARNING-AWARE cost.** Greedy and SA share the same
`layout_cost` today (PremiumCost not built). So the SA's global search *exploits
the cost's blind spots*: it found a uart layout that's cost-cheaper but has a
label overlap the cost doesn't price (0→1), while fixing mixed-signal (2→0). To
make the SA strictly ≥ greedy and reach "perfect on all 10", the cost must price
the layout-warning classes (label-over-symbol, value-over-symbol) the SA can
currently exploit — the `PremiumCost` work (Step 7). NOT via `prepare()`-per-eval
(too slow); via a cheap approximation of those overlap classes on the
un-prepared writer.

**SA-speed is now a real blocker**, not just deferred: the SA on a 100-pin
(bedrock) / 121-ball (bga) board runs many minutes (thousands of build+route
evals), so it can't even be scoreboarded on all 10 in reasonable time. Needs an
iteration budget scaled down for big boards, or a cheaper per-eval cost, before
"SA on all 10" is practical.

**rf-lna 2/2** — the ADL5542-MPN-over-cap: fix in `solve_text_positions` (the IC
value candidates sort by OWN pin-text overlap, not NEIGHBOR-symbol overlap, so a
long MPN with no clear spot falls onto a cap), not the placement search.

### 6.2 SA perfect on the fast fixtures (2026-06-15, `73c0f66`)

After the warning-aware SA candidate pick + far-band IC MPN candidates:

```
fixture                  greedy  anneal
divider / mcp1703 / 555     0       0
uart / grid-demo            0       0
rf-lna-frontend             0       0
mixed-signal-adc-frontend   2       0
0-warning (fast)           6/7     7/7
```

**SA is 7/7 perfect on the fast fixtures** and guaranteed ≥ greedy (the
warning-aware pick). Remaining for "perfect on all 10":
- **SA too slow on big boards** — bedrock (100-pin) / bga (121-ball) route the
  whole sheet per move, so the SA runs many minutes; can't even scoreboard them.
  Needs a per-board iteration budget or a cheaper per-eval cost.
- **The 2 truthfulness bugs** (`#21`): bga multi-unit GND fragments → shorts onto
  1V2; bedrock-selfrepair `baseline_ir` adjacent-rail short. These are RAIL-ROUTING
  correctness (the `wire`/`emit_rail` path), independent of greedy/SA. Fixing them
  + removing from `KNOWN_TRUTHFULNESS_BUGS` is the core remaining blocker.
