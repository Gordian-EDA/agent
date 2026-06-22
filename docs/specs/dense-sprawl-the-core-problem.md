# Dense-board sprawl — the core quality problem (diagnosis + what's ruled out)

Status: **diagnosed, structural fix open** (2026-06-22). The #1 blocker to "9+ on hard dense
circuits." Engine: `crates/sch-layout/src/floorplan.rs`. Measure with `tools/layout_metrics.py`.

## The problem (objective + faithful)

On dense agent boards (40–60+ parts) the engine SPRAWLS: parts are placed in tight local
clusters but the clusters float far apart, leaving 2–9× the human whitespace-per-part.

| board (real agent output) | parts | sprawl | human target | critic (samples=3) |
|---|---|---|---|---|
| stm32 | 19 | 63 | ~23 | 6/10 |
| esp32 | 29 | 80–105 | ~23 | 5/10 |
| audiocodec | 37 | 37 | ~23 | 5/10 |
| motordrv (hard) | 46 | 98 | ~23 | TBD |
| datalogger (hard) | 43 | 205 | ~23 | TBD |

Faithful critic (samples=3, clean-name real output) names it every time: *"decoupling caps and
pullups stranded far from the parts they serve"*, *"scattered islands across a vast canvas"*.
`comp` is always the worst dimension (3–5). Our wires are SHORT (median 3.8mm < human 5.1) and
local — only the GLOBAL arrangement is wrong.

## Root cause

1. **Force-layout leaves whitespace by construction** (parts repel to clearance, attracted only
   by shared-net wirelength). With DISTRIBUTED power (few inter-cluster wires) nothing pulls
   clusters together → they drift to the weak global `spread` term's equilibrium.
2. **The ship decision is crossings-first.** `Anneal::search` final `score()` = `(breaks,
   warnings, total_crossings, premium_cost)` — lexicographic. Spreading parts apart REMOVES
   crossings, so a sprawled layout BEATS a compact one in the pick. The cost actively *rewards*
   sprawl. (Confirmed: `PREFER_COMPACT` selection weight had zero effect — all generated
   candidates are sprawl 60–205; there is no compact candidate to select.)

## Ruled out (measured, do not re-tread — see `docs/experiments/quality-loop-log.md`)

- **Post-hoc block re-pack / `compact_blocks`** — gutters between many small blocks SPREAD faster
  than the centroid-pull compacts; sprawl went UP. esp32 rejected (block moves create shorts).
- **`proxy_cost` spread weight** 0.45→1.6/0.8 — helped stm32, REGRESSED esp32 (chaotic SA
  sensitivity; doesn't generalize — the constant-tuning trap).
- **LLM soft zones (`zbias`)** — sensible plan but soft bias loses to spread/hpwl + pick has no
  zone term → critic worse on all 3.
- **LLM authored `layout:` grid** — neutral (esp32 8=8) + audio grid added 24 ic_crossings.
- **Candidate-selection by compactness (`PREFER_COMPACT`)** — no effect (no compact candidate).
- **Naive shadow compaction** (push-to-touch) — would remove the GOOD inter-block gutters too →
  a tight but JUMBLED layout (human whitespace is intentional *between* blocks, not within).

## The structural fix (open)

The layout is a **mix of local (tight clusters) and global (flow-ordered, gutter-separated
blocks)**. The force-layout gets local right, global wrong. Need a **two-level / block-aware**
placement where the GLOBAL arrangement is optimized at the block level:

1. **Coarse, correct partition** — group ALL parts into a SMALL number of functional blocks
   (target #clusters ≈ 0.27×parts; my `build_functional_blocks` over-fragments → merge tiny
   blocks / connected-components on signal nets). The fine partition was a key failure cause.
2. **Compact each block internally** (already OK — local clusters are tight).
3. **Arrange blocks** flow-ordered (L→R by signal depth) with ONE consistent gutter (~12–16mm),
   no overlap — and CRUCIALLY make the ship decision accept this: the crossings-first `score()`
   must trade a crossing budget for compactness (the faithful critic tolerates a few crossings
   FAR better than sprawl — `rout` 6 vs `comp` 3). This is the missing cost lever.
4. Candidate options: (a) **sequence-pair / B*-tree** encoding so every search state is a compact
   packing by construction (Tier S1 — biggest, most robust); (b) a block-level placement pass
   feeding the existing SA a tight seed + a compactness-accepting pick. Start with (b) behind an
   env gate, validate sprawl→~23 on the battery with `layout_metrics.py` AND faithful critic
   (samples≥3), promote once it wins.

## KEY ENABLING INSIGHT (use this for the implementation)

**Lever A makes tight block-packing SAFE and gutter-free.** The earlier `compact_blocks`/Lever-B
attempts failed partly because moving blocks rigidly re-routed inter-block WIRES into geometric
shorts (the truthfulness gate then reverted them — esp32). But Lever A (now @70mm) already converts
long inter-block connections to NET LABELS. With labels carrying all inter-block connectivity there
are **no inter-block wires** → (a) moving/packing blocks can't create inter-block shorts, and (b) no
routing channel is needed between blocks, so the gutter can be SMALL (the gutters were what made
compaction *increase* sprawl). So the recommended structural path is:

1. Lever A ON (label inter-block nets — already shipped).
2. COARSE partition into ~0.27×parts functional blocks (merge singletons into their nearest
   anchor's block; `build_functional_blocks` is the seed but MUST be coarsened).
3. Pack block bboxes tightly (small uniform gutter ~6mm — labels not wires fill the gaps),
   flow-ordered L→R, preserving each block's tight internal layout (rigid translate).
4. SHIP IT DIRECTLY for dense boards (gated), bypassing the SA's crossings-first re-sprawl — the
   block arrangement is deterministic and Lever-A-safe. Validate sprawl→~23 (`layout_metrics.py`)
   + faithful critic (samples≥3). Only the intra-block crossings remain (few, local).

This is the most promising concrete next implementation; it threads between every failure mode found.

## Hard gates (unchanged)
`LAYOUT_SEARCH=anneal floorplan_netlist` (connectivity) + `placement_snapshot` (≤34-pin refs
byte-identical) + faithful VLM critic (samples≥3 on real `agent_design` output, NOT lifted yaml).
