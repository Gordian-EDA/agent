# Schematic-quality improvement loop — running log

Goal: Circuit YML → kicad_sch that looks **human-professional**. Target: **9+ VLM-critic
on hard dense circuits** (50–164 parts). Engine: `crates/sch-layout/src/floorplan.rs`.
Started 2026-06-21 (session loop, 30-min cadence).

## Metric / gates
- `tools/schematic_critic.py OURS.png --circuit "..."` (opus-4-8 vision; readability/routing/compactness/convention + ranked defects).
- Hard gate: `LAYOUT_SEARCH=anneal cargo test --release -p sch-layout --test floorplan_netlist` (connectivity oracle). A prettier render that breaks connectivity is a regression.
- Free (greedy) path must stay bit-identical if shared cost code is touched.

## Assets
- `~/kicad-dataset/` = 100 human triples (.json/.kicad_sch/.png). `~/kicad-scraper/dataset/` currently mirrors it (check hourly for growth).
- Dense human boards (placed≥45): 42; densest = `2b0638a2e866` (164 parts). Index: `/tmp/dataset_index.json`, dense slice `/tmp/dense_slice.json`.
- Live LLM: **gpt-5.4** is the highest GPT-5.x the respan.ai gateway serves (gpt-5.5 → 400/404, NOT available). gpt-5.4 drives the full agent tool loop OK (verified). gpt-5, opus-4-8, sonnet-4-6 also work.

## Baseline (current engine, ANNEAL=1) — 2026-06-21
Simple references (NOT consistently 9+):
- divider-filter **6/10** (R8 pulled off the divider spine onto a horizontal stub)
- mcp1703-power-entry **8/10**
- 555-blinker **5/10** (long cross-sheet detour wire; junction knot/congestion; unrelated-net crossings)
- uart-level-translator **8/10**
Dense (gpt-5.4 generated): `dense-stm32` **7/10** (read=8 rout=7 **comp=6** conv=9) — banished S1 RESET w/ long rail to NRST; long BOOT L-path; center whitespace/sprawl. Critic PRAISED the aligned decoupling row + consistent power-symbol usage.

## Diagnosis (convergent)
Recurring defect simple+dense = **a part/leg lands in the wrong spot and is joined by a long detour wire** (R8 off-spine, 555 detour, S1 banished, BOOT path).
Two roots:
1. **Search can't make large global moves.** SA relocates satellites only ±2 cells, nudges anchor-blocks only ±1 cell → a part seeded in the wrong region can never migrate home. Global arrangement is **seed-dominated**. (The `anneal_locality` fast lane has temp-scaled cluster jumps but moves whole clusters, not banished individual sats.)
2. **Labels are only a failure-fallback, penalized at 1000.** `route.rs`: "a failed route falls back to label connectivity." `signal_label_count` = fallbacks, weight 1000 in `layout_cost` (correctness wall). So the engine DRAWS wires everywhere, incl. long detours, and only labels when routing fails. **Humans deliberately label long/global nets** (critic praised exactly this). Cost is biased opposite to humans.

## Candidate levers (ranked by impact×cheapness, data-grounded)
- **A. Label-vs-wire policy** — emit a net LABEL (not a long wire) for nets whose route is long / detour / crosses congestion. Directly kills the #1 critic defect; mirrors humans; connectivity-safe (labels preserve net → oracle stays green). Risk: label soup (balance via threshold; measure with critic). **← likely first experiment.**
- **B. Global placement reach** — better seed (partition/spectral) + a "relocate satellite toward its home pin" move (temp-scaled radius) so banished parts migrate. Reduces the NEED for long connections. (Tier S1 + locality spec stage 2.)
- Tier S2 (learned cost from humans), S1 (floorplan encoding), #5 templates, #6 partitioning — pending mining results.

## In flight
- Mining workflow (run wf_5ddf5c4f-cae): 18 dense human boards → ranked layout rulebook w/ measurable proxies. Feeds S2/cost calibration + confirms label usage.
- Tasks: see TaskList (#1 baseline, #2 dense set, #3 mining, #4 cost refit, #5 floorplan enc).

## Decisions / notes
- gpt-5.5 unavailable → use gpt-5.4 for live agent design.
- Generated dense circuits saved as fixtures: `/tmp/renders/dense-*.png` + `.circuit.yaml` (+ .kicad_sch) for reproducibility.

## Iteration 1 — Lever A: signal-label distribution (2026-06-21) ✅ landed (uncommitted)
Implemented in `floorplan.rs`: `route_signal` skips MST hops > `SIGNAL_LABEL_SPAN` (50mm) so
the label-bridge names each side → long cross-sheet wire becomes a net-label pair (human
idiom). **Finalize-only** (`fan_risers`) + **board-gated** (`pin_total > FAST_PINS`) so the
per-move SA scorer + all ≤34-pin references stay byte-identical. Env override
`SIGNAL_LABEL_SPAN_MM` for A/B + sweeps (set huge to disable). Mirrors `rail_should_distribute`.

Results (gpt-5.4 dense battery, INFER+ANNEAL, OFF vs ON via env toggle, stable `-OFF`/`-ON` PNGs):
- **Oracle GREEN** (`LAYOUT_SEARCH=anneal floorplan_netlist`: reference + challenge fixtures ok). Connectivity safe.
- **audiocodec: `ic_crossings` 9 → 0** (Lever A removed 9 wires-through-IC-bodies — worst defect class). Hard, objective win.
- **stm32: routing dim 8 → 9** (clean A/B, same circuit; overall 8→8, critic variance dominates).
- Codec now fans pins to side-banked labels (the human "high-pin IC = label hub", rule 10).

Known issues / follow-ups:
1. **Label–symbol collisions** (stm32 NRST↔C8; audio J1↔BUF_R_IN). Lever-A labels are stub
   movables with only {stub-out, retracted} candidates; both collide on crowded pins. → give
   signal-label movables more candidate positions (L-stub / perpendicular), or fix via placement (B).
2. **Ugly auto-net-names** visible once labelled (`NET_U1_NRST` vs human `NRST`). Names come from
   the circuit/lift, not Lever A. Consider gating labels to meaningfully-named nets, or prettifying.
3. **Sprawl remains** — esp32 OFF 5/10 (blocks flung apart, decoupling stranded). Lever A fixes the
   *connection*, not the *cause*. → **Lever B (disjoint functional blocks + signal-flow placement)** next.
4. Consider lowering the small-board gate so references with detours (555 5/10, divider 6/10) benefit —
   controlled test: render the 4 refs with Lever A on, critic, keep only if no regression.

Critic variance is large (same OFF file scored 6 and 8). Use `--samples 2-3` for decisive calls;
trust engine signals (ic_crossings, body_crossings, wire-length) over single critic samples.

### Critic battery (OFF vs ON @ 50mm) — the realign
| circuit | OFF | ON@50 | note |
|---|---|---|---|
| stm32 | 8 (rout 8) | 8 (rout 9) | slight + on routing |
| esp32 | 5 | 5 (read 5→4, comp 4→3) | NEW defect: "label soup, can't trace nets" |
| audio | 6 | **5** | soup penalty > the ic_crossings 9→0 win |

**Conclusion:** blanket 50mm labelling is net-negative on SPRAWLED boards — labels only read
professional once placement is TIGHT (humans label all >50mm nets but keep blocks tight so few
qualify). The critic, not engine metrics, is the target, and it punishes soup. **Placement
(Lever B) is the dominant lever; Lever A is downstream.** → set `SIGNAL_LABEL_SPAN=90` (conservative:
only egregious cross-sheet detours, can't soup, byte-identical refs, oracle-green) and pivot to B.

### NEXT: Lever B — functional-block placement (task #6)
Partition → tight disjoint blocks (gutters, no overlap) → arrange along L-to-R signal flow. Mined
proxies: cluster ratio 0.22–0.34×parts, inter-cluster gap ≥10–15mm, gap/intra >2, directional_score
=(mean_x(out)-mean_x(in))/width >0.5. Build on `docs/specs/locality-aware-placement-search.md`
(two-level SA: cluster-relocate / cluster-swap moves already specced; the block seed is the gap).
Seed entry points: `infer_ir`, `assign_cells`, shelf packing (`floorplan.rs:296-403,490-602`).
Re-enable aggressive Lever A (drop span→50) once blocks are tight, then labels = few + organized.

## Iteration 2 (2026-06-21) — objective metrics + Lever B negative results

**New human data:** `~/kicad-scraper/dataset/` grew 100→455 boards (293 dense ≥60 parts; up to 179).

**Built `tools/layout_metrics.py`** — deterministic layout scorecard from a `.kicad_sch`
(wire-length dist, sprawl, cluster ratio/gap, label/power ratios). The low-noise complement
to the high-variance VLM critic. **This is the key deliverable of the iteration** — it lets us
measure ours-vs-human objectively instead of eyeballing.

**Human target distribution (n=455):** wire_len_median **5.1mm**, wire_frac_gt50 **~0.0**,
sprawl **~23**, cluster_ratio **0.27**, min_gap **16.5mm**, label_per_part 0.76.

**Ours (dense battery, objective):**
| circuit | wire_med | frac>50 | sprawl | cluster_ratio |
|---|---|---|---|---|
| stm32 | 3.8 | 0.0 | 63 | 0.37 |
| esp32 | 3.8 | 0.05 | 80 | 0.72 |
| audio | 3.8 | 0.05 | 37 | 0.54 |
Takeaways: **our wires are already SHORT (3.8 < human 5.1)** — local routing is good. Our
**sprawl is 2-3.5× human** and clusters are MORE fragmented (0.37-0.72 vs 0.27). bbox-sprawl is
dominated by a FEW stranded outliers (esp32 bbox_w=454mm: decoupling far-left + microSD far-right).

**Lever B attempts (both FAILED, gated behind `BLOCK_FLOW`, default OFF → zero default impact):**
- `flow_arrange_blocks` (shelf-pack blocks into gutter-separated grid): **increased** sprawl
  (stm32 63→83) — adding gutters spreads, doesn't compact.
- `compact_blocks` (centroid-pull + padded block separation): also **increased** sprawl
  (stm32 63→82); too-fine partition (cluster_ratio→0.79) + per-block gutters spread faster than
  the pull compacts; esp32 rejected (block moves create geometric shorts → truthfulness gate).
- `build_functional_blocks` (full-coverage signal partition) is kept (reusable); the metric tool too.

**THE REALIGN (important):** the engine ALREADY seeds decoupling caps beside their IC
(`infer_ir` :543-588) AND binds them in `cohesion_targets` (nearest IC supply pin) — both citing
"the #1 stranded-decoupling defect." So post-hoc block manipulation fights a well-tuned system and
loses. Sprawl is emergent from seed+SA+**cost**, and the cost TOLERATES sprawl (proxy spread weight
0.45). **The lever is the COST, not another placement pass** → pursue Tier S2: calibrate the cost
toward the human metric distribution (sprawl→23, cluster_ratio→0.27, frac>50→0), now measurable.
Open question worth checking: agent boards use INFERRED rails (no power symbols) — a less-tuned path
than the references; the 54-power-symbols-for-29-parts (esp32) distributed-power may itself inflate sprawl.

**Spread-weight tuning (also FAILED to generalize — reverted):** `proxy_cost` spread 0.45→1.6 then
0.8 (dense-only, refs untouched). stm32 improved a lot (sprawl 63→30→37) but **esp32 consistently
REGRESSED (80→102→101)** and audio mixed (37→56→37). This is the chaotic-SA-sensitivity trap CLAUDE.md
names explicitly: a single weight shifts the trajectory unpredictably per-board. Reverted to 0.45
(`PROXY_SPREAD_W` const kept = 0.45, behaviour-identical). 

**Conclusion of iteration 2:** the easy/local levers (post-hoc block passes, cost-constant tuning) are
OBJECTIVELY EXHAUSTED — none generalize. Dense sprawl is emergent from an already-well-tuned
seed+SA+cost. Per CLAUDE.md ("stop grinding, go structural"), the next attempts must be STRUCTURAL,
not another weight/pass:
- **Candidate-selection by metric (low-risk, try first in iter 3):** keep the SA trajectory unchanged;
  among the ~9 truthful low-warning candidates the fast lane already produces, add a sprawl/cluster
  tiebreak (prefer the one closest to human ~23 / 0.27). Avoids the chaotic-trajectory trap because it
  doesn't change what the SA explores, only which result ships. Wire into the pick at `Anneal::search`
  (~:2305) and the fast-lane pick (~:2349). Measure with `tools/layout_metrics.py`.
- **Two-level locality cost** (`docs/specs/locality-aware-placement-search.md`): cluster-internal + boundary
  cost so the global arrangement is optimized at the cluster level — the real structural fix; bigger.
- **Tier S2 learned cost**: fit a ranker on human-vs-perturbed using the metric features; generalizes by
  construction (vs hand-tuning one weight that helped 1/3 boards).

State of working tree (uncommitted, on main): Lever A (SIGNAL_LABEL_SPAN=90, finalize-only, oracle-green,
refs byte-identical) + `tools/layout_metrics.py` + gated-off Lever B (`BLOCK_FLOW`, default off) +
`build_functional_blocks`/`compact_blocks` (kept, reusable). Gates green (snapshot, anneal oracle).

## Iteration 3 (2026-06-21) — LLM-as-planner

Force-layout local fixes are exhausted (iter 2). The GLOBAL structure (signal flow + block grouping)
that the local search can't discover should come from the **LLM** (which understands the circuit).
Two existing engine hooks: soft `zbias` (`ir.zone`, `$ZONE_FILE`) and the **authored `layout:` grid**
(`grid_from_layout`, the tuned path the 8/10 references use). New tools: `tools/zone_plan.py`,
`tools/grid_plan.py` (gpt-5.4 → zones / 2D grid).

**Soft zones (`ZONE_FILE`, `ZBIAS_W` raised 0.8→4.0) — FAILED.** gpt-5.4 produced a *sensible* plan
(power-left, MCU-center, peripherals-right) but the engine only partly follows it (soft bias loses to
spread/hpwl; the candidate pick `layout_cost` has no zone term). Critic A/B: **stm32 8→7, esp32 7→5,
audio 6→5 — worse on all three.** Soft zones disrupt without enough benefit. (ZBIAS_W left at 4.0;
zero-effect when no zone file → references byte-identical, harmless.)

**Authored `layout:` grid (`grid_plan.py`) — testing.** The STRONG form: LLM emits a 2D refdes grid →
the engine places it via the grid_order wall (reliable, not soft) → fans satellites. gpt-5.4's esp32
grid is sensible (power→MCU→peripheral→microSD flow) and renders clean (0 crossings), but came out
PORTRAIT (9×6) with a few stray single cells. Measuring with `--samples 2` (critic variance is huge:
esp32 baseline scored 5 AND 7 across runs — single-sample A/B is unreliable; ALWAYS use samples≥2).

**Measurement lesson:** critic variance ±1-2 makes single-sample comparisons noise. Use `--samples 2-3`
for every A/B from now on; lean on `layout_metrics.py` for the deterministic signal.

**Grid result + the big recalibration.** esp32: baseline **8/10** = grid **8/10** (samples=2, identical
dims read8/rout8/comp7/conv8). audio grid regressed (24 ic_crossings). So LLM-authored grid is NEUTRAL.
More importantly: the SAME esp32 baseline image scored **5, then 7, then 8** across this session →
**critic variance is ±2-3**, larger than any effect I measured. Re-read of the whole session under this
lens: the engine is ALREADY ~8/10 on these dense boards; the early "5/10"s were low-variance draws, and
most of my A/B deltas (Lever A 8→8, zones 8→7, grid 8→8) are WITHIN NOISE. The only trustworthy signals
are DETERMINISTIC: ic_crossings (Lever A audio 9→0 = real win; audio-grid 24 = real regression),
wire_frac_gt50, sprawl. Hand-tuning against a ±3-noise critic on 3 boards is chasing noise.

ZBIAS_W reverted to 0.8 (zones failed; zero-effect without a zone file regardless). Zone/grid LLM-planner
tools kept (`tools/zone_plan.py`, `tools/grid_plan.py`) — reusable if integrated into the candidate
pick later.

### Recalibrated direction (for next iterations)
1. **Reliable evaluation FIRST** (the real blocker): high-sample (K≥5) critic + the deterministic metric
   across a LARGER battery (10+ dense circuits, drawing on the 293 new dense human-comparable prompts),
   aggregating DEFECTS that recur in ≥half the samples — the signal above the noise. Without this, no
   change is measurable.
2. **Fix DETERMINISTIC defects** that recur board-to-board (ic_crossings, body_crossings, wire_frac_gt50)
   — these are trustworthy and Lever-A-style fixable, unlike critic-noise "sprawl".
3. Recurring REAL qualitative defects (consistent across runs): (a) spread/whitespace [placement, didn't
   generalize], (b) "label-heavy hurts tracing" [tension: long-wire vs label, both penalized — root is
   placement tightness], (c) minor text crowding. (b) suggests a deterministic win: BANK auto-labels by
   side/prefix beside the IC (mined rule #10, the high-pin-IC fan-out hub) so label-heavy sheets read clean.

## Iteration 4 (2026-06-21) — deterministic wins + a measurement-fidelity fix

**Lever A finalized at 70mm (was 90), by a DETERMINISTIC sweep (no critic):** on the dense battery,
span=70 gives `ic_crossings`=0 (eliminates wires-through-IC-bodies, the worst class), `wire_frac_gt50`
~0.01 (human target ~0), and label counts within human range — strictly better than 90 (fewer long
wires, equal/lower labels) and avoids the label explosion of 50 (audio 55 vs 25 labels). **Oracle GREEN**
(anneal challenge fixtures, 30s). Finalize-only + board-gated → refs byte-identical. This is the
session's cleanest real win — measured on trustworthy deterministic signals, not the ±3-noise critic.

**MEASUREMENT-FIDELITY BUG found (explains a lot of session pessimism):** the ugly `NET_U1_NRST` labels
in my A/B renders are a **lift artifact**, NOT the agent's output. The agent's actual `.kicad_sch` and
`.draft.yaml` have CLEAN names (OSC_IN, SWDIO, NRST, BOOT; zero `NET_*`). My re-render pipeline
(lift flat `circuit.yaml` → INFER) mangles unnamed-net names, so every `ours-gen-*` render was penalized
by names the engine never emits. → The real dense output is cleaner/better than my lifted re-renders
showed. FOR FAITHFUL ENGINE A/B: render from the agent's `.draft.yaml` (clean names) OR critic the
direct `agent_design` PNGs (`/tmp/renders/dense-*.png`, `hard-*.png`) — never the lifted flat yaml for
naming-sensitive judgement. (Deterministic metrics — ic_crossings/frac>50/label-count — are unaffected
by names, so the Lever A=70 decision stands regardless.)

**Faithful hard-dense test (in progress):** generating 2 genuinely hard dense boards (4-ch motor driver,
RP2040 datalogger) via gpt-5.4 on the CURRENT engine, to critic with samples=3 — the real-product
measurement the goal (9+ on hard dense) actually needs.

**Faithful results (samples=2-3, real clean-name output) — the conclusive picture:**
| board | parts | sprawl | critic | dominant defect |
|---|---|---|---|---|
| stm32 | 19 | 63 | 6/10 | RESET long detour, sprawl |
| esp32 | 29 | 80-105 | 5/10 | decoupling/pullups stranded far |
| audio | 37 | 37 | 5/10 | scattered islands, label-only |
| motordrv | 46 | 98 | 5/10 | 4 channels scattered, decoupling sprawl |
| datalogger | 43 | 205 | **4/10** | vast canvas, huge gaps, no inter-block wiring |

**CONCLUSION (locked): dense-board SPRAWL is the core blocker to 9+, and it is a FORCE-LAYOUT
LIMITATION, not a tuning/selection problem.** The cost ships sprawl because `score()` is
crossings-first and spreading parts removes crossings; worse, NO compact layout is ever *generated*
(the most-compact candidate, gravity, is still sprawl ~60 vs human ~23). Every incremental fix tried
(block re-pack, compaction, spread-weight, zones, grid, compact-selection — 6 variants) fails or
doesn't generalize. The fix is STRUCTURAL — see `docs/specs/dense-sprawl-the-core-problem.md`:
block-aware compact placement (coarse partition → tight blocks → flow-ordered with one gutter) +
a compactness-accepting ship decision, or a sequence-pair/B*-tree packed encoding (S1). This is a
focused multi-hour rewrite, not an incremental tweak — the next major effort.

**Shipped this session (real, gated, oracle-green):** Lever A (signal-label distribution @70mm,
deterministic-tuned: ic_crossings=0, frac>50→0). Everything else reverted to zero-effect/gated-off.
**Infrastructure built:** `tools/layout_metrics.py` (objective measurement — the key enabler),
`tools/zone_plan.py` + `tools/grid_plan.py` (LLM-planner harness), mined human rulebook, dense+hard
gpt-5.4 batteries, the faithful-rendering insight (lift mangles names; use draft/direct PNGs).

## Iteration 5 (2026-06-22) — the SA SPREADS vs greedy (new lead)

Two measurement bugs fixed first: (a) I'd deleted the `gen-*` fixtures last turn, so this turn's early
renders silently FAILED and the metric read STALE files (invalidated the "BLOCK_FLOW inert" reading —
restored fixtures). (b) Real BLOCK_FLOW diagnostics: `build_functional_blocks` gives ONE mega-block +
many SINGLETONS (esp32 [19,2,1×8]; audio [36,1]) — sprawl is INTRA-cluster, not inter-block-gutter, so
`compact_blocks` (inter-block) is the wrong tool (it even INCREASES spread).

**KEY FINDING: the premium ANNEAL spreads the layout vs GREEDY** (deterministic sprawl):
| board | greedy | anneal |
|---|---|---|
| stm32 | **49** | 63 |
| esp32 | **56** | 105 |
| audio | 58 | **39** |
The SA optimizes the crossing-heavy premium cost → spreads parts to avoid crossings. Greedy hill-climbs
the base cost → stays tighter. BIG on MCU-centric boards (esp32 47% tighter under greedy); REVERSED on
audio (multi-unit op-amps benefit from the SA). So it's board-dependent — NOT "switch to greedy"
(memory `keep-the-sa-optimizer`: user wants a properly-built SA, not greedy).

Tested + reverted: `DENSE_COMPACT` (skip route-refinement) — no consistent sprawl gain (the candidates
themselves are sprawled). So skipping refinement isn't it; the SA path genuinely generates sprawl.

**Decisive test running:** critic GREEDY (compact) vs ANNEAL (sprawled), samples=2. If compact critics
higher where it's available (esp32/stm32), the fix is a **compactness-aware, board-ADAPTIVE pick**: add
the greedy result as a fast-lane candidate AND let the pick trade a crossing budget for compactness, so
each board ships whichever is tighter — keeps the SA, generalizes by adaptation.

### VERDICT — the session's biggest correction: my sprawl metric MISLEADS
Critic (samples=2) GREEDY vs ANNEAL: esp32 **5=5**, stm32 **8=8**, audio **6<7**. ANNEAL critics
equal-or-BETTER everywhere, and the critic rated ANNEAL's *compactness* dimension HIGHER (stm32 comp
8>7, esp32 4>3) — even though greedy has the smaller bbox. Greedy's smaller bbox reads as MORE
fragmented ("scattered across large empty areas"); anneal's bigger bbox reads as better ORGANISED
(aligned banks, clean blocks). **bbox-`sprawl` and `wire_frac_gt50` are POOR proxies for quality** —
the whole greedy/compaction chase optimised the wrong thing, and the "structural-rewrite needed"
pessimism was inflated by them.

Corrections applied:
- **ANNEAL (current production) is the best path** — keep it. The engine is actually ~6-8 on dense
  boards (stm32 8, audio 7), NOT 4-5; the lows were unlucky critic draws + lifted-name artifacts +
  my bad-proxy pessimism. esp32 (5) is the genuine outlier.
- **Reverted Lever A 70→90.** The turn-4 70mm change optimised `frac>50` but the critic penalises the
  resulting label-heaviness ("almost entirely net labels, hard to trace"): esp32 26 labels @90 vs 41
  @70, SAME ic_crossings=0. 70 was a self-inflicted regression. 90 keeps the ic_crossings win with
  fewer labels.
- **Removed all dead experimental code** (BLOCK_FLOW/compact_blocks/build_functional_blocks/
  DENSE_COMPACT/PREFER_COMPACT). Tree = Lever A@90 (finalize-only, refs byte-identical, oracle-green)
  + `tools/layout_metrics.py` + docs. Clean.

**Standing lesson for the loop:** validate quality changes on the CRITIC (samples≥2 on faithful
clean-name renders), NOT on bbox-sprawl/frac>50 — those moved opposite to quality this whole session.
The real remaining gap: esp32-class boards (label-heavy + some real disorganisation) at ~5-6; modest,
not the catastrophe the metrics implied.

## Iteration 6 (2026-06-22) — LLM layout HURTS; the anneal is the local optimum

Re-tested LLM-authored `layout:` grid on the weak board (esp32) with samples=2 critic (the right judge):
**esp32 NO-GRID (anneal) 7/10 vs LLM-GRID 5/10 — the grid HURTS.** Forcing the LLM's ordering through
the engine's ordinal-grid spacing fragments the sheet (isolated microSD, gaps); the autonomous anneal
handles spacing far better than any external grid/zone constraint. Combined with iter-3 (zones worse,
grid neutral): **LLM-provided layout makes dense boards WORSE.** Avenue closed.

**Definitive conclusion after 6 iterations of critic-validated testing: the anneal-based engine is the
LOCAL OPTIMUM among everything constructible here.** It beats greedy, grids, zones, blocks, compaction,
spread-tuning, compact-selection — every alternative is worse or neutral on the critic. The engine
ships ~6-8/10 on dense boards (esp32 7, stm32 8, audio 7; hard motordrv 5, datalogger 4).

The recurring NAMED defect is *"decoupling bank far from the IC it serves"* + repeated-motif scatter
(motordrv: "four identical DRV8871 channels scattered"). Both need information NOT in the netlist:
- which 3V3 cap decouples which IC (all share 3V3/GND — the engine's "most-rail-pins" heuristic mis-picks
  the MPU over the ESP32 module). Designer intent; the LLM has it but grid/zone injection dilutes it.
- that N subcircuits are IDENTICAL and should TILE on a lattice (mined rule #9; motordrv's 4 channels).

### The two concrete levers left (both real, neither tried — substantial implementations)
1. **Repeated-motif tiling** (mined rule #9): detect groups with same lib_id-multiset + same net-topology
   + shared refdes prefix (N≥3, e.g. 4×{DRV8871+cap+Rsense}); place them on an exact regular lattice
   (uniform pitch, byte-identical intra-cell). DETERMINISTIC, validatable on motordrv ("4 channels
   scattered" → tiled). Highest-confidence next implementation; fires only on repeated-structure boards
   (motor drivers, LED arrays, keyboard/key matrices) but dramatic there.
2. A genuinely better PLACEMENT algorithm than the anneal (research-level) — the only path to lift the
   general case past ~8; the anneal is already the local optimum, so this is not an incremental tweak.

Session deliverable: Lever A@90 (shipped, gated, oracle-green) + `tools/layout_metrics.py` +
mined rulebook + the hard-won measurement lessons (critic-not-metrics; faithful renders) + this
exhaustive ruled-out map so the loop never re-treads. The "engine is broken/4-5" framing was wrong;
it's a solid ~6-8 at its anneal optimum, with motif-tiling the best-targeted concrete next gain.

### Motif-tiling implemented + tested (opt-in `MOTIF_TILE`)
`align_repeated_motifs` (floorplan.rs): N≥3 anchors of the SAME part (4× DRV8871) → tiled on a
regular lattice, carrying their satellite blocks; strictly additive (applied only if it doesn't
increase truthfulness-breaks OR warnings). Motordrv: the 4 channels go from scattered-along-the-bottom
to a clean 2×2 grid. **Critic samples=2: OFF 6 = TILE 6 (convention dim +1).** NEUTRAL overall —
because motordrv's dominant flagged defect is an UNRELATED one (the buck regulator's 10-cap vertical
column + ILIM resistors scattered far from their drivers), which tiling doesn't touch. So it's a real,
safe, mined-rule visual win but not a proven net score gain on this board. Kept OPT-IN (not default-on):
the grid experiment showed layout-forcing can hurt the critic undetectably by warnings, and one board
isn't enough to default it. Would help more on a PURE repeated-array board (LED matrix, key matrix) —
validate there before promoting. Two engine features now exist: Lever A@90 (default) + MOTIF_TILE (opt-in).

The new motordrv lead: the dominant defect is a single part with MANY filter caps strung in a long
column (the buck) — same class as the "decoupling bank far from IC" defect. A satellite-fan-arrangement
problem (many caps on one IC → tidy 2-wide bank near the IC, not a 10-tall column). Concrete + recurring
across boards; a candidate next lever (validate on critic).

## Iteration 7 (2026-06-22) — CAP_BANK tried + reverted; the neutral-fix pattern

Implemented `align_cap_banks` (tall ≥6-cap column → compact 2-wide bank, gated `CAP_BANK`, warning-safe).
Visually it worked (buck column → compact block) and KILLED the named defect ("tall vertical cap column"
→ "nicely-aligned bank", convention dim 5→7). But critic samples=2: **OFF 6 = ON 6** — it INTRODUCED
congestion ("the power-supply region is a congested knot", read/rout −1): the 2-wide pack is too tight
for the caps' local power symbols. Traded one defect for another → **reverted** (unlike MOTIF_TILE which
was cleanly neutral with no new defect).

**The settled pattern after 7 iterations: targeted tidy passes are NEUTRAL on the (noisy ±2-3) critic.**
They fix the one named defect but (a) an unrelated defect dominates the score, and/or (b) the fix adds a
minor new one (tighter packing → congestion). The engine is at its anneal optimum ~6-8; incremental
post-passes don't move it. CONCLUSION: the path to consistent 9+ is NOT more targeted passes — it needs
either designer-intent the netlist lacks (which rail-cap serves which IC; LLM-grid/zone injection of it
FRAGMENTS the layout, so that's not the delivery mechanism) or a learned placement/arrangement model
(Tier S2/S1 — a major effort), or accepting ~6-8 as a solid result.

FINAL session state (uncommitted, gates green, refs byte-identical):
- **Lever A@90** (default): eliminates wires-through-IC-bodies; the one shipped quality win.
- **MOTIF_TILE** (opt-in): tiles repeated same-part anchors (4× driver channels); cleanly neutral, mined rule #9.
- `tools/layout_metrics.py` + `zone_plan.py` + `grid_plan.py` + mined rulebook + this exhaustive ruled-out map.
- Durable lessons: validate on CRITIC (samples≥2, faithful clean-name renders), NOT bbox-sprawl/frac>50
  (they moved OPPOSITE to quality all session); the anneal is the local optimum (greedy/grids/zones/blocks/
  compaction/spread-tuning all worse-or-neutral); the engine's ceiling is designer-intent the netlist omits.

## Iteration 8 (2026-06-22) — designer-intent decoupling: implemented, REGRESSED, reverted

Pursued the one path flagged most-promising last turn: feed the agent's OWN functional grouping (its
multi-block `create_design`) into the engine to resolve the cap→IC ambiguity. CONFIRMED the premise: the
agent's draft blocks correctly group each decoupling cap with its IC (esp32: C3/C4→mcu/U1,
C5/C7/C8/C9→imu/U4, C1/C2→power_entry, C6→microsd), and the engine THROWS THIS AWAY (flattens at commit;
the idiom matcher then over-merges ALL V+↔GND caps into one bank anchored by "most-rail-pins" → on esp32
that's the MPU, not the ESP32 module). Implemented `block_of` (refdes→block) threaded into the idiom
detector + `best_decoupling_anchor` + cohesion, splitting the bank by block and anchoring each group to
its own-block IC. Byte-identical for single-block/sidecar designs (snapshot green).

**Result: esp32 FLAT 8/10 → BLOCK 5/10 — a clear REGRESSION. Reverted.** Why: the per-block cap groups
are <3 caps so no tidy BANK forms — they scatter as individual cohesion-placed caps (comp 7→3, "scattered
across a huge canvas"). **TIDINESS (an aligned bank) beats CORRECTNESS-OF-ASSOCIATION:** the engine's
over-merged bank at the "wrong" IC reads as clean/convention-respecting (8/10); the "correct" but
scattered caps read far worse. Also re-confirmed esp32 baseline is genuinely ~8 (the earlier 5s were
critic ±2-3 noise) — the "stranded decoupling" I targeted was largely a low-variance artifact.

**This closes the LAST credible lever.** Designer-intent integration — the most-promising untested
path — makes it WORSE, because the engine's tidiness heuristic already wins and the LLM's correct
cap→IC mapping doesn't translate to a tidier sheet. Every avenue (greedy, grids, zones, blocks,
compaction, spread-tuning, compact-selection, motif-tiling, cap-bank reshape, designer-intent decoupling)
is now tested: all worse-or-neutral on the critic. The engine is at its anneal optimum, ~6-8, and the
residual is critic noise + a long tail of marginal defects no single change moves. FINAL shipped state
unchanged: **Lever A@90 (default) + MOTIF_TILE (opt-in)**; gates green; refs byte-identical.

## Iteration 9 (2026-06-22) — MULTI-SHEET: the right density answer + the real ceiling

The single-sheet sprawl ceiling caps complete dense boards. The PROFESSIONAL fix (53/500 human boards do
it) is HIERARCHICAL multi-sheet: one sheet per functional block. Already built:
`crates/agent/examples/render_multisheet.rs` (compiles a multi-block draft, emits each block as a
single-block sub-design with shared nets → labeled ports, renders each).

**esp32 (4 sheets, samples=2 critic):** mcu **8**, imu **8**, microsd **8**, power_entry **6**. Each clean
sheet has its decoupling RIGHT beside its IC — multi-sheet ALSO solves the cap→IC problem for free, by
separation (no sibling IC on the sheet). Brings dense designs to a consistent ~8 vs the noisy 5-8 of the
cramped single sheet.

**THE REAL CEILING (key finding): the engine caps at ~8 PER SHEET regardless of simplicity.** The clean
sub-sheets score 8 — exactly the TUNED REFERENCES (mcp1703 8, uart 8). So 9 is NOT a density problem; it's
a per-sheet quality limit on EVERYTHING. 8 iterations of every rule-based/LLM lever couldn't lift even
simple sheets past 8 → **9+ is very likely UNREACHABLE with this engine architecture**; it needs the
learned holistic-aesthetic placement model (Tier S2), a research effort.

Multi-sheet IS the best achievable + correct density handling (consistent ~8, professional, fixes
decoupling-association by separation). Weak sheet power_entry=6 is a localized routing issue (ESD
pass-through congestion).

**MOTORDRV (the hardest board, single-sheet 4-5/10) → multi-sheet, samples=2:** debug_status **9**,
mcu_core **8**, motor_driver_1_2 **8**, motor_driver_3_4 **8**, power_entry_1 **9**, power_entry_2 **9**.
**Average ~8.5, THREE 9s** — a single-sheet 5/10 board becomes six clean 8-9 sheets. And `power_entry_2`
(the BUCK) scored 9 — the "tall 10-cap column" defect I spent all of iter-7 on just VANISHES when the buck
gets its own sheet with room. **This is the genuine answer for dense circuits.** The session's conclusion
flips from "stuck at 6-8" to: dense quality = MULTI-SHEET (8-9 per block), and the engine is already
excellent per-block (it's the single-sheet CRAMMING that was the whole problem).

DELIVERABLE for production: a multi-sheet COMMIT path in the agent (`apply_design` emits one flattened
sheet via `emit_anneal`). render_multisheet proves the per-block emit; the agent piece is a hierarchical
kicad_sch (root + sub-sheets + hierarchical labels) — substantial but the genuine win for dense designs.

## Iteration 10 (2026-06-22) — multi-sheet COMMIT implemented + validated; finding generalizes

**Generalizes to all 3 hard boards** (per-block, samples=2 critic):
- esp32 5-8 single → mcu/imu/microsd 8, power_entry 6 (~7.5 avg)
- motordrv 4-5 single → 9,8,8,8,9,9 (~8.5 avg, THREE 9s)
- datalogger 4 single → mcu 8, power 9, qspi 9, sensors 8, storage 7 (~8.2 avg, TWO 9s)
So every hard board goes from a cramped 4-5/10 to ~8/sheet with multiple 9s. The answer is solid.

**COMMIT IMPLEMENTED** (`crates/agent/examples/render_multisheet.rs` + `write_multisheet_project`):
writes a hierarchical KiCAD project — root `.kicad_sch` (deterministic uuids, per-block `(sheet)` symbols)
+ per-block sub-sheet files, with symbol instance-paths rewritten into the hierarchy
(`/SUB_ROOT` → `/MAIN_ROOT/SHEET_UUID`) and global-label cross-sheet connectivity. Format reverse-engineered
in `docs/specs/multisheet-commit.md`.

**VALIDATED with kicad-cli:** `sch erc` → ZERO path/annotation/unconnected/duplicate errors (the hierarchy +
instance paths + cross-sheet connectivity are STRUCTURALLY CORRECT); the only "errors" are the env's
missing-library config (affects any ERC run). `sch export svg` → renders root + every sheet (a real,
openable KiCAD design). Remaining 12 MINOR quality violations: 5 global_label_dangling (block-internal nets
over-marked as ports), 6 pin_to_pin (PWR_FLAG across sheets), 1 same_local_global_label — refinements, not
blockers (the single-sheet path also has ~53 such warnings).

**Bottom line: the session's answer — dense circuits → multi-sheet (8-9/sheet) — now has a WORKING,
validated, committable implementation.** Remaining: (a) wire `write_multisheet_project` into the agent's
`apply_design` (detect dense multi-block → multi-sheet commit) so the live agent ships it; (b) trim the 12
minor ERC violations (tighten the port/internal-net split + per-sheet PWR_FLAG).

## Iteration 11 (2026-06-22) — multi-sheet WIRED INTO THE AGENT (production)

**Extracted** the commit into a reusable module `crates/agent/src/multisheet.rs`: `emit_multisheet`
(one-sheet-per-block — agent blocks are already well-sized; the example's split/merge is a follow-up),
`write_project` (hierarchical root + sub-sheets + instance-path rewrite), `dedup_pwr_flags`, `emit_and_check`.
**Wired into `apply_design`** (tools.rs): a DENSE multi-block design (≥3 blocks, ≥25 parts) now COMMITS as a
multi-sheet project (root at `ctx.sch_path`, sub-sheets alongside) instead of a cramped single sheet.
**The live agent now ships dense designs the professional way.**

**ERC cleanup:** each sub-sheet emitted its own PWR_FLAG → duplicate "power output" ERC errors.
`dedup_pwr_flags` keeps one PWR_FLAG per net globally → **8 errors → 1-3** across motordrv/esp32/datalogger
(the 80-90 lib_symbol_issues are the env's missing-library config, not faults). Remaining 1-3: a PWR_FLAG on
a net that ALSO has a real regulator-output driver (3V3 from the LDO) — benign (redundant driver,
connectivity intact, << the single-sheet's 53 warnings); full suppression needs cross-sheet driver analysis.

**Validated** `emit_multisheet` on the live-gpt-5.4 drafts (the exact fn apply_design calls): motordrv
(5 blocks/58 parts), esp32 (4/42), datalogger (5/54) → valid hierarchical projects, render all sheets,
1-3 ERC errors. **Gates green**: placement_snapshot byte-identical (apply_design change gated to dense
multi-block; refs single-block), full agent builds clean. New example `commit_multisheet` validates it.

**STATUS: the deliverable is built + validated** — the session's answer (dense → multi-sheet 8-9/sheet) is
now PRODUCTION; the agent emits it automatically. Remaining polish: the 1-3 ERC errors; refined
block-split/merge; a multi-sheet-aware preview render.

**DETERMINISTIC apply_design commit test** (`examples/test_apply_multisheet`, no live LLM): create_design →
`apply_design{commit:true}` on the motordrv draft (5 blocks/58 parts) → committed sch `is_multisheet=true`,
**6 kicad_sch files** (root + 5 sub-sheets). CONFIRMS the commit path writes a multi-sheet project. (A live
gpt-5.4 BLDC run showed `applied=false` — gpt-5.4 didn't finalize the commit that run, i.e. AGENT behavior,
not a code fault: the return JSON is unchanged by my edit and the deterministic commit succeeds. Live commit
depends on the agent choosing commit=true, which is normal nondeterministic agent behavior.)

## Iteration 12 (2026-06-22) — live e2e SUCCESS + ERC v2 dedup + the next lever

**LIVE gpt-5.4 e2e WORKED end-to-end.** Fresh prompt (USB-C ESP32-S3 IoT sensor gateway) → gpt-5.4 designed
it → **applied=true** (committed; confirms the prior BLDC `applied=false` was pure gpt-5.4 nondeterminism,
not a bug). Split into 5 well-sized sheets (power_entry 13, controller 11, storage 8, sensors_display 7,
indicators 5). Per-sheet critic: **controller 8, indicators 9, power_entry 6, sensors_display 6, storage 6 —
avg 7.0**. HONEST FINDING: multi-sheet fixes block SEPARATION, but per-sheet placement is **6–9, NOT uniform
9** — clean blocks (controller/indicators) hit 8–9, congested ones drop to 6. The three 6s share concrete,
recurring, FIXABLE defects: (a) a net **label overlapping a component body** (SD_MOSI over R7) — same class as
the old wire-through-body detector gap, a label-placement issue; (b) **congested signal channels** (parallel
vertical runs + stacked junctions left of a multi-IC); (c) the **exiled paired CC resistors** (idiom gap).
These three are the concrete path to uniform 9 — each is a real per-sheet placement/routing defect, not vibes.

**ERC v2 dedup** (`dedup_pwr_flags`): added DRIVEN-net detection — a net referenced on a sheet that carries
no flag for it is regulator-driven (e.g. 3V3 off an LDO) and needs NO flag; strip every PWR_FLAG for it
(the flag conflicts with the driver's power-output). UNDRIVEN rails (GND/VBUS/VMOTOR off a connector) keep
exactly one. Result: **motordrv 1→0, esp32 1→0 (both ERC-CLEAN), datalogger 3→2**. The 2 datalogger errors
are pre-existing design/split artifacts (VBAT has NO flag anywhere → battery-net limitation, not my dedup;
SWCLK debug-net undriven; a GPIO↔power net collision; one SD_MISO↔GND label short) — NOT flag handling.
Kept flags verified = exactly the undriven rails. Snapshot gate byte-identical, full build clean.

**THE NEXT LEVER toward uniform-9 (per-sheet quality):** multi-sheet gives clean block SEPARATION, but
within-sheet placement still has the ~8 ceiling with variance to 6. The live power_entry=6 names a concrete,
recurring, HIGH-VALUE defect: the **USB-C CC-pulldown pair** (two ~5.1k R from CC1/CC2 to GND) is not
detected as an idiom, so R2 gets exiled. USB-C is ubiquitous → a `cc_pulldown_pair` idiom (co-place the two
CC resistors beside the connector, like the decoupling/divider idioms) is a well-defined, non-speculative
win, not constant-tuning. sensors_display (multi-IC, 12 xings) points at the same per-sheet placement work
(Tier S2 learned cost is the general path; targeted high-frequency idioms are the cheap path).

## Iteration 13 (2026-06-22) — REFRAME: the per-sheet 6s are PORT-LABEL CONGESTION, not label orientation

Investigated the storage-sheet "SD_MOSI label over R7" defect deeply + EMPIRICALLY (rendered the label at 4
angle/justify variants). FINDINGS:
- The defect is NOT a label-orientation bug. SD_MOSI is a single-pin cross-block PORT at R7's bottom pin; its
  global-label PENTAGON extends DOWN from R7 (matching `port_label_obstacle`'s Side::Bottom model) and
  **overlaps R8 + the SD_CLK wire just below**. Flipping the label angle (270→90) only rotates the text
  INSIDE the pentagon — the pentagon's box position is fixed by anchor+side, so **every angle variant is
  byte-identical in the render**. So orientation is a dead end; ruled out empirically.
- Root cause = **port-label CONGESTION**: the placement packs R7 (vertical), R8 (horizontal), and three port
  pentagons (SD_MOSI, SD_CLK, SD_MISO_RAW) into one tight column, so the labels collide with neighbor bodies.
  This UNIFIES defects (a) [label-over-body] and (b) [congested channels] — both are over-packed port-rich
  regions, not orientation/routing.
- There IS a relief routine — `nudge_satellites_off_labels` (floorplan.rs:3944) pushes a free satellite off a
  FOREIGN port-label keepout toward its home centroid — but it under-performs here (R8 stays overlapping):
  likely R8 can't clear within the step cap, or its home-ward push doesn't exit the box. **That routine is the
  fix site.** Concrete next step: when a satellite can't clear toward-home, fall back to a least-penetration
  push directly out of the box; and/or feed port-label boxes into part SPACING so port-rich columns get room.

NET: this turn ruled out the wrong fix (orientation) and pinned the real lever (port-label congestion relief
in `nudge_satellites_off_labels` + spacing). No engine edit shipped — a placement change at turn's tail risks
the snapshot gate; it's now precisely scoped for a careful, gated implementation. Task #8 updated.

## Iteration 14 (2026-06-22) — SHIPPED: port-label congestion relief → dense avg 7.0 → 8.2

Implemented + VALIDATED the congestion fix. Root, instrumented (NUDGE_DBG): the old `nudge_satellites_off_labels`
pushed a satellite (R8) off a foreign port-label toward its home centroid — but that landed it ON a connector
(J2), and the FOLLOW-UP label-unaware `decongest` then evicted it from J2 straight back onto the SD_MOSI label.
The two passes fought; R8 ended back at its start (x 88.9→60.96 by the nudge, →91.44 by decongest). 

FIX: replaced nudge+decongest with **`decongest_off_labels`** (floorplan.rs) — ONE loop that resolves part-vs-part
overlaps AND free-satellite-on-FOREIGN-port-label overlaps together (net-aware: never pushes a part off its OWN
port's label), so neither undoes the other. Gated on `MULTISHEET_REFINE` (single-sheet refs never reach it).

RESULT (live2 IoT-gateway, content-only critic, samples=2): **controller 8→9, indicators 9→9, power_entry 6→7,
sensors_display 6→8, storage 6→8 — AVG 7.0 → 8.2.** The two worst sheets (+2 each); even the clean controller
+1. GATES ALL GREEN: placement_snapshot byte-identical; LAYOUT_SEARCH=anneal floorplan_netlist PASS;
ERC unchanged on the 3 hard boards (motordrv 0, esp32 0, datalogger 2). First concrete per-sheet quality lift.

REMAINING caps toward uniform 9: (1) power_entry 7 = the **CC-pulldown-pair idiom** (R1/R2 5.1k CC1/CC2→GND not
co-placed — the still-separate defect c); (2) sensors/storage 8 = minor residual dog-legs / small wire clusters;
(3) a NEW find — COMMITTED (framed) sub-sheets place content (J2/C4) OVER the A4 title block (a framed render
scored 4 vs 8 content-only). The committed multi-sheet output should reserve the title-block region or offset
content above it — a real polish item for what the user actually opens (the critic only sees content-only).

## Iteration 15 (2026-06-22) — FIXED the committed-sheet title-block overlap (deliverable bug)

Investigated two levers. (1) The **title-block overlap** in the COMMITTED multi-sheet output: sub-sheets carry a
title block but a tight content-fit `User` page (e.g. storage 132×95), so KiCAD's title block (page bottom)
overprints the lowest parts — a real bug in what the user OPENS (content-only renders hide it; framed render
scored 4). FIXED: `emit.rs` paper-sizing now reserves a 33 mm bottom band when a title is set AND
`MULTISHEET_REFINE` is on (paper → 132×128). Content is top-left anchored (content_extent = max-corner+margin),
so the band sits empty below it and the title block drops into it cleanly. **Gated on MULTISHEET_REFINE ⇒
single-sheet references never reach it ⇒ snapshots BYTE-IDENTICAL** (verified). Content placement unchanged, so
content-only critic (8.2) + connectivity are untouched. Framed storage critic **4 → 7** (title-block overlap
gone; residual = the minor routing congestion, same as its 8 content-only). The committed deliverable is now
correct.

(2) The **cc_pulldown_pair idiom** (power_entry's cap): mapped the idiom system — declarative patterns in
`circuit-graph/src/library.rs` + a PATTERN-NAME-SPECIFIC placement `match` (floorplan.rs:798, each idiom needs a
`place_*` fn) + detection runs on ALL boards (snapshot risk if a reference matches connector+2R-to-GND). So it's
a real multi-part change (pattern + place_cc_pulldown + active_library + snapshot-safe validation) — deferred as
the next critic-moving step, scoped. The title-block fix was the safe, completable win this turn.

## Iteration 16 (2026-06-22) — CC-pulldown idiom: tested, REGRESSED, reverted (a clean negative result)

Implemented the cc_pulldown idiom end-to-end and tested it; it made power_entry WORSE, so reverted.
- **Pattern (KEPT, snapshot-safe):** `CC_PULLDOWN` in `circuit-graph/src/library.rs` — USB-scoped anchor
  (`LibAny(["USB"]) + PinsAtLeast(6)`) + two resistors on distinct connector signal nets, both to GND.
  Verified the at-risk references (uart-level-translator, grid-demo) use GENERIC connectors (`Conn_01x05`,
  `Conn_01x02`), so a USB-scoped pattern never matches them ⇒ placement_snapshot stayed BYTE-IDENTICAL
  with it active. Moved to `extended_library` (defined+safe, not active) after the placement failed.
- **Placement (REVERTED):** tried report-only + a post-pass `align_cc_pulldowns` snapping the exiled twin R2
  beside R1 (mirroring `align_led_chains`). FAILED: in the congested USB-entry area the snapped spot is taken,
  so the follow-up decongest scatters R1/R2 AND the LDO/bank; with no decongest the twins' rotated labels
  COLLIDE. **power_entry 7 → 6** (critic: "colliding rotated net labels at R1/R2 … scattered LDO/bank").
  Removed the post-pass + match arm + active entry. Gates restored: snapshot byte-identical, idiom tests 12/0,
  build clean, ERC 0; power_entry back to its 6-7 baseline (the 1-pt swing is critic variance, same layout).
- **LESSON (the real fix):** report-only "snap beside twin" only works where the spot is free (LEDs). For
  a congested connector cluster the pair needs a FROZEN geometry placement BELOW the connector's CC1/CC2 pins
  (à la `place_crystal` beside the osc pins) so the search keeps them tidy as a unit and routing/labels follow.
  That's the scoped next step; the pattern is ready in extended. Net: a tested idea that didn't pan out, cleanly
  reverted with no regression — exactly the "test ideas, keep what works" loop.

## Iteration 17 (2026-06-22) — CC-pulldown idiom SHIPPED (freeze placement); removes the defect, sprawl now caps

Re-attempted the cc_pulldown idiom with a FREEZE placement (the iter-16 lesson: the report-only snap failed
because the target cell wasn't reserved). Added `place_cc_pulldown` (floorplan.rs) — reserves two adjacent
cells beside the connector for R1/R2 — and a freeze match arm (`freeze: true`, claims the pair), re-activated
`CC_PULLDOWN`. RESULT: **R1/R2 now sit together beside J1** (was R2 exiled); the critic's defect list confirms
the fix — it NO LONGER mentions the CC-pulldown/R2-detour, now flagging a SEPARATE pre-existing defect instead.
GATES ALL GREEN: placement_snapshot byte-identical (USB-scoped → generic-connector refs never match), anneal
floorplan_netlist 2/2 (connectivity), idiom tests 12/0, build clean, ERC 0. KEPT (correct + generalizes to
every USB-C board; snapshot-safe; no regression).

**power_entry SCORE still 6 — now capped by a DIFFERENT defect:** a large EMPTY MID-REGION between the
connector cluster (J1+CC+ESD, top) and the LDO/decoupling cluster (U2+C1/C2/C3, bottom). Two loosely-coupled
clusters (only VBUS/3V3 bridge them) placed far apart = sprawl. This is the **cluster-compaction** problem
(DENSE_COMPACT/BLOCK_FLOW were tried earlier this session and reverted as critic-neutral) — the dominant
remaining power_entry defect. NOTE: critic SCORE unchanged but the defect LIST improved (one real defect gone);
per CLAUDE.md "trust the defect list," this is a genuine fix whose score benefit is masked by the sprawl.
Next target: pull loosely-coupled clusters together on a sub-sheet (reduce the empty mid-region).

## Iteration 18 (2026-06-22) — collapse_empty_bands: power_entry 6→7 (empty-mid-region defect gone)

Targeted the sprawl. First ruled OUT splitting (the loosely-coupled LDO cluster is held together ONLY by
power rails VBUS/3V3, so a split-by-non-rail-components over-fragments it into singletons — messy). Instead
shipped **`collapse_empty_bands`** (floorplan.rs, gated MULTISHEET_REFINE): finds the first EMPTY horizontal
band between part rows wider than 25.4 mm and shifts everything below it up to a clean 12.7 mm gap, surgically
(preserves cluster internals), looping for further bands. Only fires on a CLEAN empty band, so compact sheets
(controller/indicators/sensors/storage have none) are untouched.

RESULT: power_entry **6 → 7**; the critic's "large empty mid-region" complaint is GONE, replaced by a DIFFERENT
defect ("LDO isolated far from its decoupling caps" — a decoupling-placement issue, U2's 3V3 output caps C2/C3
not co-placed). GATES GREEN: placement_snapshot byte-identical (gated), anneal floorplan_netlist 2/2
(connectivity, parts shifted + re-routed), ERC 0. KEPT — removes a real defect class (clean empty bands) + safe.

NOTE: power_entry's sprawl was DISTRIBUTED scatter, not a single clean gap, so the collapse only caught a ~10mm
band — yet the score still rose 6→7 and the empty-mid-region defect cleared. The NEW cap is the LDO↔output-cap
distance (decoupling). The whack-a-mole continues: each fix clears its defect and exposes the next; the layout
is steadily better (CC-pair handled, sprawl reduced) even when the score creeps slowly. Next: LDO/decoupling
co-placement (U2 + C2/C3). Live CAN-node test running to check whether sprawl/decoupling defects are COMMON
across designs (evidence to prioritize).

### Live CAN-node result (fresh gpt-5.4 board, 4 sheets) — the defect landscape

power_entry **7** (text crowding @ ESD array + mid-sheet sprawl), mcu_core **8** (crystal+decoupling+reset all
co-placed cleanly; minor SWD dog-leg), can_interface **6** (MCP2562 Vio/STBY adjacent-pin NUMBERS "5"/"8"
overprint — a SYMBOL-level spacing issue, not engine-placed; + VDD power symbol far from its pin = "bare stub"),
sensors **8** (I2C pull-ups + decoupling clean; minor label crowd + one long top run). AVG **7.25**.

EVIDENCE-BASED ASSESSMENT (now 2 fresh boards + the dense fixtures): the engine ships CONSISTENT 7-9/sheet —
idioms work (crystal, decoupling, CC-pulldown, pull-ups all co-place), connectivity is honest, ERC clean. The
remaining gap to uniform 9 is a DIVERSE LONG TAIL, not one lever: (1) net-label / pin-text crowding in dense
corners (engine-addressable, like the iter-14 congestion relief); (2) cluster sprawl (iter-18 collapse helps
the clean-gap case); (3) minor dog-legs / one-off long runs; (4) power-symbol-far-from-pin "bare stub"; (5)
SYMBOL-level pin-number overprint (library, not engine). No single fix moves all sheets to 9 — each clears one
defect and the critic re-targets the next. This is the per-defect-grind's diminishing return; **the structural
path to UNIFORM 9 is Tier S2 (a cost model learned from the human corpus), which optimizes the whole layout
holistically rather than chasing defects one at a time.** The grind still yields safe per-defect wins (kept:
congestion relief, title-block, CC-pulldown, empty-band collapse) and is worth continuing opportunistically,
but uniform 9 is a learned-model problem.

## Iteration 19 (2026-06-22) — rule-3 power distribution on sub-sheets (can_interface 6→7)

Used the mined `human-layout-rulebook.md` to pick the next lever by EVIDENCE. Rule 3 (distribute power as
local symbols, no page-spanning rails) was "✅ partial — only boards > FAST_PINS." That exactly explains the
CAN-node can_interface "VDD bare stub": a small sub-sheet (≤34 pins) falls below the gate, so a power net whose
pins SPREAD across the sheet draws a page-spanning trunk to a far symbol → reads as a dangling stub.

FIX (floorplan.rs rail phase): on `MULTISHEET_REFINE` sub-sheets, also distribute a power net (local symbol per
pin) when its pins span > 38 mm — even below FAST_PINS. Span-gated so tight 2-pin taps keep their clean short
trunk. RESULT — a clean NET WIN across BOTH live boards: CAN-node **can_interface 6 → 7** (VDD bare-stub GONE)
+ **mcu_core 8 → 9** (cleaner local power taps; power_entry/sensors held) ⇒ avg **7.25 → 7.75**; IoT-gateway
ALL FIVE sheets HELD (9,9,7,8,8 = 8.2, no regression). GATES: placement_snapshot byte-identical (gated
MULTISHEET_REFINE), anneal floorplan_netlist 2/2 (connectivity — rail drawing changed), ERC unchanged (8 — the
extra VBUS power symbols are the usual PWR_FLAG class, not faults). The rulebook-driven approach worked: a
concrete, evidence-based, snapshot-safe win on a real recurring defect (page-spanning power rails on small
sheets). REMAINING per rulebook: LEVER C (left-to-right signal flow, ❌ no flow term) is the last HIGH-priority
GLOBAL lever (BLOCK_FLOW was tried+reverted — needs a fresh angle); plus the diverse local tail + Tier S2.

## Iteration 20 (2026-06-22) — rule-6 broad-power decoupling: correct diagnosis, but REVERTED (net flat)

Chased the "orphaned decoupling cap" (rule 6). ROOT, correctly diagnosed: the idiom-graph classifier
(floorplan.rs:753 closure) marks a net Power ONLY if it's a designated `ir.rails` net — so VBUS/3V3/VDD on a
SMALL sub-sheet (not a rail) are Signal, and the decoupling idiom's Power-rail edge never matches them (the
engine's own `is_power_net` recognizes them, but the closure ignored it). Tried: (a) broaden the closure to
`is_power_net(net)` on MULTISHEET_REFINE sub-sheets; (b) lower the decoupling pattern min 3→1 with the host
gating <3-cap matches to sub-sheets. Both gated ⇒ placement_snapshot byte-identical, anneal netlist 2/2, idiom
12/0 — all clean, and C5 DID co-place next to U3.

BUT net flat-to-negative: CAN node power_entry **7→8** (LDO bank decoupling co-placed — a real fix!) but
mcu_core **9→8** and sensors **8→7** ⇒ avg **7.75 → 7.5**. Firing the decoupling idiom MORE BROADLY (every
VBUS/3V3/VDD part, incl. single caps) isn't uniformly better than the SA's own placement — it helped the LDO
bank but disturbed the MCU/sensor sheets. Per "keep only CLEAR wins," **reverted all three changes** (closure,
min, gate); engine back to the iter-19 state (snapshot byte-identical, idiom 12/0, build clean). A clean
negative result + a real lesson: the idiom co-placement is NOT a strict improvement when broadened; the
"orphaned cap" needs a more SURGICAL fix (only co-place a cap that is genuinely far AND would not disturb a
working bank), or it's a Tier S2 (holistic cost) call. The diagnosis (VBUS≠Power in the idiom graph) is logged
for a future targeted attempt.

## Iteration 21 (2026-06-22) — GPT 5.5 is live; fixed an agent-prompt gap that blocked it from designing

**GPT 5.5 is now available** on the gateway (the user's preferred model — was unavailable all session). But its
first two live runs ended `applied=false` with NO design — the full trace showed it called `get_design()` (the
prose's "ALWAYS call this first"), then researched symbols (search/get_symbol_info) for 17 calls, then STOPPED
(`Completed`) without ever calling `create_design`. ROOT: the system-prompt workflow (agent.rs SYSTEM_PROMPT)
lists get_design → search → get_symbol_info → validate_design(yaml) → apply_design but **never mentions
`create_design`** (the tool that authors a new draft) and says get_design is ALWAYS first. gpt-5.4 found
create_design in the tool list anyway; GPT 5.5 went strictly by the prose and never authored.

FIX (prompt only, no engine risk): added a "STEP 0 — DECIDE the path" (NEW design → author full YAML +
`create_design`; EDIT → `get_design` first) and a hard completion criterion ("you are NOT done until
`apply_design(commit:true)` COMMITTED; a turn that ends after only searching is a FAILURE"). Build clean,
prompt tests pass (2/2). Re-testing GPT 5.5 on the moderate IoT-gateway prompt to confirm it now authors +
commits. This is a real agent-USABILITY fix for the preferred model — higher-value right now than another
per-sheet tweak, and it helps every model commit reliably (gpt-5.4 too).

**CONFIRMED (iter 22):** the prompt fix WORKED — GPT 5.5's re-test trace now shows `create_design →
validate_design (0 errors)` (it AUTHORS designs now; before the fix it only researched symbols and quit).
Full gate suite GREEN (sch-layout + circuit-graph + kicad-bridge all pass incl. the 7.6-min floorplan_netlist;
snapshot byte-identical). The run didn't reach the commit only because **GPT 5.5 is SLOW** — few tool calls
per 16-min window, so the timeout hit before apply; re-running with a 40-min cap to get a full GPT-5.5 board.
**FULLY CONFIRMED (iter 23):** the 40-min GPT-5.5 run completed the ENTIRE workflow — research →
`create_design` → `validate_design` (0 err) → `review_design` **score 10/10 "clean"** → `apply_design` → edit
→ apply → review. So GPT 5.5 not only authors but designs WELL (a 10/10 from the independent reviewer); the
prompt fix is fully validated end-to-end. It just persists no `.draft.yaml` because the 40-min timeout cut off
agent_design's exit-time copy — GPT 5.5 DID commit via apply_design (the temp board is lost on exit). Bottom
line: agent + GPT 5.5 works; for a critic-able board, run with a longer cap or save the draft earlier.
OPS NOTE: `pkill -f "agent_design"` self-kills the launching shell (its own cmdline contains that string,
exit 144) — never use it; and `timeout` kills `cargo` but not the child binary blocked on the slow API, so
old runs LINGER. Net: the agent now drives GPT 5.5 to author+validate designs; gpt-5.4 stays the fast path
for quick iteration. Engine unchanged (turn-19 state, all wins intact).

## Iteration 23 (2026-06-22) — snap_lone_decaps: tested, REVERTED — the co-placement wall is real

Looked at the IoT-gateway sensors sheet: the recurring "orphaned decoupling cap" — C5 (3V3 bypass for U4/BME280)
drifts to the far corner because the bank idiom (≥3 caps) doesn't fire for a lone cap. Tried a SURGICAL
post-pass `snap_lone_decaps` (gated MULTISHEET_REFINE, run last): a single 2-pin power+GND cap, not in a bank,
FAR from the IC sharing its power net → snap it beside the IC, using the engine's `is_power_net` (recognizes
3V3) WITHOUT the iter-20 idiom-graph change (so banks untouched). Snapshot byte-identical, build clean.

FAILED: C5 ended ~40mm from U4 still + wire-xings 0→2. The snapped spot beside U4 is OCCUPIED (the IC's area is
busy — which is WHY the cap was orphaned), so the body-decongest evicts it right back out. Reverted.

**CONSOLIDATED LESSON (turns 16, 20, 23):** targeted "snap part X next to part Y" post-passes FAIL in congested
sub-sheet areas — the target spot is taken and decongest re-scatters the move. The ONLY co-placement that stuck
was the CC-pulldown FREEZE (iter 17: RESERVE the cells during the search). The orphaned-decap would need the
same — a freeze that seats a lone bypass cap pin-adjacent — but firing the decoupling idiom for lone/VBUS/3V3
caps (to trigger the freeze) disturbs the working banks (iter-20 regression). So this defect is a genuine
co-placement-vs-congestion TRADE-OFF the local grind can't resolve cleanly: it's a **Tier S2 holistic-cost
problem** (balance pin-adjacency against congestion globally), not another post-pass. The per-defect grind has
hit its wall; the durable wins (multi-sheet, congestion relief, title-block, CC-pulldown, empty-band collapse,
power distribution, the GPT-5.5 prompt fix) stand, and the remaining uniform-9 gap is squarely Tier S2.

## Iteration 24 (2026-06-22, ULTRACODE) — Tier S2 foundation: workflow → pin_crowd cost term → REVERTED → refined lever

Ran the **tier-s2-foundation workflow** (wf_becb27fb-6fb: 4 high-effort research agents mapping the engine cost
@floorplan.rs:3140/4321, the human target distribution, the defect→cost gaps, and learn-from-examples field
approaches → xhigh synthesis). It produced a thorough, code-grounded plan (saved: docs/specs/tier-s2-pin-crowd.md):
the cost has the ATTRACTION half (`cohere`/`stray` pull satellites to their anchor pin) but is MISSING the
REPULSION half, so caps stack or stay orphaned and post-passes fight — add `pin_crowd` (a per-pin quadratic
crowding penalty) so the SA resolves the trade-off in the objective.

IMPLEMENTED `decap_cohesion` (DECAP_HUG_W=2.5 stronger lone-cap pull + PIN_CROWD_W=8 anti-stack), gated on
MULTISHEET_REFINE, added outside `layout_cost`'s base/multiunit (`+0.0` ⇒ snapshot BYTE-IDENTICAL, verified;
anneal netlist 2/2). RESULT — REVERTED: the IoT-gateway critic dropped EVERY sheet (controller 9→8, indicators
9→8, power_entry 7→6, sensors 8→7); and the target defect (orphaned C5) DIDN'T move (still ~47 mm from U4).

**THE REFINED LEVER (the real Tier S2 insight, deeper than the plan):** the orphaned-cap is a **NO-ROOM**
problem, not weak-pull. `signal_anchor_centroid` already targets C5 at U4's 3V3 pin and the pull fires — but
co-placing C5 there needs its body to OVERLAP the occupied area beside U4 (R10 + wires), and `overlaps` is a
1500× hard wall the hug can't beat; and there's no crowd to relieve (lone cap). And the *broad* hug/crowd term
disturbs the WORKING decoupling placements on the clean sheets (the iter-20-class regression, in cost form). So
the missing lever is **RESERVE-A-DECAP-SLOT beside each IC power pin DURING placement** (inflate the IC's
effective footprint by a decap keepout, so the search leaves room and the cap drops in without colliding) —
pin_crowd is only the anti-stack half. That's a PLACEMENT-STRUCTURE change (the IC footprint), not a cost
addend, and it's the precise next Tier S2 step (scoped in the spec). Net: a thoroughly-researched, cleanly
reverted negative result that converted "Tier S2 = vague learned cost" into a SPECIFIC, code-grounded next
lever. Engine back to the validated state (snapshot byte-identical, full gate suite green).

### Then INSTRUMENTED the reserve-a-slot lever — and PROVED the SA is at a local optimum (iter 24b)

Built `place_lone_decaps` (body-aware: move an orphaned non-idiom satellite to a VERIFIED-FREE slot beside its
anchor pin — can't disturb working sheets, can't be evicted; gated, snapshot byte-identical) and INSTRUMENTED it
(DECAP_DBG). The trace is decisive: **every orphaned satellite (caps AND pull-ups) already sits at its NEAREST
FREE slot** — the search either lands it back where it is (no closer slot exists; the inner rings around the IC
pin are all occupied) or finds NONE. Generalized it to ALL non-idiom satellites and critic-validated on the IoT
gateway: **controller 9→8, sensors 8→6** ("a knot of junctions around the two pull-ups") — perturbing the
satellites at all CREATES junction knots. Reverted.

**DEFINITIVE, INSTRUMENTED CONCLUSION (after 6+ validated attempts across iters 13-24):** the orphaned-satellite
defect is NOT a placement BUG — the SA's force-layout already seats every satellite at its nearest free slot
given the part density; the IC's adjacent area is genuinely full (its OTHER satellites + neighbors), and there is
no closer free slot. So: a cost-PULL disturbs working banks (iter20/24a); a post-pass MOVE either no-ops (already
optimal) or creates junction knots (iter24b); a snap gets decongest-evicted (iter23). The local levers are
EXHAUSTED — proven, not assumed. The defect is the SA's BEST under its density/compactness balance; the critic
wants pin-adjacency AND compactness AND no-knots simultaneously, which is a GLOBAL space-allocation+routing
optimum the greedy force-layout can't reach. **Uniform 9 needs a fundamentally better PLACEMENT ALGORITHM**
(field-survey rec: a learned-to-rank cost over the human corpus, or a global placer that co-optimizes
satellite-slotting + label-vs-wire routing) — a real research build, NOT another incremental term/pass. The
engine ships consistent 7-9/sheet (professional) and is at its incremental ceiling; that ceiling is now PROVEN
with instrumentation, and the next step is the genuine learned/global placement effort (scoped in
docs/specs/tier-s2-pin-crowd.md + the field survey).

## Iteration 25 (2026-06-22) — i2c_pullup IDIOM: a real WIN (the "ceiling" was wrong for un-idiomed shapes)

REFRAME that broke the deadlock: the instrumentation showed the orphaned satellites are exactly the ones
WITHOUT an idiom — the satellites that HAVE one (decoupling bank, crystal, cc_pulldown) co-place beautifully
(controller's "aligned decoupling bank" = 9). The cc_pulldown FREEZE already fixed the analogous "R2 exiled"
defect by reserving cells DURING the search. The sensors "pull-ups far below the IC, long detours" defect had
no idiom. So I built one.

**`I2C_PULLUP` idiom** (crates/circuit-graph/src/library.rs): an IC with two resistors, each from a DISTINCT
signal pin UP to a SHARED power rail — the SDA/SCL bus pull-up shape (mirror of CC_PULLDOWN, but rail=Power and
freeze with `Orient::Up`). New `place_i2c_pullup` (floorplan.rs) reserves the pair beside the anchor; gated into
the library behind MULTISHEET_REFINE so single-sheet references stay byte-identical.

FIRST CRITIC: sensors pull-ups FIXED ("pull-ups far below" → "proper vertical pull-ups"), BUT controller 9→8 —
the idiom MISFIRED on the ESP32's EN/BOOT control pull-ups (not an I2C bus), freezing them as a "sprawling
cluster." FIX: added a `PinsAtMost` NodePred (new — crates/circuit-graph/src/pattern.rs + matcher.rs) and capped
the anchor at 16 pins: a small peripheral (BME280 = 8) matches, a large MCU (ESP32 = 30+) does NOT. Re-audit:
i2c_pullup now fires ONLY on U4. Controller misfire gone.

VALIDATED + KEPT: sensors pull-ups are now a clean PARALLEL VERTICAL PAIR tapping up to 3V3 (human-conventional);
the critic's defect list flipped to "proper vertical pull-ups." Gates ALL green: circuit-graph 13/13 (added a
match-the-sensor-not-the-MCU regression test), placement_snapshot byte-identical, LAYOUT_SEARCH=anneal
floorplan_netlist 2/2, multi-sheet ERC errors unchanged (8→8, pre-existing in the LLM design). i2c_pullup fires
ONLY on small I2C peripherals so it touches NO other sheet (controller 8 / power_entry 6 are critic ±1-2
variance on byte-identical sheets, NOT regressions). A real, SAFE, GENERALIZABLE win (every I2C sensor): the
idiom-FREEZE lever works where cost-terms/post-passes failed, because it reserves space DURING the search
instead of fighting the SA after it. **Correction to iter-24's "ceiling":** the per-sheet tail is NOT a hard
ceiling for defects that have a recognizable IDIOM — the path for the local tail is MORE freeze-idioms for the
recurring un-idiomed shapes (next candidates: series-resistor-by-pin for SPI/UART, RC filters), and Tier S2 only
for the truly un-idiomed sprawl. Sensors' remaining cap is a minor "longish bottom GND run" (separate, next).

## Iteration 26 (2026-06-22) — LDO-pin: a 2nd WIN (pin the small decoupling anchor with its bank)

Next-lowest sheet: power_entry = 6 ("the LDO U2 isolated in empty space, far from the caps it serves"). AUDIT
showed the decoupling idiom DOES fire (anchor=U2, caps=C1/C2/C3) — but `place_decoupling` freezes only the CAPS
relative to the anchor's seed cell; the ANCHOR stays free, and a 3-pin LDO connects ONLY through power rails
(weak cohesion) so the SA drifts it off its own frozen bank. (A big IC like the controller's stays put because
its many pins give strong cohesion.)

FIX (floorplan.rs, decoupling match arm): when the decoupling anchor is SMALL (3-4 pins = an LDO/regulator),
PIN it with its bank by pushing it into the frozen `cells` at its seed cell. `orient_angle` returns 0 for any
non-2-pin part so there's no rotation. Gated on MULTISHEET_REFINE ⇒ single-sheet references byte-identical.

VALIDATED + KEPT: power_entry **6 → 7-8** — the critic's "isolated LDO" capping defect is GONE (U2 now sits in
its cluster; remaining is only "minor cap sprawl"/"mild U1/U2/D1 congestion", lesser issues). Fires ONLY on U2
(the sole 3-4 pin decoupling anchor; U3 has >16 pins), so it touches NO other sheet — controller 8 / others are
±1 variance on byte-identical sheets. Gates green: placement_snapshot byte-identical, anneal floorplan_netlist
2/2, ERC errors unchanged 8→8. **Two idiom/freeze wins this session** (i2c_pullup + LDO-pin) confirm the lever:
recurring per-sheet defects with a recognizable structure are fixable by reserving/pinning placement DURING the
search. Remaining power_entry lever: pull the cap BANK up to the pinned LDO (place_decoupling seats the bank in a
bottom band, not adjacent) — a place_decoupling change, next.

## Iteration 27 (2026-06-22) — cap-bank-above-LDO: power_entry to a SOLID 8 (3rd win)

Completing iter-26: with U2 pinned, its caps still sprawled because `place_decoupling` seats the bank at
`col = acol - (n+1)` — far to the LEFT of the anchor (often NEGATIVE columns → flung to the spare-column area).
A tall IC's bank is fine there, but a small LDO is the same width as its caps, so the bank lands far away. FIX
(place_decoupling): for a SMALL anchor (3-4 pins, multi-sheet) seat the bank at `base = 0` — DIRECTLY ABOVE the
pinned LDO — so the cap row hugs it (the textbook compact power-entry block). U2↔C1 went ~50 mm → ~16 mm.

VALIDATED + KEPT: power_entry **6 → 8** (samples=3): "clean, conventional, proper in-line caps/resistors,
consistent power symbols; only mild CC-pulldown rail length keeps it from 9." BOTH the isolated-LDO AND the
cap-sprawl defects are gone. Gates green: placement_snapshot byte-identical (base change gated on small_anchor =
multi-sheet only), anneal floorplan_netlist 2/2, ERC 8→8. Fires only on U2 (the sole small decoupling anchor).

**THREE wins this session** (i2c_pullup, LDO-pin, cap-bank-above-LDO) — the freeze/pin-idiom lever is robust.
IoT gateway now: controller 8-9, indicators 9, power_entry **8**, sensors 8, storage 8 (avg ~8.4, every sheet
≥8). Remaining caps: power_entry's "mild CC-pulldown rail length" (R1/R2 far from J1's CC pins — a cc_pulldown
placement tweak), sensors' "longish bottom GND run", controller's "one long detour net". Each is now a MINOR,
single-named defect — the worst structural defects (orphaned pull-ups, isolated LDO) are SOLVED.

**GENERALIZATION CHECK (iter 27) — the wins hold on a 2nd, untuned board.** Re-critiqued the CAN node (live3,
STM32+CAN+USB-LDO, a board the idioms were never tuned for): power_entry **8** (LDO fixes fired on U1),
mcu_core **9** (aligned decoupling + crystal), can_interface 7 (a CAN termination/connector knot — a DIFFERENT,
un-idiomed defect), sensors **8** (i2c_pullup fired on U4/R7/R8). Avg ~8.0, up from the ~7.75 baseline; every
sheet ≥7. i2c_pullup + the LDO pair fire and help on a board they were never tuned for ⇒ the freeze/pin-idiom
lever is GENERALIZABLE, not overfit to the IoT gateway. Next un-idiomed structural defects surfaced: CAN
bus-termination cluster (can_interface), and a pull-up junction cluster when i2c pull-ups bank tightly (refine
the i2c_pullup junction spacing).

## Iteration 28 (2026-06-22) — FRESH-BOARD generalization: uniform 8/10, + two reverted no-ops

Continuous-work session (user: "iterate until Jun 25, don't stop each iteration"). Two experiments tried + cleanly
REVERTED (kept the engine clean): (1) `single_bypass` idiom for the lone-decap — fired correctly after excluding
diode/ESD arrays, but the freeze cell lands in the rail band (doesn't hug the anchor like the LDO case) and added
14 warnings; the lone-decap stays in the hard bucket. (2) `collapse_empty_columns` (horizontal sprawl collapse) —
NEVER fires on live2 (verified via COLLAPSE_DBG): the sub-sheet sprawl is EDGE space (reframe handles it), not
between-cluster gaps, so horizontal gaps are rare (the engine stacks clusters vertically). A no-op ⇒ reverted.

**KEY RESULT — generated a FRESH dense board (gpt-5.4, applied=true): USB-C STM32L4 logger (crystal+LDO+
microSD-SPI + THREE I2C devices BME280/SHT31/DS3231-RTC + coin-cell + 2 LEDs) — a board the idioms were NEVER
tuned for. ALL idioms fired (cc_pulldown, crystal, decoupling×2, led_indicator×2, i2c_pullup). Critic: usb_power 8,
mcu_core 8, storage 8, sensors_rtc 8 = UNIFORM 8.0**, every sheet "clean, conventional," the idioms explicitly
praised ("tidy decoupling bank", "proper in-line passives", "vertical decoupling taps", "horizontal I2C bus").
The engine now ships a CONSISTENT, professional 8/sheet on arbitrary dense boards — the structural defects are solved.

**The 8→9 gap is now a small set of RECURRING MINOR defects** (each "only X keeps it from publishable"): (a) the
I2C PULL-UP junction knot (sensors_rtc + live3 sensors — the bus + 2 pullups + multiple devices congest at the
tap junctions); (b) cluster SPRAWL / wide empty regions (usb_power, power_entry); (c) density crowding (mcu_core
crystal/LED/power cluster). (a) is the most recurring and the next target (i2c_pullup junction placement). These
are aesthetic-tail defects near the deterministic engine's ceiling; the freeze-idiom lever solved the structural ones.

### `regulator` idiom (2-cap LDO) — tried, REVERTED, and it revealed the next BOUNDARY

The 2-cap LDO (input+output cap) is a real gap (DECOUPLING needs ≥3 caps, so usb_power/live3 LDOs sprawl from
their caps). Built a lib-scoped `regulator` idiom (LDO + Vin-cap + Vout-cap, pin LDO + caps above). It FIRED
correctly (live3 U1+C1+C2 co-placed, ~13 mm; did NOT fire on live2's 3-cap LDO ⇒ DECOUPLING claimed first). BUT
the critic DROPPED live3 power_entry **8→7**: "a sprawled, label-only LDO/cap block that breaks the power-flow
reading." REVERTED.

**THE BOUNDARY (this turn's key insight):** co-placing the LDO+caps as a tight ISLAND pulls the LDO OUT of the
power-flow path (USB→LDO→rail); because it connects via distributed rail LABELS, the island reads as disconnected
and the critic penalizes the broken flow MORE than it rewards the tidy block. So the remaining 8→9 defects are NOT
local-co-placement problems — they are GLOBAL placement problems: (a) LDO must sit IN the USB→rail flow, not be
boxed; (b) I2C bus = devices in a ROW so the shared bus is one clean line; (c) even part distribution vs sprawl.
The FREEZE-IDIOM lever (3 clean wins: i2c_pullup, LDO-pin, cap-bank-above) is now EXHAUSTED — confirmed by 3
consecutive reverts (single_bypass, collapse_columns, regulator) all failing for the same reason: they impose a
LOCAL arrangement that fights the GLOBAL flow/topology. Path to uniform-9 = GLOBAL placement (signal-flow ordering
+ bus-row alignment), i.e. Tier S2 / a flow-aware placer — a structural effort, not another local idiom. The
engine ships a validated, generalized UNIFORM 8/sheet; that is the local-idiom ceiling.

## Iteration 29 (2026-06-22) — IC-less cap-bank row: a 4th WIN (the "ceiling" wasn't fully reached)

Generated a 2nd fresh validation board (motor controller: DRV8301 gate driver + op-amp current sense + CAN +
USB-C/buck — a totally different topology). Critic: 8,8,**6**,6,8,9. The **6** (mcu_core_2) was a SYSTEMATIC defect:
when the multi-sheet split lands a block of decoupling caps on a sheet WITHOUT their IC (here 5×100nF + a CAN
connector, no MCU), the decoupling idiom can't fire (no anchor) and the caps "scatter at random offsets instead
of an aligned bank."

ROOT CAUSE (instrumented via CAPROW_DBG): `align_rail_cap_rows` DOES fire and row them — but it ran BEFORE
`decongest_off_labels`, which then re-staggered the row to separate the power-symbol labels (PITCH=7.62 packed
them too tight). FIX: (a) widen PITCH 7.62→12.7 (clears the value/refdes labels), (b) run `align_rail_cap_rows`
as the LAST placement word (moved into the MULTISHEET_REFINE finalize block, after decongest_off_labels +
collapse), so the bank stays aligned. **mcu_core_2 6→8** ("clean, well-aligned decoupling-and-CAN-connector
sheet"). Fires ONLY on IC-less cap sheets (verified: 1× on the motor board, 0× on live2/live3) ⇒ touches nothing
else; snapshot byte-identical; anneal netlist 2/2. A real, generalizable win (any multi-sheet split that orphans
a cap bank from its IC).

**Correction:** the "uniform-8 is the local ceiling" claim was slightly premature — this was a genuine LOCAL win
(a timing/tuning fix, not a global placer). Lesson reaffirmed: keep generating fresh boards — new topologies
surface systematic local defects the 2-board set never hit. Remaining motor low sheet: current_sense=6 (op-amp
text/value collision + bare output pin + vertical sprawl — an op-amp feedback idiom candidate, next).

## Iteration 30 (2026-06-22) — 3rd fresh board (AUDIO) + multi-unit grid revert; the 5-board picture

(a) Tried a 2-wide GRID seed for multi-unit parts (the current_sense quad-op-amp vertical sprawl). REVERTED:
6→5 — the grid seed got un-done by the SA (scattered with gaps). 5th confirmation of the design rule: arrangements
the SA undoes revert; only freeze-during-search or last-pass wins stick. The multi-unit arrangement is therefore
a GLOBAL (seed+freeze) problem too, not a local seed.

(b) Generated a 3rd fresh board — USB AUDIO interface (STM32 + PCM5102 I2S DAC + TPA6132 headphone amp + op-amp
buffer + RC filters + jack), another new topology. All idioms fired (cc_pulldown, led_indicator, crystal,
decoupling×6). Critic: usb_power 8, mcu **9**, dac 8, analog_out 8 = avg 8.25, every sheet ≥8. No systematic LOW
defect (unlike the motor board's mcu_core_2=6 that became win #4) — just "minor cosmetics" (sprawl, loose cap
clusters, dog-legs).

**THE 5-BOARD VALIDATION PICTURE** (IoT gateway, CAN node, logger, motor, audio — all dense, multi-sheet, LLM-
authored, mostly UNTUNED): every board ships a CONSISTENT 8-9/sheet, every sheet ≥7, the great majority 8, several
9. The four freeze/last-pass wins (i2c_pullup, LDO-pin, cap-bank-above-LDO, IC-less cap-row) GENERALIZE across all
of them. The engine is a validated, professional uniform-8-with-9s on arbitrary dense boards. The remaining 8→9
gap is uniformly the COSMETIC/GLOBAL tail (sprawl, loose clusters, bus knots, LDO-in-flow, multi-unit blocks) —
all of which the 5 reverts proved need the flow-aware SEED+FREEZE placer (docs/specs/flow-aware-global-placement.md),
the one lever that works WITH the SA at global scale. That is the scoped, deliberate path to uniform-9.

## Iteration 35 (2026-06-22) — 6th board (RELAY, repeated motifs) + decongest-respects-frozen revert

Generated a 4th fresh validation board: 4-channel RELAY control (STM32 + 4 identical relay channels: driver
transistor + base R + flyback diode + relay + screw terminal + LED). The LLM split the 4 channels into separate
sheets. Critic: power_entry 8, mcu_core 7, relay_ch1-4 all **9** (the repeated-motif channels are PERFECT — clean
series/tap orientations, no defects). 6th board validated uniform 8-9; the engine handles repeated motifs cleanly.

mcu_core=7 defect: "scattered (non-banked) decoupling caps + congested crystal cluster". Investigated: the
decoupling bank IS detected (idiom on U1, C6-C9) but placed scattered because the CRYSTAL cluster competes for
the space above the MCU. Tried `decongest`-respects-frozen (don't let decongest shove the frozen bank) — REGRESSED
6 (the frozen caps were already placed scattered by the congestion, so pinning them just sprawled the rest). The
root is placement ORDERING (crystal idiom placed first occupies the band the decoupling bank wants), a global
placement-ordering issue — NOT a decongest fix. Reverted (10th revert this session).

**SESSION CLOSE PICTURE (6 boards, ~30 sheets):** 4 freeze/last-pass wins generalize; the engine ships uniform
8-9/sheet (most 8, many 9, the cleanest motif sheets a perfect 9) on arbitrary dense LLM-authored boards. The
remaining 8→9 gap is uniformly placement-ARCHITECTURE: bus-device rows (needs SA rigid-cluster + pervasive
freeze-respect across 6 passes, exhaustively mapped iter 31-34), crystal-vs-decoupling space competition (needs
flow/ordering-aware block placement), and subjective critic cosmetics on already-correct layouts. 10 reverted
experiments left the engine byte-identical and green throughout. The frontier is the flow-aware placement rewrite
(docs/specs/flow-aware-global-placement.md); the four wins + 6-board uniform-8-with-9s validation are the durable result.

## Iteration 37 (2026-06-22) — 7th board (ETHERNET SBC) + LDO-threshold revert

Generated a 7th fresh board — STM32H7 single-board computer with a LAN8720 Ethernet PHY (RMII), a 3V3 BUCK + a
1V8 LDO, USB-C, microSD. GPT 5.5 was tried first (the preferred model) but STALLED again (applied=false after 25
research calls — the documented research-stall); gpt-5.4 produced it reliably. Critic: ethernet_phy **9** (the
most complex NEW structure — PHY + RMII signals + decoupling — is publishable!), mcu_1 8, regulators 7. The engine
handles a genuinely new topology (PHY/RMII, dual regulators, magnetics) cleanly.

regulators=7 defect: the 1V8 LDO U3 (5-pin, with EN) is loose/isolated from its (well-aligned) cap bank. This is
the LDO-pin win's target, but U3 has 5 pins and the LDO-pin/cap-bank-above thresholds are 3..=4. Extended both to
3..=6 → U3 co-placed (xings 2→0) BUT critic REGRESSED 7→5 ("caps scattered, EN stub exposed"): the cap-bank-above
places the bank directly above the anchor, which works on a SIMPLE power_entry sheet but COMPETES with the buck +
inductor on this busy dual-regulator sheet. Other 6 boards unchanged (ERC identical). Reverted (13th revert).

Also tried (and reverted, 12th) an agent-loop "nudge" to push GPT-5.5 past its research-stall — it broke 6 history
tests (re-prompt adds messages the tests assert against); not worth breaking the gate for a model-behavior fix.

**7-board picture:** uniform 7-9/sheet, the great majority 8-9, the cleanest sheets (relay channels, this PHY) a
perfect 9. The engine GENERALIZES across IoT/CAN/logger/motor/audio/relay/Ethernet — wildly different topologies.
The residual 7-8 sheets are busy-sheet CONGESTION (dual regulators, crystal-vs-decoupling) where co-placement
can't fit, i.e. the geometric/architecture frontier (incremental rewrite gains). 4 wins + 13 reverts; engine green.

## Iteration 38 (2026-06-22) — 8th board (FPGA, multi-unit) + multi-regulator scatter identified

8th fresh board: Lattice iCE40 FPGA dev board (USB-UART FT232, dual LDO, SPI flash, GPIO headers). Critic:
**fpga_core_1 = 9** — the engine handles the **4-unit iCE40 multi-unit symbol CLEANLY** (tidy aligned pin banks,
in-line decoupling) — a strong result, since multi-unit was a noted limitation. config_flash 8. power_entry **6**.

KEY FINDING — the recurring LOW-sheet defect across boards is the **MULTI-REGULATOR SCATTER**: a power sheet with
2-3 regulators (FPGA power_entry has 3 LDOs; Ethernet regulators has buck+LDO) flings them "to separate corners
with label-only connectivity." ROOT CAUSE: the decoupling idiom fires for ONE anchor only (`best_decoupling_anchor`
picks the single best), so only one regulator's bank is recognized/pinned — the others' caps are never banked and
the SA drifts each regulator off. The LDO-pin win fixes the SINGLE-regulator case; multi-regulator needs the idiom
to fire PER regulator (each LDO + its own bank, pinned), then arrange the blocks — a multi-anchor extension.

Tried two fixes, both reverted byte-identical:
- Large-bank ROW-WRAP (place_decoupling, n>8 → stacked rows): synthetic 12-cap test → critic 6 ("staggered grid
  instead of one tidy aligned row" — the critic PREFERS a single row; the real issue is decongest stagger, not
  width). 14th revert. (Also: the real FPGA distributed its caps across units, so no single big bank existed.)
- Single-IC 5-pin-LDO threshold (3..=4 → +5 when lone IC): a NO-OP on the test set (both low sheets are MULTI-
  regulator, so single_reg=false correctly), and it doesn't address the multi-regulator scatter. 15th revert
  (verified regulators held at 7 — no regression).

**8-board picture:** uniform 6-9/sheet, the great majority 8-9, perfect 9s on the hardest structures (relay
channels, Ethernet PHY, multi-unit FPGA). The ONE recurring sub-8 defect is now precisely root-caused: multi-
regulator power sheets need per-regulator decoupling banking (a bounded, named next lever — NOT the open-ended
placement rewrite). 4 wins + 15 reverts; engine green throughout.

### Correction (iter 38): multi-regulator scatter IS the global frontier, not a bounded lever

Traced deeper: the FPGA power_entry decoupling matched ONLY U1 (shared-rail caps attribute to one anchor), and the
critic flags the LDOs THEMSELVES "flung to separate corners" — so the regulator ANCHORS are dispersed by the SA,
not just their caps. Per-regulator decoupling banking would only tidy the LOCAL grouping (caps↔their LDO); it would
NOT pull the spread anchors together. So the multi-regulator scatter is the SAME global cluster-placement frontier
as the bus-row: keeping multiple RELATED ANCHORS together through the SA + finalize. Confirmed: ALL recurring sub-8
defects (bus rows, multi-regulator power sheets, crystal-vs-decoupling) reduce to ONE root cause — the engine pins
single small clusters (one decoupling bank, one crystal, one CC-pulldown) but has no rigid MULTI-ANCHOR cluster, and
the five+ placement passes don't uniformly honor a freeze for large anchors. That is the flow-aware placement
rewrite (docs/specs/flow-aware-global-placement.md), now triply-confirmed (bus / regulators / crystal) across 8 boards.

## Iteration 39 (2026-06-22) — regulator-column compaction: final-pass does NOT scale to multi-anchor

Pushed on the multi-regulator scatter with `compact_regulators` — the cap-row/rigid-cluster pattern (run DEAD LAST,
overlap-safe) applied to PULL multiple regulators + their satellites to a common column. Result: REGRESSED Ethernet
regulators 7→6 (the buck's cluster + the LDO collide at the shared column = "congested knot") and did NOT help the
FPGA power_entry (still 6 — the 3 LDOs don't fit a clean column). 16th revert, byte-identical.

**CONCLUSIVE:** the rigid-cluster FINAL-PASS — which WON for single small cap banks (align_rail_cap_rows) — does NOT
scale to MULTI-ANCHOR clusters (bus devices, multiple regulators). Two independent confirmations now: align_bus_clusters
(no-op, didn't fit) and compact_regulators (regresses, collides). A post-hoc pass can't make room a dense sheet doesn't
already have; only SA-INTEGRATED placement can reserve space for a multi-anchor cluster from the start. This is the 4th
line of evidence (bus geometry + regulator collision + crystal competition + final-pass-can't-make-room) all pointing
to the SAME conclusion: uniform-9 on multi-anchor power/bus sheets = the flow-aware placement rewrite, not any further
finalize-pass trick. The 4 single-cluster wins are the ceiling of what the freeze/last-pass architecture can deliver.

## Iteration 40 (2026-06-22) — bus-cluster soft cohesion: clusters anchors but STRANDS their caps

Attempted the most promising rewrite-adjacent idea yet: extend the EXISTING, WORKING multi-unit cohesion (the soft
cohere term that makes a multi-unit FPGA place as one block, scoring 9) to BUS clusters — anchors of different
refdes sharing ≥2 signal nets (I2C SDA+SCL). A soft SA pull, NOT a rigid block or post-pass (both of which the
layered pipeline defeats). Result: it DID cluster the devices (U4/U5 to a common x), but REGRESSED both bus sheets
(live3 sensors 8→6, fresh1 sensors_rtc 8→7) — the decoupling caps STRANDED far from their now-moved ICs.

ROOT CAUSE (the decisive insight): a decoupling cap coheres to "nearest V+ pin", which in a multi-DEVICE cluster
flips ambiguously between the clustered ICs — so pulling the anchors together does NOT carry their satellites. The
multi-unit cohesion works precisely because units of ONE chip have NO separate per-unit caps. So: soft cohesion
clusters anchors but can't move their caps with them; the rigid block (iter 34) carries caps but finalize un-does
it; the final-pass (iter 36/39) can't make room. 17th revert. ALL THREE mechanisms fail for the same structural
reason — a multi-anchor cluster is {anchors + each anchor's own satellites}, and NO single existing lever moves that
compound unit coherently through the SA AND finalize. The rewrite must introduce a first-class CLUSTER object
(anchors + their satellites) that every pass treats atomically. That is the precise, now-fully-bounded spec.

## Iteration 41 (2026-06-22) — 9th board (BLDC driver): the multi-anchor frontier is the DOMINANT limiter on hard boards

9th board: 3-phase BLDC motor driver (STM32 + 3-phase gate driver + 6 MOSFETs in 3 half-bridges + 3 current-sense
amps + CAN + buck). This is the HARDEST topology tested — 3-phase ⇒ everything is a REPEATED MULTI-ANCHOR MOTIF.
Critic: gate_drive **6** (3 bootstrap stages scattered), power_stage **5** (3 half-bridges "scattered in different
orientations rather than drawn as repeated columns"), current_sense **5** (sprawl + LLM-design dangling op-amp out).
The LOWEST board this session.

**This REFRAMES the rewrite's impact — it is NOT "incremental":** on EASY/medium topologies (the first 8 boards) the
multi-anchor sheets are a minority, so the engine averages 8-9 and the frontier costs ~1 point on a few sheets. But on
a HARD topology where MOST sheets are repeated multi-anchor motifs (any 3-phase / poly-phase / multi-channel power
board), the SAME frontier dominates and the engine drops to **5-6**. So the multi-anchor cluster placement rewrite is
the difference between 5-6 and 8-9 on the hardest boards — a MAJOR, not incremental, lever. The "repeated columns"
phrasing the critic uses thrice (relay/bootstrap/half-bridge) names the exact target: detect a repeated motif (N
identical {anchor+satellites} clusters) and lay them out as N ALIGNED COLUMNS, same orientation. This is the
first-class CLUSTER object (iter 40) plus a REPETITION-alignment step. 4 wins + 17 reverts; engine green.

**Honest 9-board verdict:** the engine is professional-grade (8-9) on the ~80% of topologies dominated by single-
anchor structures, and drops to 5-6 on multi-anchor-motif-heavy boards (3-phase drivers). The single rewrite —
first-class clusters + repetition-aligned columns — is the high-impact path to uniform-9 across ALL topologies.
