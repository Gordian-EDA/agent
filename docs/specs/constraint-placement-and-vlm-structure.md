# Constraint-based placement (D) + VLM-supplied structure (E)

A research-grounded design for breaking the placement ceiling.

> **STATUS (2026-06-21): EXPLORATION COMPLETE — both D (cola) and E (VLM-structure) built, validated,
> and concluded on branch `experimental/cola-engine`.**
>
> **Built:** the `cola` crate (VPSC block solver + constrained stress-majorization + label-aware
> non-overlap + crossmin signal-flow ordering + per-IC cap-bank; pure, dependency-free, 11 tests),
> integrated into sch-layout behind a never-regress 3-way A/B (shelf/crossmin/cola), plus a Phase-3 VLM
> structure POC (`tools/vlm_structure.py` → soft `COLA_VLM` flow/alignment constraints). All env-gated;
> `placement_snapshot` byte-identical throughout.
>
> **Result — both paths MATCH the mature pipeline but cannot beat it, for one architectural reason.**
> On the small sheets the pipeline produces, cola TIES the SA (dense MCU 8=8, gate-driver 7=7, BGA 9=9).
> On LARGE un-partitioned sheets cola WINS (6–7 vs 5 — global optimisation out-routes the SA's local
> search; 61 vs 169 crossings). BUT the **agent self-partitions every design into functional blocks
> ≤16 parts**, so the engine never receives a large sheet — cola's edge never materialises. The
> VLM-structure POC emits *sensible* structure, yet ADDING it regresses already-good sheets (8→6/7):
> no headroom, because small-sheet placement is already near-optimal. ⇒ **The quality lever is the
> AGENT's design / partitioning, UPSTREAM of placement — not the placement engine.** A from-scratch
> constraint engine and a VLM structure pass, both matching the tuned pipeline within days, is the
> honest, negative-with-a-clear-reason research result.
>
> **Reusable if the architecture ever changes** (large un-partitioned sheets, or a single-page-overview
> mode) — where both demonstrably DO help: the `cola` crate and `vlm_structure.py`. No further
> placement work is warranted; the lever is upstream. Full journey in memory `cola-engine-state.md`.
Captures a literature pass (see "Sources") so the build, when approved, targets the proven structural
solution rather than another round of SA term-tweaks (which are exhausted — see
`schematic-quality-and-the-critic-ceiling.md`).

## 1. The problem (recap)

The current engine places via a greedy/anneal search on a router-free `proxy_cost`
(HPWL + bbox-spread + per-satellite cohesion + straightness). Two structural limits:
- **Local optima** — the SA gets stuck; 8+ term-level levers all traded one defect for another.
- **The cost can't express "schematic structure"** — aligned rows, signal-flow order, tidy banks,
  rails-horizontal. The only thing that reliably produces structure today is *freezing* it via an idiom.

## 2. What the field actually does (literature synthesis)

### Track D — force-directed / constraint-based placement
- **Force-directed survey** (Cheong & Si): classical models = accumulated-force / energy-minimisation /
  combinatorial; hybrids = multilevel (coarsen→place→refine, the standard local-minima escape) + MDS.
  Schematic-specific variants add an **octilinear magnetic force** that pulls edges/parts onto axes.
- **Constraint-based layout = the mature answer** (Adaptagrams/**libcola**): minimise force-directed
  **stress via majorization** subject to **separation + alignment + orthogonal-edge constraints**, solved
  as a per-axis **QP**. This is force-directed's global optimisation *with* hard structure.
- **HOLA** (human-like orthogonal layout): decompose graph into **core + trees**; stress-minimise the
  core; attach trees "outside"; finish with **opportunistic alignment**. Beats prior orthogonal layout in
  user studies and approaches hand-drawn quality — the "human-like" comes from **motif-aware** handling
  (chains/trees), not a uniform force field.
- **ARCOL** (2026): stress-min core → orthogonalise (HOLA grid rules) → tree-reattach, with soft
  aspect-ratio normalisation. **Explicitly notes the gap for schematics**: needs *directional / layer
  ranking* (Sugiyama-style) that pure constraint layout lacks — **which we already have in the `crossmin`
  crate.**
- **CircuitLM** uses plain **force-directed placement** (springs + repulsion) + Manhattan routing for
  schematics — confirming force-directed is the algorithmic placement choice; but plain (unconstrained)
  → organic, not schematic-grade.

### Track E — LLM/VLM for schematic layout
- **EEschematic** (multimodal-LLM analog schematic agent) — the most directly relevant. The **LLM emits
  PLACEMENT** (JSON coords + orientation) after identifying substructures from 6 building-block exemplars;
  **wiring is deterministic** (a priority-ordered net algorithm); a **Visual-Chain-of-Thought** loop feeds
  the *rendered image* back to the LLM against good/bad references to refine. Results: 90% correct,
  aesthetics 8-9/10 on simple blocks but **degrades on complex** (telescopic cascode 5/10). Stated fix:
  **"constraint-guided refinement."** ⇒ This is precisely where Track D complements E.
- **Reason-SVG** — "**Drawing-with-Thought**" scaffold: Concept→Canvas→Decompose→Coordinate→Style→Assemble,
  with a hybrid reward (thought / render-validity / semantic / aesthetic). Planning-then-drawing measurably
  raises structural validity.
- **SVGen** — curriculum (simple→complex) + CoT + RL with an **Integrity reward** (parses/closes) and a
  **Path-matching reward** (right element count). Failure modes: missing paths, dimension drift.
- **CircuitLM** — multi-agent (NER → retrieval → electronics-expert CoT → CircuitJSON → SVG); LLM does the
  *logic*, layout is algorithmic.

### The objective — the most actionable single find
- **DiagramEval** — score a diagram by **graph metrics**: Node-F1 (do the labelled elements match) and
  **Path-F1** (do multi-hop connections match). Correlates with human judgement **far** better than
  CLIPScore (0.43/0.41 vs 0.11/0.08) and is **resistant to gaming** (you can't fake it by scattering
  elements without real connections). ⇒ An **objective, gaming-resistant layout metric** we can extract
  from our own rendered SVG and compare to the netlist graph — directly answers the CLAUDE.md "never game
  the critic" constraint *and* the long-standing "warnings/crossings don't capture quality" gap.

## 3. The design — a three-layer hybrid that unifies A/D/E

```
netlist + LayoutIr
   │
   ▼   ① STRUCTURE  (E + crossmin)
   │   VLM Drawing-with-Thought pass: identify substructures, functional groups,
   │   flow direction, alignment sets → emit a CONSTRAINT SET (not raw coords).
   │   crossmin supplies Sugiyama layer ranking for L→R flow. Idiom matcher
   │   contributes its motifs as constraints too. Everything validated vs the netlist + soft.
   ▼
   │   ② PLACEMENT  (D, the core — extend the `forceplace` crate)
   │   Constraint-based STRESS MAJORIZATION: minimise net stress (wirelength) subject to
   │   {alignment: rails horizontal, banks aligned; separation: flow order + non-overlap;
   │   orthogonal}. Global (escapes SA local optima) + structured (constraints). Then
   │   LEGALISE: snap to grid, orient symbols octilinearly, materialise rails.
   ▼
   │   ③ ROUTE + SCORE
   │   Existing elbow router (already strong; both EEschematic & CircuitLM kept wiring
   │   deterministic). Select/iterate using the DiagramEval-style graph-F1 objective
   │   (gaming-resistant) alongside the VLM critic.
   ▼  .kicad_sch
```

**Why this is the right unification:**
- **Idioms become constraints**, not frozen cells that `decongest` fights — they participate in one global
  solve (HOLA's motif-aware insight).
- **Force-directed gives the global move** the greedy SA can't (multilevel stress majorization).
- **The VLM supplies the structure** rules can't infer (signal intent, functional grouping) — but only as
  *soft, validated constraints*, never raw geometry, so it can't break connectivity (EEschematic's residual
  failure mode).
- **graph-F1 gives an objective** the current warnings/crossings metrics lack, and that can't be gamed.

**Why it should beat the prior art too:** EEschematic fails on complex aesthetics and itself prescribes
"constraint-guided refinement" — that's our layer ②. CircuitLM's plain force-directed lacks alignment/flow
— our constraints + crossmin layering add them.

## 4. Phased plan (build only after approval; each phase independently shippable)

- **Phase 0 — the objective, first (lowest risk, immediate value).** Implement the graph-F1 metric:
  extract the connection graph from our rendered output, compare to the netlist graph (Node-F1, Path-F1,
  plus crossing/alignment sub-scores). It needs **no placement change**, plugs into the existing additive
  A/B as a gaming-resistant selector, and is the **measurement foundation** for validating D and E. Likely
  a small win on its own (better A/B picks than warnings/crossings).
- **Phase 1 — constraint solver.** Extend `forceplace` (currently Fruchterman-Reingold springs) to
  **stress majorization + separation/alignment constraints** (port the libcola core). Acceptance: the
  multi-rail LDO chain places as one clean aligned row *without* a frozen idiom, beating the SA on graph-F1.
- **Phase 2 — motif→constraints.** Feed idiom-matcher motifs + `crossmin` layer ranks in as constraints.
  Run as an **additive A/B** (constraint placer vs SA, keep-better by graph-F1 + critic) on multi-sheet
  sub-sheets. Never regresses (min-of-two).
- **Phase 3 — VLM structure (E).** Drawing-with-Thought pass emitting a *validated, soft* constraint set
  (groups/flow/alignment). Cache by netlist hash; fall back to rule-based constraints if the VLM output
  fails validation.

## 5. Risks & mitigations
- **Solver effort** (stress majorization + constraint QP in Rust) — port the libcola algorithm core; keep it
  a separate crate so it unit-tests in isolation (like `forceplace`/`crossmin` already do).
- **Legalisation** (continuous → on-grid orthogonal) is the historically hard step — lean on HOLA's
  grid-alignment rules and the engine's existing snap/orient/decongest.
- **VLM variance/cost** — structure-only output (not geometry), schema-validated, soft, cached.
- **Regression** — everything runs as an **additive A/B** (keep-better), new lane opt-in/gated, so it can
  never ship worse than today's SA, exactly like the `crossmin` A/B.

## 6. Validation gates (unchanged triad + the new metric)
- `placement_snapshot` — references byte-identical (new lane gated behind an env, like MULTISHEET_REFINE).
- `floorplan_netlist` oracle — connectivity intact (`LAYOUT_SEARCH=anneal`).
- **graph-F1 + VLM critic A/B** vs the SA across the fleet — must win or tie, never regress.

## Sources
- [Force-directed algorithms for schematic drawings and placement: A survey (Cheong & Si)](https://arxiv.org/abs/2204.01006)
- [Adaptagrams / libcola — constraint-based layout](https://www.adaptagrams.org/)
- [IPSep-CoLa: Incremental Separation Constraint Layout](https://www.researchgate.net/publication/6715571_IPSep-CoLa_An_Incremental_Procedure_for_Separation_Constraint_Layout_of_Graphs)
- [HOLA: Human-like Orthogonal Network Layout (Kieffer et al.)](https://ialab.it.monash.edu/~dwyer/papers/hola2015.pdf) · [impl](https://github.com/skieffer/hola)
- [ARCOL: Aspect Ratio Constrained Orthogonal Layout](https://arxiv.org/abs/2603.29618)
- [EEschematic: Multimodal-LLM agent for analog schematic generation](https://arxiv.org/abs/2510.17002)
- [CircuitLM: Multi-agent LLM for schematics from NL](https://arxiv.org/html/2601.04505v1)
- [Reason-SVG: structured reasoning for SVG generation (RL)](https://arxiv.org/pdf/2505.24499)
- [SVGen: interpretable SVG generation with LLMs](https://arxiv.org/abs/2508.09168)
- [DiagramEval: evaluating LLM-generated diagrams via graphs](https://arxiv.org/pdf/2510.25761)
