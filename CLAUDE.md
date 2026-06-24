
A Rust workspace for an LLM agent that designs KiCAD schematics and PCBs.

## Conventions

### When stuck

When you've iterated a few times on a hard problem and the metric isn't moving (and
especially before you start _tuning constants_ or reaching for ever-more-speculative
local fixes), **stop and think high-level about the problem's structure** — the
property you can exploit — rather than grinding the same approach:

Prefer a workflow/research subagent (survey how the field solves the structural version of
the problem). 

### Fuzzy / approximate string matching

Use `fuzzy-matcher`'s `SkimMatcherV2` (fzf-style subsequence scoring) for any
fuzzy search, ranking, or autocomplete over a candidate set. 

`strsim` (edit distance) is appropriate only for "did-you-mean" single-best
suggestions (e.g. `provider.rs`, `lint.rs`, `parse.rs`), not for ranking a list.

### Feedback

Do not trust your own sense. Always use a fresh context subagent or special critic scripts to find issues.

`tools/schematic_critic.py` and `tools/pcb_critic.py` are given to you.

Ask the subagent to always look for spaces of improvements. When making major decisions, spawn multiple parallel advisor subagents to think about the pros and cons of the proposal and synthesize.

## Autonomous

Prefer fully autonomous rather than asking questions. Only ask questions if the instruction given is conflicting or unclear. If you do have questions to ask, ask your subagent.

Never stop until the ultimate goal or deadline is reached. Disregard context limits or token limits, focus on getting the job done. Do not pause between milestones.

You should regularly commit and push to origin. Resolve conflicts if any.

## Documentation and Code Quality

The only documentation needed is Rust docstrings. Do not write separate docs. Do not over-documentation or comments. Your documents and comments should be elegant, concise and easy for humans to read. 

Prefer self-explainatory code over comments and documentation.

## Zero Tolerence for Legacy / Drifted Code

While you are reading a source file, if anything not make sense to you, (e.g. an reference that does not exist in the repo), it is probably a drifted / legacy code. Stop your current work, ponder if it is still relavant, change the description or kill it.

Do not think about backwards compatibilty or blast radius when you are refactoring. Make all your work complete and final.

## Naming and Renaming

You should always rename stuff to reduce confusion. Do not be afraid of a huge blast radius. If a name does not serve as a good introduction, go give it a new name.

Good names: `kicad-ipc` `Point2`

## Code reuse and module structure

Use subcrates to separate maintain surfaces: most of the subcrates should be suitable for different maintainers to work parallelly.

Prefer breaking a large function into smaller maintainable pieces.
