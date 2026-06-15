# auto-pcb

A Rust workspace for an LLM agent that designs KiCAD schematics.

## Conventions

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

Gate every change on the netlist oracle
(`cargo test --release -p sch-layout --test floorplan_netlist`) — a prettier
render that breaks connectivity is a regression.
