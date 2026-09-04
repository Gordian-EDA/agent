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


## Correction: the corpus figures above measured the wrong path

`examples/render_corpus` composed its layout IR from the caller's intent alone, which
carries rails and ports but no trees, so every fixture with an intent lost its authored
trees and drew as one bare row. Fixed in `15e644a1`; the numbers above are still valid
against each other, since both sides used that path, but they describe a sheet the
production path never draws.

On the path the sheet is actually drawn, same seven fixtures, three reads each:

| | |
| --- | --- |
| pooled over 21 reads | **7.48** |
| readability | 8.14 |
| routing neatness | 8.14 |
| convention | 8.00 |
| compactness | 6.71 |

`mcp1703-power-entry` scores 9, `555-blinker` 8, `divider-filter` 8.

Tried on the correct path and reverted: letting a band that already fits the page
re-compete on its proportions, to break up ribbon-shaped rows. It measured pooled 7.48 to
6.71 with compactness 6.71 to 5.29, every dimension down, and the unrestricted form was an
order of magnitude slower. The apparent win for this change, and the label overlap that had
blocked it, were both artifacts of the untreed path.


## Acceptance run, everything landed (`--suite schematic --repeat 2 --jobs 7`)

Each case run twice, the median reported, the spread shown after "of".

| case | parts | netlist | erc e/w | critic | human look |
| --- | --- | --- | --- | --- | --- |
| dataset-current-sense-2995e0dd | 33 | yes | 0/4 | **8** of 6,8 | 8 |
| dataset-ddr-memory-411be040 | 28 | yes | 10/32 | **8** of 7,8 | 8 |
| dataset-ecg-sensor-07aabb42 | 49 | yes | 0/0 | 6 of 6,5 | 4 |
| dataset-ecg-sensor-07aabb42 (after the `N$` fix) | 49 | yes | 0/0 | **9** of 9,5 | 6 |
| dataset-ibm-m122-261071e7 | 24 | yes | 1/68 | **8** of 7,8 | 7 |
| dataset-light-accessory-266db471 | 21 | yes | 0/0 | 6 of 6,5 | 4 |
| dataset-rp2040-mocon2040-0ea574f3 | 33 | yes | 0/0 | 7 of 7,5 | 7 |
| dataset-stm32-microcontroller-22e02ab9 | 31 | yes | 0/19 | 6 of 5,6 | 4 |
| dataset-three-phase-0cdac5a0 | 40 | yes | 0/0 | 7 of 6,7 | 7 |
| prompt-555-blinker-ldo | 15 | - | 0/0 | 7 of 7,6 | 6 |
| prompt-arduino-uno | 36 | - | 0/2 | 6 | 4 |
| prompt-bjt-preamp | 16 | - | 0/0 | 7 | 5 |
| prompt-blue-pill | 30 | - | 0/13 | 6 of 6,4 | 5 |
| prompt-hbridge | 18 | - | 0/0 | 7 of 6,7 | 6 |
| prompt-sallen-key-gain | 17 | - | 0/0 | 6 | 3 |

**CORRECTION.** The line that first stood here said three of fourteen passed. That counted
each case's BETTER attempt: `typical` took the upper of the two middle runs, which with
`--repeat 2` is simply the best one. On the conservative statistic — the lower of the two —
**none of the fourteen passes every check** and the mean critic is 5.79, against 5.93 for
the single-run suite this morning. Fixed so an even number of runs reports the lower middle.

What the two numbers together say is that the gap is CONSISTENCY, not ceiling. Every case
moved by one to three points between two identical runs: the ECG front end scored 9 and
passed everything on one attempt and 5 on the other; the Blue Pill 4 and 6. The engine can
draw a sheet that clears the bar and does not do it reliably.

The structural change is in what is left failing. **Every remaining failure is the visual
score.** Not one case fails on ERC, and not one fails netlist fidelity — all eight dataset
sheets reproduce their human original exactly, where three did not this morning. The
correctness work is done; what stands between here and a green table is the critic reaching
8 on eleven more sheets.

Movement on individual cases since this morning: three-phase 4 to 7, STM32 3 to 6, H-bridge
4 to 7, DDR 6 to 8, IBM 7 to 8, RP2040 6 to 7. Blue Pill went 8 to 6 and the 555 8 to 7,
which is within the two-point read noise and the reason each case is now run twice.


## The ribbon, and what it cost

Every one of the 28 agent sheets in the acceptance run came out three to six times wider
than tall, against the 1.4 of the page. The cause was `beside_the_fixed`: a design arrives
one block per `place_parts` call, and each block was grafted to the right of everything
already drawn. Five calls make a row.

The corpus never showed it, because its fixtures are single-block sheets drawn in one pass.
That is the whole reason a day of layout work aimed at compactness could not move it.

With beside and below scored on the proportions they leave behind, over six runs of three
multi-block cases:

| | before | after |
| --- | --- | --- |
| sheet aspect | 1.67, 5.66, 3.17, 1.21, 5.59, 3.93 | 1.39, 1.45, 1.70, 1.62, 1.44, 1.11 |
| critic, lower of two | ECG 5, Blue Pill 4, STM32 5 | ECG 7, Blue Pill 5, STM32 5 |

One ECG attempt scored **10** — judged better than the human sheet it was extracted from —
with the netlist exact, ERC clean and every check green. An STM32 attempt scored 8, also
fully green. The same ECG case scored 6 this morning with five wires and 124 labels.


## Full suite after the ribbon fix — and the ribbon fix does NOT show up in it

`--repeat 2`, lower median reported.

| case | attempts | reported |
| --- | --- | --- |
| dataset-ibm-m122-261071e7 | 8, 9 | **8 — passes** |
| dataset-ddr-memory-411be040 | 7, 7 | 7 |
| dataset-current-sense-2995e0dd | 6, 6 | 6 |
| dataset-stm32-microcontroller-22e02ab9 | 6, 6 | 6 |
| prompt-555-blinker-ldo | 6, 7 | 6 |
| prompt-bjt-preamp | 6, 6 | 6 |
| prompt-sallen-key-gain | 6, 6 | 6 |
| dataset-ecg-sensor-07aabb42 | 5, 9 | 5 |
| dataset-rp2040-mocon2040-0ea574f3 | 5, 6 | 5 |
| dataset-light-accessory-266db471 | 5, 5 | 5 |
| dataset-three-phase-0cdac5a0 | 5, 5 | 5 |
| prompt-arduino-uno | 5, 6 | 5 |
| prompt-hbridge | 5, 6 | 5 |
| prompt-blue-pill | 4, 6 | 4 |

**1 of 14 passes; mean 5.64.** Against the previous run's 0 of 14 and 5.79 — passes up by
one, mean slightly DOWN. So the ribbon fix, whose mechanical effect is not in doubt (sheet
aspect 3.5 → 1.45 across six runs, every sheet now near the page's proportions), **does not
show as a score gain over fourteen cases**.

Its apparent +2 on the ECG case in the three-case sample was itself variance: the same
binary gave that case 7 there and 5 here.

It is kept anyway, and the reason is not the score. A sheet 613 mm wide and 173 mm tall,
with five blocks strung in an unbounded row because each was grafted to the right of the
last, is wrong however it is graded. But nobody should cite it as a scoring win.

The spread remains the story: ECG 5 and 9, Blue Pill 4 and 6, on identical runs.


## Final run of the day: cross-section naming

`--repeat 2`, lower median, against the previous suite.

| case | before | now | attempts | |
| --- | --- | --- | --- | --- |
| dataset-ecg-sensor-07aabb42 | 5 | **8** | 8, 9 | PASSES |
| dataset-ddr-memory-411be040 | 7 | **8** | 8, 9 | PASSES |
| prompt-blue-pill | 4 | 6 | 6, 6 | |
| prompt-bjt-preamp | 6 | 7 | 7, 6 | |
| dataset-light-accessory-266db471 | 5 | 6 | 6, 6 | |
| dataset-stm32-microcontroller | 6 | 6 | 6, 6 | |
| prompt-555-blinker-ldo | 6 | 6 | 6, 7 | |
| dataset-rp2040 / three-phase / prompt-hbridge / arduino | 5 | 5 | | |
| dataset-current-sense-2995e0dd | 6 | 5 | 5, 6 | |
| prompt-sallen-key-gain | 6 | 5 | 5, 5 | |
| dataset-ibm-m122-261071e7 | 8 | 6 | 6, 8 | |

**2 of 14 pass, mean 5.93** against 1 of 14 and 5.64.

Read it carefully. Five cases up, three down, six flat; +0.29 on the mean. With per-case
run-to-run spread of one to two points, the standard error on that mean difference is about
0.35, so **+0.29 is inside one standard error and is not distinguishable from noise.**

The part that is more than noise: the two passing cases pass with BOTH attempts at 8 or
above (8,9 and 8,9), where passing has previously meant one lucky attempt out of two. The
ECG case in particular went from 5,9 to 8,9 — its floor up three points. That is the shape
of a consistency gain, and consistency is what the goal depends on.

Day's arc on this suite: 1/14 at the sprint baseline, 0/14 once the statistic was corrected
to the lower median, now 2/14.
