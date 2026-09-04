# Handoff (2026-09-04)

## The goal, in the user's words
Schematic to 10/10 first, as fast as possible; PCB paused. `~/sch-agent` is a REFERENCE TO LEARN
FROM — never a dependency, no bridge, no subprocess. Tools give the agent flexibility, not
constraints. Never stop on a partial state (no turn budgets; run to completion). No undo/revisions.
KiCAD 10 CLI only. Quality first; time later via parallel subagents.

## Yardstick
`python3 quality/run.py --suite schematic --jobs 2` — 14 cases (8 `dataset-*`: draw from a human
sheet's netlist, scored by netlist match + anchored critic vs the original; 6 `prompt-*`: Sallen-Key,
555+LDO, BJT preamp, H-bridge, Arduino-style, Blue Pill). Pass = ERC 0, anchored critic ≥ 9 (a human
sheet = 9), human-look ≥ 8. Baseline before the sprint: 1/14 (`quality/baselines/2026-09-03-schematic-suite-baseline.md`).
Single sample after the tree typesetter: Blue Pill critic 9 [7,9,9], human-look 6 — content packed
into a ribbon on an oversized page (the remaining top defect).

## What is on origin/main (all gated: build, tests, clippy -D warnings, arch check, corpus 7/7)
- `sch-flex`: the model writes flexbox-style row/col layout trees per block (payload `layout`, block
  `title`/`note`); the engine measures/aligns/routes. anneal/cluster/spine + relation intents deleted.
- `review_schematic`: anchored visual critic (same prompt files as `tools/schematic_critic.py`),
  prompt loop: review → revise trees → review, stop at 9 or two flat rounds.
- Title blocks, block frames, notes, page fit (A5→A2 ladder), rail length bound, orthogonal-wire
  invariant, drag primitive behind move_symbols/swap/connect, remove_region/delete_labels, repairable
  footprints everywhere, derived-net resolution, strict mode (no invented parts) + `netlist_fidelity`.
- Run-to-completion loop, harness with clean-render judges, findings + self-diagnosis per run.

## In flight (worktrees with committed work; their agents died with the session)
- `.claude/worktrees/shape` → `lane/route-shape-pagefill`: shape-based route acceptance
  `(len−manhattan)+6·bends+20·crossings ≤ 8.5|30.5` (else label pair), trunk lines, page fill /
  sheet balance (standard page aspect, blocks in rows filling ≥ 60 % of the page). Targets the
  ribbon-page defect. Finish, measure on prompt-blue-pill/hbridge/sallen-key, merge.
- `.claude/worktrees/tbox` → `lane/text-box-model`: one as-drawn text box model (`sch-model/src/text.rs`,
  validated against KiCAD's SVG: glyph advances, ink height = size, stroke 0.12·size, symbol field
  angle = (sym_rot+field_rot) mod 180) consumed by the lint, field/label boxes and the text solver;
  target collisions/part 0.35 → ~0.06 (human 0.16). Finish, measure, merge; snapshots will move —
  re-bless only with critic modal-of-3 ≥ before.
- Then: port sch-agent's text conventions we still lack (rotated-part text horizontal, ref/value by
  pin axis, connectors mirrored at row ends, labels on wired pins sit on the wire), and iterate on the
  suite's top critic defect until the table is green.

## Rules that cost us time when broken
- Build targets go under `/home/mimi/agent/.claude/targets/<lane>`; `/tmp` is a 22 GB tmpfs.
- Merge scripts must verify `git merge-base --is-ancestor <tip> main` before deleting a branch;
  never `pgrep -f`/`pkill -f` with a pattern that appears in the same command.
- Every campaign/suite refusal string is a tool defect until proven otherwise; harvest per run.
- Memory: `/home/mimi/.claude/projects/-home-mimi-agent/memory/MEMORY.md` (index) + files.
