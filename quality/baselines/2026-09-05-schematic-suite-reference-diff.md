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
