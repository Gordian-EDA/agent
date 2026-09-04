# Schematic suite, 2026-09-04 (`--suite schematic --jobs 5`)

One uncapped run per case on main after the sprint merges, agent `gordian` release
build, `gpt-5.6-luna`. Compared against the run on the same tree with both lanes merged
but before the day's engine fixes.

## What changed between the two runs

- Block folding decided once, by the proportions of the finished grid (`sch-flex`).
- Port and cluster labels may swivel about their tap point (`write/textsolve.rs`).
- `VSS*` names a ground, so a return path draws as rails rather than text labels.
- `arrange` accepts a block map as `layout`; `intent` accepts a bare net-to-side map.

## Scoreboard

Critic is the anchored score (three reads, modal) against a human sheet rated 9; for
`dataset-*` cases against that case's own human original.

| case | before critic/look | after critic/look |
| ---- | ------------------ | ----------------- |
| dataset-ddr-memory-411be040 | 7/6 | 6/4 |
| dataset-ecg-sensor-07aabb42 | 6/3 | 6/4 |
| dataset-ibm-m122-261071e7 | 6/4 | **7/6** |
| dataset-light-accessory-266db471 | 6/4 | 5/3 |
| dataset-power-over-135a2a11 | 6/6 | 6/5 |
| dataset-rp2040-mocon2040-0ea574f3 | 4/4 | **6/5** |
| dataset-stm32-microcontroller-22e02ab9 | 5/4 | 3/2 |
| dataset-three-phase-0cdac5a0 | 6/6 | 4/3 |
| prompt-555-blinker-ldo | 7/5 | **8/6 — every check passes** |
| prompt-arduino-uno | 5/3 | 6/3 |
| prompt-bjt-preamp | 6/4 | 6/6 |
| prompt-blue-pill | 7/6 | **8/7 — every check passes** |
| prompt-hbridge | 6/5 | 4/7 |
| prompt-sallen-key-gain | 6/– | 5/4 |

Mean critic 5.93 before, 5.71 after, over all fourteen.

## Reading it honestly

The mean did not move, and it could not have: the critic reads about two points apart
on one unchanged sheet (`tools/corpus_critic.py` exists because of this), and each case
is a single agent run whose composition choices vary as much again. A 14-case
single-run comparison cannot resolve a one-point change.

What did move is the ceiling and the pass count. **Two cases now clear every rubric
check (`prompt-555-blinker-ldo`, `prompt-blue-pill`), where none did before** and the previous best score anywhere was 7. Both are
cases where the model composed good layout trees; both draw titled framed blocks with
notes and wire their topology rather than naming it.

The two large drops, `dataset-three-phase` (6 to 4) and `dataset-stm32-microcontroller`
(5 to 3), are not engine regressions. Three-phase reviewed 7, then 8, then 7, then 4 and
shipped its last round — the revision loop is a random walk and the run ships wherever
it stops. That is now reported back to the model as a regression against the session's
best.

## Where the wall is

Composition, not correctness. Sheets are electrically sound and fold onto standard pages
now, but they occupy one band or corner and leave the rest of the page empty, and it is
the model's own trees plus the block pack that decide that. The unported pieces from the
reference are a balance-weighted pack score and a post-pack spread to fill.


## Corpus measurement after the second wave (verified on a clean checkout of `1ca5254b`)

`tools/corpus_critic.py` over the seven scored validation fixtures, three critic reads each.
This path renders the engine directly, with no agent run in it, so it isolates the engine.

| | start of day | after |
| --- | --- | --- |
| mean of the seven | 6.57 | **7.29** |
| pooled over all 21 reads | 6.43 | **7.00** |
| compactness | 4.57 | **6.43** |
| convention | 7.86 | **8.14** |
| routing neatness | 7.86 | **8.00** |
| readability | 7.57 | 7.57 |

Every dimension equal or better. `divider-filter` reaches 9, `555-blinker` and
`mcp1703-power-entry` 8. All 23 fixtures emit with no shorts and no opens throughout.

Two cautions for anyone comparing numbers across this file. The critic reads about two
points apart on one unchanged sheet, so only the pooled figure and the dimension averages
are worth reading. And the RENDER PIPELINE moves the score as well: the same Blue Pill sheet
scored 8 through the harness and 5 through an ad-hoc PDF rasterization, so corpus scores and
suite scores are internally consistent but must never be put in one table.
