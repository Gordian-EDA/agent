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
score. It sends a render to the OpenAI-gateway vision model with a tightly-scoped
prompt and returns strict-JSON ranked defects + a 0-10 score; it exits nonzero on
any critical/major so it can gate a loop.

```
set -a; . ./.env; set +a   # OPENAI_API_KEY / OPENAI_BASE_URL
python3 tools/schematic_critic.py OURS.png [--reference REF.png] \
        --circuit "one-line description of the intended circuit"
```

Use it to drive iteration: render (ANNEAL=1 for the premium path) → critic →
fix the highest real defect → re-critic; aim for **consistently 9+** across varied
circuits. Generate fresh circuits with `cargo run --release -p agent --example
agent_design -- OUT.png "<prompt>"` (needs the OpenAI backend).

CAVEAT — VLMs (this critic AND Read-tool sub-agents) systematically **over-report
"wire through a component body"** on a correctly-drawn series/divider part (a
vertical resistor with wires above and below it is normal, NOT a crossing) and on
op-amp triangles / connectors. Always confirm against ENGINE GROUND TRUTH: the engine
counts the actual crossings (`count_body_crossings` + `count_collinear_body_crossings`
+ `count_ic_body_crossings`) and surfaces them on `EmitOutput.body_crossings` /
`.ic_crossings`, also returned as `wire_through_body` in the `apply_design` tool
result. When that count is 0, pass `--engine-clean` to the critic so it suppresses the
false positives (it also asserts the verified-complete netlist, killing dangling-pin
FPs on faint GND glyphs).

Gate every change on the netlist oracle
(`cargo test --release -p sch-layout --test floorplan_netlist`) — a prettier
render that breaks connectivity is a regression.
