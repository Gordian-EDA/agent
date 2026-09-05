# Schematic suite, 2026-09-05, main 5e70ebbc (`--suite schematic --repeat 2`, 7-sample critic)

The day's method changed after the user's challenge ("we are essentially copying a good
implementation"): put the reference's render beside ours on the same circuit, list the
concrete differences, fix the top one. Landed: connect names a net across sections
(7ea65a2d), the reference's block-composition paragraph in the prompt (32b41625),
headings at 2x body (a9e38314), no per-placement completeness nag (ef68d254), plain
labels instead of 128 pennants (lane/plain-labels), refusal of any placement that
overlaps a symbol and the `arrange` that claimed to move restored parts (lane/no-overlap),
junction dots from final geometry, 314 -> 185 with 0 at plain bends (lane/junctions),
frames around power flags alone dropped (94263959), and the cross-block net fix
(b88a3674) with its guard (5d2d9b05).

## Scoreboard (25 of 28 renders scored; 3 lost to the gateway)

| | previous main | this main |
| --- | --- | --- |
| cases passing every check on BOTH attempts | 0 / 14 | **3 / 14** (blue-pill, ibm-m122, ddr-memory) |
| passing one attempt | 4 | 1 (sallen-key) |
| mean critic, same 25 renders | 6.36 | 6.58 |
| mean human-look | 5.28 | 5.08 |
| Blue Pill | 6.86 / 6.43 | **8.43 / 7.86** |
| longest wire on any sheet | 302 mm | 56 mm |
| dataset cases matching reference netlist | all | all |

## Reading it honestly

Three passes is the most any run has produced, and the Blue Pill — the circuit the
side-by-side was done on — went from 6.9 to 8.4. But four sheets fell to 4-5, and the
deterministic counts say why: on 28 comparable renders, with block counts flat
(115 -> 113) and identical symbols per dataset case, LABELS rose 1456 -> 1871 (+28%) and
TEXT COLLISIONS 514 -> 901. The cross-block join fix names every piece of every
crossing net; that is correct (scattered nets 156 -> 0) but a human wires most of those,
and on the dense dataset sheets (three-phase#1 64 -> 173 labels on 40 parts) the labels
pile up. The plain-label glyph is NOT the cause: redrawing the regressing sheets with the
old pennants adds ~96 collisions, not removes them (measured by lane/plain-labels).

Two of my own claims were wrong and are corrected here: the composition prompt did not
fragment blocks (flat), and the Arduino/Blue Pill collision counts went UP (32 -> 97,
13 -> 71), not to zero as I first read them.

## Next lever, from the evidence

The reference's rule is DISTANCE, not block membership: its engine "wires connected pins
that are close, uses net labels for the rest" (`schagent/prompt.py:38`). Our `across`
rule forces a label pair across blocks even when the pins are adjacent, and the promotion
labels every piece. With `ap_nets` now recording the authored net on the earlier block's
pin, a later block can wire a close partner and label only what is far. That is one
change that cuts labels and matches the human sheets. lane/cross-block-nets has it.

## Second run, main 93c27034 (+ net-join minimal/stitch, thrash guard, cleanups, column tracks)

22 of 28 scored by the harness; the other 6 timed out in the critic (300 s cap not
scaled with 7 samples — fixed 5ffffe2a) and were rescored by hand with the same tool.

| | previous main (5e70ebbc) | this main |
| --- | --- | --- |
| pass both attempts | 3 / 14 | **0 / 14** |
| pass one attempt | 1 | 3 (555, current-sense 8.71, ibm 8.86) |
| mean critic, same renders | 6.41 | 6.52 |
| Blue Pill | 8.43 / 7.86 | 5.86 / 7.71 |

Blue Pill attempt 0 is attributable, not noise: the thrash guard fired 7 times and
held the LED-polarity loop, but the model — still told to clear a `fix: null`
finding — escalated around it: removed the LED twice, purged four GND symbols, five
power symbols, three capacitors and a flag (each a fresh ref set), and finished with
28 labels (was 96), a 159 mm wire and 5 wires through bodies. The cause is the
unrepairable blocking finding, not the guard; follow-up is with the loop lane.

The DDR and ECG drops (9.00→7.00, 7.57→5.86) are within one run's swing on those
cases; deterministic facts are checked below before blaming the typesetter.

## Third run, main 4520f35a (+ rail names never hidden, orientation by chain fan-out, honest frames, guard v2, LED-polarity fix)

| | second run | this run |
| --- | --- | --- |
| pass both attempts | 0 / 14 | 0 / 14 |
| pass one attempt | 3 | **8** (555 8.57, stm32 8.29, current-sense 8.00, ddr, ibm, bjt, hbridge, arduino) |
| mean critic, all 28 | 6.52 | 6.38 |
| mean human-look (excluding 2 grader parse failures) | 5.08 | 5.08 |

Deterministic state of main at this run, block-replay path: 51 frames for 51 blocks,
0 overlapping frame pairs, 0 rail glyphs without a name, text collisions 214, scattered
/ shorted / body overlaps 0 on all 24.

## The finding that explains the flat mean

Best-case is rising (eight single-attempt passes, the most ever); worst-case is not.
Between two attempts of one case the critic differs by up to 3 points, and that is the
agent, not the engine. Two facts from the logs:
- 13 of 27 renders never called `review_schematic` at all.
- Where it did, the agent's OWN critic grades with 3 samples and reports the modal —
  the statistic proved unable to resolve a sheet (786458a2) — and tells the model to
  stop when "the lowest sample reaches 9". It read 9.0 on three-phase#1 (harness 5.43)
  and on bjt-preamp#1 (harness 6.57), and stopped. The reference wins by iterating until
  its critic says 8; ours iterates on a critic that cannot see.
Fix in flight: review tool on the 7-sample mean, finish gated on a review
(lane/review-calibrated).
