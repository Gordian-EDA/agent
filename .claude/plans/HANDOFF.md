# Handoff

## Goal
Schematic quality to 10/10 first; PCB paused. `~/sch-agent` is a reference to learn from, never a
dependency. Quality first, no time limits, no partial stops, no undo. KiCAD 10 CLI only.

## Yardstick
`python3 quality/run.py --suite schematic --jobs 2` — 14 cases; pass = ERC 0, anchored critic ≥ 9,
human-look ≥ 8. Baseline before the sprint: 1/14. Latest Blue Pill: critic 9 [7,9,9], human-look 6;
its remaining defect is content packed into a ribbon on an oversized page.

## Where things are
- `origin/main`: layout trees (`sch-flex`, old engines deleted), anchored `review_schematic` loop,
  titles/frames/notes, page fit, strict mode + netlist fidelity, run-to-completion loop.
- Unmerged, committed, clean:
  - `.claude/worktrees/shape` (`lane/route-shape-pagefill`): shape-based route acceptance + page
    fill — fixes the ribbon page. Finish, measure on prompt-blue-pill, merge.
  - `.claude/worktrees/tbox` (`lane/text-box-model`): one as-drawn text box model for lint and
    solver — fixes text over circuitry. Finish, measure, merge.
- Then iterate on the suite's top critic defect until the table is green.

## Rules
- Build targets under `.claude/targets/<lane>`; `/tmp` is a 22 GB tmpfs.
- Before deleting a branch: `git merge-base --is-ancestor <tip> main`.
- Memory: `/home/mimi/.claude/projects/-home-mimi-agent/memory/MEMORY.md`.
