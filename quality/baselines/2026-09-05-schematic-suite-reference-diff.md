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

## Fourth run, main c0775116 (+ calibrated review: 7-sample mean, done at 8, review-before-finish gate; rail-connect fix; frame polish)

| | third run | this run |
| --- | --- | --- |
| pass both attempts | 0 / 14 | 0 / 14 |
| pass one attempt | 8 | 4 |
| mean critic, same 27 renders | 6.38 | **6.73** |
| mean human-look | 4.70 | **5.56** |
| total requests | 1173 | **2177** |
| in-run reviews | 33 | **124** (868 vision calls) |

Best numbers on both graders so far, and the loop now demonstrably works when it
stops: current-sense#0 reviewed 4.00, arranged once, reviewed 8.71, finished at 9.29.
But the stop rule ("stop when the mean fails to improve twice") lives only in the
prompt and was not obeyed:
  current-sense#1  4.57 6.00 6.29 6.00 6.71 6.00 6.86 7.00 7.14 | 5.14 4.86 4.00 5.86 6.71 6.43 5.14 -> 5.86
  stm32#0          3.57 3.86 6.29 6.00 6.00 6.14 6.00 5.86 6.29 6.14 6.00 4.00 -> 6.00
  rp2040#0         29 reviews, 430 requests -> 5.71
A sheet that reaches 7.1 and is then arranged again loses two points and never gets
them back, because a live sheet has no "best so far" to return to (undo was removed by
user direction). Fix in flight, in the loop not the prompt: second non-improving
review ends layout editing for the turn; a review with no layout change since the
last is refused (lane/review-stop). No further suite until it lands — each review is
seven vision calls.

## Fifth run, main ad8f185a (+ review stop enforced at the second flat review; review grades the same two images as the harness; finish gate on changed-since-review)

| | fourth run | this run |
| --- | --- | --- |
| pass both attempts | 0 / 14 | 0 / 14 |
| pass one attempt | 4 | 2 |
| mean critic, same 27 renders | 6.73 | **6.24** |
| mean human-look | 5.56 | **4.93** |
| total requests | 2177 | 1910 |

Requests and reviews fell as designed; the means fell with them. The trajectories say
why, and it is not the stop rule — it is what the rule reveals. Self-review means in
order, then the harness final:
  light-accessory#0   6.86 3.29 3.43 -> 5.00
  three-phase#1       7.29 6.00 3.00 -> 6.10
  bjt-preamp#1        8.43 7.86 6.57 -> 6.70
  hbridge#0           6.43 4.00 5.43 -> 6.30
  current-sense#0     4.50 5.57 5.57 6.00 6.00 5.71 -> 5.57   (previous run: 4.00 -> 8.71 -> 9.29)
On most runs the FIRST review is the best score the sheet ever has, and each arrange
after it makes the sheet worse. The operator "review, then re-arrange the named block"
degrades sheets more often than it improves them. The fourth run scored higher only
because unbounded iteration sometimes wandered back up.

Two responses. In flight: close the loop at the FIRST non-improving review, and tell the
model a worse review means the last edit hurt. Not built, by direction: keeping the
best-reviewed sheet and restoring it at finish — that is the checkpoint mechanism the
user removed on 2026-09-02, and it is the only thing that would let a bad edit cost
nothing. That decision is the user's.

## Sixth run, main a4cff34d (+ band review rule, promoted labels read outward, readable net names, text seated on drawn ink, multi-unit hardening)

| | fifth run | fourth run (best before) | this run |
| --- | --- | --- | --- |
| pass both attempts | 0 / 14 | 0 / 14 | **1 / 14** (555) |
| pass one attempt | 2 | 4 | 4 |
| renders scoring >= 8 | 3 | 8 | **10** |
| mean critic, all 28 | 6.24 | 6.73 | **6.96** |
| mean human-look | 4.93 | 5.56 | **5.86** |
| total requests | 1910 | 2177 | **1505** |

First run over 7 on the same-case comparison (6.70 -> 6.96 against the fourth run), on a
third fewer requests. The review rule that did it, measured on the fifth run's
trajectories before it shipped: when the first review reads under 5.5, 10 of 10 runs
improve (+2.07 mean); at 5.5 or above, 0 of 15 improve. So above the band the first
review is the finish; below it the loop iterates with a three-flat cap. Fifteen runs
now stop on their first review.

The blocker has moved: three renders at 8.14-8.71 (rp2040#0, ecg#0, blue-pill#1) fail
ONLY on ERC — `pin_to_pin` power-output conflicts (the finding demoted to stop the
thrash), `label_dangling`, and bare rail pins. Deterministic block-replay state: text
collisions 214 -> 7 today, frames 51 with 0 overlaps, 0 unnamed rails, 22 machine-shaped
labels (from 71), scattered/shorted/body-overlaps 0 on all 24.

## Suite 9 — main 226d32d3 (lane/erc-blockers merged), 28 renders, 7-sample mean

critic 6.96 → 6.96 (flat), human-look 5.86 → 6.07, requests 1505 → 1009.
Pass both attempts: prompt-bjt-preamp. Pass one attempt: 555-blinker-ldo, rp2040,
stm32-microcontroller, ibm-m122, ecg-sensor. Renders passing all checks: 7 of 28;
renders ≥ 8: 5. No ERC-only failures remain except one new one:

- **ddr-memory#1: 42 ERC errors (reference 13), critic 6.** The given netlist has 32
  single-pin nets (`DDR_DQ0_A` …), the DRAM's bus to another sheet of the source
  project. `check_schematic` reported each as a blocking `single-pin-net` error with
  `fix: null`; the model no-connected all 32 (stripping the labels), re-named them one
  `connect` call at a time (33 calls), was told the same thing again, and deleted the
  labels. Attempt 0 never touched them and passed ERC with 10. Fix: the lint is a
  warning phrased as a port; nothing in the drawing can "repair" a port but stripping it.

Side-by-side (fresh reviewer, human original vs ours) on the two cases lowest on both
attempts, current-sense 5.43/5.43 and sallen-key 6.0/6.0:
- Every `place_parts` after the first is a graft beside what is drawn and nothing is
  ever re-seated: four good blocks strewn across the top 1/12 of an A2 page, density
  0.87 parts/1000 mm² vs the human's 3.5 on A4, captions 300 px from their block,
  `arrange` on a seated block a no-op. → lane/reseat.
- `place_parts` marked a lone pin on an author-named net no-connect (route.rs
  `terms.len() < 2`), so port nets came out bare and the model spent 55 requests
  (current-sense#0) and 27 (ddr#1) re-naming them. Fixed 86b8aeea: labelled on a stub.
  Replay labels 837→853, text collisions 7→2; reviewer KEEP on three fixtures.
- Machine names printed as labels: `N$3`/`N$7` (the dataset's unnamed nets, which
  `machine_parts` does not recognise) and `N_U11_8` (INA240's pin 8 is named `+`, so
  no readable name exists; the human wires it). Open.

## Suite 10 — main a03c4e5f (single-pin-net demoted, router obstacles), 14 renders (one attempt), 7-sample mean

critic 7.18 (v9 per-case worst 6.55 / best 7.38), look 6.00, requests 596 for 14 (v9: 1009 for 28).
Passing all checks: current-sense, ecg, ibm, rp2040, hbridge (5 of 14).
- current-sense 5.43 → 8.86 in 19 requests (v9: 55). The `single-pin-net` error was the
  whole thrash: with it a warning the model placed, checked, reviewed and finished.
- ddr 42 → 0 ERC errors; critic 6 (dense BGA sheet, no thrash left, 22 requests).
- Low and consistent: three-phase 5.29 (4 ERC), light-accessory 5.86 (77 requests),
  sallen-key 5.71, ddr 6.0 — the next side-by-side targets.
- This binary predates the port-label rule (86b8aeea) and the wire-more merge.

## Evening: what the render showed, and what changed (main 6e280724)

Side-by-side of our deterministic Blue Pill replay against `~/sch-agent`'s render named
the visible gap: every decoupling cap carried its own glyph pair (68 power symbols vs
27), because `emit_rail` gave up any trunk wider than 50 mm. Landed, all replay 0/0/0,
netlist oracle green, reviewer KEEP:
- bank rails (4f46c3b3): a net's pins split into one-row banks; each bank one rail, one
  glyph, judged by the air between risers. Blue Pill 68 → 32 glyphs; mcp1703 10 → 5.
- one glyph per point (f17a5d98): 81 coincident glyph pairs across the corpus → 0.
- numbered nets join across calls (b8d6c066): `N$n` straddling two `place_parts` calls
  was silently dropped; 8 of 14 cases are dataset netlists named this way.
- merged: serve-pin (support parts beside the pin they serve, shunts 41 → 54 % wired),
  reseat (whole-sheet re-pack after place_parts; esp32 fill 71 → 87 %).
- REVERTED (6e280724): re-seat after `arrange` — the live Blue Pill came out torn across
  an A1 page (8 place_parts, 5 remove_symbols; pieces moved apart from their frames).

Live agent runs on the merged engine (one attempt): dataset-stm32 7.86 PASS (0 ERC, 55
requests, A3, framed); prompt-blue-pill 6.57 (101 requests, A2, blocks in the corners —
the model's arrange/remove churn is what the engine cannot pack).
