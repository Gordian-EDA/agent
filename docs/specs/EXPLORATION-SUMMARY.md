# Schematic-quality exploration — summary (what works / what doesn't)

Two arcs were explored to raise auto-pcb schematic quality. This condenses both and states, honestly,
what works and what doesn't.

## TL;DR
1. **Constraint-based PLACEMENT (`cola`)** — a research success but ~nil product value: the engine is
   competitive-to-better, but the architecture pre-solves the problem it targets.
2. **Agent DESIGN-correctness review** — the real win: a hybrid reviewer + fix loop that cuts the
   functional-defect rate **~75% → ~12%** on a fleet, validated against ground truth. With clear limits.

---

## Arc 1 — constraint-placement engine `cola`  (branch `experimental/cola-engine`)

**Works:** a from-scratch VPSC block solver + constrained stress-majorization + label-aware non-overlap
+ crossmin signal-flow + per-IC cap-bank, in a pure crate (11 tests), integrated never-regress behind a
3-way A/B. It **matches** the mature SA on small sheets and **beats** it on large un-partitioned sheets
(single-sheet 62-part board: cola 7 vs SA 5; 61 vs 169 crossings).

**Doesn't (for the product):** the agent self-partitions every design into functional blocks ≤16 parts,
so the engine **never receives a large sheet** — cola's global-optimisation edge never materialises.
Net product value ≈ nil. The crate stands as a reusable asset if the architecture ever changes
(un-partitioned large sheets / single-page-overview mode). Detail: memory `cola-engine-state.md`.

---

## Arc 2 — agent design-correctness review  (branch `experimental/agent-design`)  ← the productive arc

**The gap (validated empirically):** the strong model (opus-4-8) produces electrically *complete*
designs (decoupling / ESD / bias present) that pass ERC and look clean — but **~75% have a FUNCTIONAL
defect** (pin-function mis-wire, 5 V part on a 3.3 V rail, open-loop topology, reversed polarity, wrong
feedback divider). These are invisible to KiCAD ERC (connectivity is fine) and to the layout critic.

### What works
- **`tools/design_critic.py`** — an electrical reviewer of the *netlist* (not the render). Validated:
  caught a hand-confirmed ATtiny ISP mis-wire (4/10), scored good designs 9–10 with **zero false
  positives**.
- **Hybrid reviewer = diverse-lens LLM ensemble + deterministic exact-math ERC.** Measured against
  GROUND TRUTH (inject known defects, `tools/inject_defect.py` + `recall_harness.py`):
  - **~90 %+ recall on real defects** across 5 defect types (pinswap / disconnect / value / railswap /
    reverse); **no false-fixes** on correct designs.
  - **Diverse lenses beat repeated sampling** (recall 76 % → 88 % over 1→3 lenses; repeated same-prompt
    is flat — it can't recover a *consistent* miss).
  - **Deterministic ERC** (`circuit-lang/src/erc.rs`, 6 checks + 13 tests: feedback-divider ratio, LED
    current, dangling/shorted part, crystal load caps, reversed polarity, V12/V33 rails) is exact,
    zero-variance, and **recovers a class the LLM ensemble consistently missed** (reversed polarity).
- **Productionised** as `Agent::run_turn_reviewed` (a reusable review→fix loop; `design_review` example;
  TUI shows the verdict). **Fleet product impact (8 diverse circuits):** designs with a critical/major
  functional defect **6/8 (75 %) → 1/8 (12.5 %)**; mean design-quality score **6.0 → 7.6**. Wins:
  can 5→10, rs485 5→10, charger 6→9, nrf 7→8.

### What doesn't work (honest limits + failures)
- **High-confidence-only fix threshold lets real defects slip.** The loop only feeds back HIGH-confidence
  critical/major defects (for FP-aversion). A genuinely critical defect the reviewer is only
  *medium*-confident on persists — fleet failure: **usbuart** had D+/D- swapped (USB won't enumerate),
  rated critical/*medium*, and survived 2 fix rounds.
- **The loop can thrash / degrade.** On that hard case the fix attempts *lowered* the score (4→3). There
  is **no keep-best regression guard** — a bad fix is committed anyway.
- **It fixes flagged defects but doesn't lift a mediocre design.** `audio`: the flagged defect was fixed
  (1→0) but the score stayed 4 (sub-threshold issues remain).
- **Neither layer is complete.** LLM recall isn't 100 % (rare/subtle/quantitative misses); the
  deterministic ERC has *limited coverage* (clear-rail dividers, common topologies — ambiguous rail
  names fall back to the LLM).
- **Measurement is one distribution.** Recall is on *injected* defects into *curated* fixtures, and the
  fleet is small (8). Real-world defect distributions will differ. Also note measurement near-circularity
  (LLM-graded), mitigated by the ground-truth injection but not eliminated.

### Recommended next (highest-value, not yet done)
1. **Feed back medium-confidence CRITICAL defects** (catastrophic-impact even at lower confidence — would
   catch usbuart's D+/D-), accepting some FP cost.
2. **Keep-best regression guard** in the loop (don't commit a fix that lowers the review score).
3. **Wider ground-truth corpus** — real failure modes, more part families — before trusting the % numbers.

---

## Assets (branch `experimental/agent-design`)
- `crates/circuit-lang/src/erc.rs` — deterministic exact-math ERC (13 tests)
- `crates/agent/src/review.rs` + `Agent::run_turn_reviewed` — hybrid diverse-lens + ERC review→fix loop
- `tools/design_critic.py`, `tools/inject_defect.py`, `tools/recall_harness.py` — measurement harness
- examples: `design_review` (the loop), `erc_check` (run the deterministic layer on any design)
