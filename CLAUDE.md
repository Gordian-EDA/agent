# auto-pcb

A Rust workspace for an LLM agent that designs KiCAD schematics.

## Conventions

### When stuck: refresh, reframe, realign

When you've iterated a few times on a hard problem and the metric isn't moving (and
especially before you start *tuning constants* or reaching for ever-more-speculative
local fixes), **stop and think high-level about the problem's structure** — the
property you can exploit — rather than grinding the same approach:

- **Refresh** — re-derive the problem from first principles; what is *actually* being
  optimized, and what structure does the input have?
- **Reframe** — find the structural lever. E.g. schematic placement is a **mix of local
  and global** (clusters of tightly-coupled parts joined by a few global nets), so the
  search and the cost should be **locality-aware / two-level**, not a flat whole-board
  re-route per move. See `docs/specs/locality-aware-placement-search.md`. Other examples
  this session: idiom detection → a *graph-similarity* problem (own crate, declarative
  patterns); "wires too close to body" → the engine's *detector* had a blind spot, not
  the renderer; long ground rails → *distributed local power symbols* (a netlist property
  the agent controls), not a placement tweak.
- **Realign** — re-check the goal: are you optimizing the right thing? Is the metric a
  faithful proxy (the VLM critic over-reports *and* the engine under-reported
  wire-through-body — cross-check both)? Has the user reframed the goal?

Prefer a workflow/research pass (survey how the field solves the structural version of
the problem) over another round of speculative local edits. Capture the strategy you
land on as a spec under `docs/specs/`.

### Fuzzy / approximate string matching

Use `fuzzy-matcher`'s `SkimMatcherV2` (fzf-style subsequence scoring) for any
fuzzy search, ranking, or autocomplete over a candidate set. The reference
implementation is `SymbolIndex::search` in `crates/kicad-bridge/src/search.rs`.
Reuse it — do **not** hand-roll another fuzzy matcher or add a second
fuzzy-search dependency.

`strsim` (edit distance) is appropriate only for "did-you-mean" single-best
suggestions (e.g. `provider.rs`, `lint.rs`, `parse.rs`), not for ranking a list.

### Visual review: fresh sub-agent schematic critique

When the deliverable is a **rendered schematic** (or any visual artifact),
do **not** trust your own eyeballing to judge quality — you rationalize work you
just produced as "good enough" and gloss over real layout defects. Judge it with
fresh, unbiased sub-agents instead:

1. **Render.** `cargo run --release -p agent --example render_targets` writes
   `/tmp/renders/ours-*.png`. References live at `docs/validation/references/*.png`
   (`divider-filter`, `mcp1703-power-entry`, `555-blinker`, `uart-level-translator`;
   `logic-board-spaghetti` is a non-goal).
2. **Spawn one sub-agent per artifact, in parallel.** Give each a harsh
   adversarial-reviewer persona, the reference path **and** our render path, and a
   **structured-defect schema** (ranked list, not vibes). Sub-agents see images
   via the Read tool — it renders PNGs visually.
3. **Make them look for the defects the eye skips:** wires routed straight
   *through* a component body (including IC packages — the class most often
   missed), symbol/text overlap, off-spine legs, missing port labels, orientation
   violations (series part not horizontal / rail tap not vertical).
4. **Synthesize** the parallel reviews into ranked *engine-level* defects, fix the
   recurring high-impact ones, then **re-review**. Trust the review's defect
   **list** over your own eyeballing.

### Automated VLM critic: `tools/schematic_critic.py`

Prefer this over (or alongside) ad-hoc sub-agents for an OBJECTIVE, repeatable
score. It sends a render to the OpenAI-gateway vision model (default
`anthropic/claude-opus-4-8` — the strongest that works on the gateway) and returns a
RICHLY-STRUCTURED verdict: per-dimension scores (readability / routing / compactness /
convention), `strengths`, and ranked `defects` each with a `confidence` and a
`verification` trace. It exits nonzero only on a **high-confidence** major/critical, so
low-confidence (FP-prone) claims never gate a loop.

```
set -a; . ./.env; set +a   # OPENAI_API_KEY / OPENAI_BASE_URL
python3 tools/schematic_critic.py OURS.png [--reference REF.png] \
        --circuit "one-line description of the intended circuit" [--show-reasoning]
```

Use it to drive iteration: render (ANNEAL=1 for the premium path) → critic →
fix the highest real defect → re-critic; aim for **consistently 9+** across varied
circuits. Generate fresh circuits with `cargo run --release -p agent --example
agent_design -- OUT.png "<prompt>"` (needs the OpenAI backend).

FALSE POSITIVES — mostly handled now, but know the failure mode. The prompt forces the
model to REASON FIRST and TRACE every `wire-through-body` / `dangling-pin` candidate to
its wire endpoints before reporting (`--show-reasoning` prints the trace), which kills
the classic over-reports — a vertical series/divider resistor with wires above+below is
normal (NOT a crossing); a pin ending in a faint GND glyph is grounded (NOT dangling).
This brought the divider from an FP magnet to 9/10 with zero FPs. The `--engine-clean`
flag still hard-suppresses those two classes when the engine's geometry analysis
(`count_body_crossings` + `..collinear..` + `..parallel..` + `count_ic_body_crossings`,
surfaced as `EmitOutput.body_crossings` / `wire_through_body`) confirms 0 — but note the
engine itself under-reported until the **parallel-offset-through-plate** detector was
added, so a critic flag with `body_crossings=0` is worth a manual look, not an automatic
dismissal. (Native extended-thinking can't be enabled via the gateway, and
`--temperature` 400s on opus/gpt-5; Gemini 3 Pro / gpt-5.4 aren't usable here.)

Gate every change on the netlist oracle
(`cargo test --release -p sch-layout --test floorplan_netlist`) — a prettier
render that breaks connectivity is a regression.
