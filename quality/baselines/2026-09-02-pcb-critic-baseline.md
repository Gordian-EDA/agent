# PCB corpus — placer/router before and after the local-algorithms lane (2026-09-02, `lane/local-algos` off main @ de76fb57)

## Before

Produced by `cargo run -p pcb-workflow --example validate_pcb_corpus --quiet -- --required
--emit-dir <dir>`, then each emitted board rendered with `kicad-cli pcb export svg` +
ImageMagick at 900 dpi and scored once by `tools/pcb_critic.py --json-only` (`--drc-clean`
for every board except `bga25-route`). The critic samples the model once, so a single
score carries ±1 of noise; the deterministic columns do not. `bends`/`off_angle` are the
new `RouteMetrics` tidiness counters — corners in the emitted copper, and segments that
are neither axis-aligned nor 45°.

`bga25-route` **fails the gate on main**: KiCAD reports net `S7` as an F.Cu track and a
B.Cu track with no via stitching them, while our own lint reports zero findings. That is a
pre-existing truthfulness hole in the oracle, not a regression from this lane.

| board | parts | layers | place ms | route ms | vias | wirelength mm | bends | off-angle | kicad faults | status | critic |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- | ---: |
| rc-divider | 3 | 2 | 4 | 3 | 0 | 19.17 | 7 | 0 | 0 | OK | 6 |
| transistor-led-driver | 6 | 2 | 30 | 41 | 0 | 62.55 | 16 | 0 | 0 | OK | 6 |
| keepout-route | 2 | 2 | 5 | 17 | 0 | 36.00 | 0 | 0 | 0 | OK | 6 |
| rc-lowpass-chain | 8 | 2 | 91 | 84 | 0 | 86.95 | 19 | 1 | 0 | OK | 6 |
| power-buck | 8 | 4 | 114 | 10369 | 12 | 66.53 | 32 | 1 | 0 | OK | 4 |
| led-array | 9 | 2 | 10 | 77 | 0 | 111.01 | 17 | 0 | 0 | OK | 9 |
| bga25-route | 2 | 4 | 23 | 14642 | 24 | 127.81 | 49 | 0 | 3 | KICAD_DRC_FAULT | 5 |

Whole run: 37.7 s wall for seven boards including KiCAD DRC. **These timings are a DEBUG
build** and overstate the cost by 10-40x; the release numbers are in the after table, and
they are what the north-star budget should be read against.

## What the critic actually faults

Every board but `led-array` loses its points to the same three things, in order:

1. **Board utilisation** (major on six of seven). The corpus fixtures prescribe `bounds`,
   so the placer cannot resize the outline — but it *can* stop stranding the cluster in a
   corner. `power-buck` puts eight parts in the lower-right ~12% of its outline.
2. **Connectors off the edge** (major on `power-buck`, `bga25-route`, `rc-divider`).
   `power-buck` seats J1/J2 beside U1 (x=18.0/36.0) instead of on a board edge. My first
   hypothesis — that `unified_fanout_place`'s structured fast path was returning before the
   SA edge cost ran — was WRONG, and instrumenting `place_tuned` disproved it: that path
   fires on none of the seven boards. The real defect was that
   `edge_seek_position_candidates` offered the FLUSH seat only to an authored named edge,
   so a plain `edge_seek` connector reached the wall only by accident, via grid steps in
   `polish_positions`; and `seat_edge_seek_parts` treated the seat as a cost bet the
   wirelength of walking back out could win.
3. **Routing neatness**: short arbitrary-angle stubs entering pads (`led-array` D1/D3), and
   one long non-octilinear diagonal on `power-buck`/`rc-lowpass-chain` — the `off_angle`
   column. Same-kind passives also sit at mixed rotations (`rc-divider` R1 at 270° beside
   R2 at 0°; `power-buck` C1/C2 on different rows).

`led-array` scores 9 precisely because it has none of these: a centred two-row grid of
aligned same-kind parts, the header on the left edge, no vias.

## Timing headroom

`place_ms` is negligible everywhere (≤114 ms). Routing is the whole budget, and it is
spent inside `adaptive_grid_rescue`: `power-buck` 10.4 s and `bga25-route` 14.6 s, versus
≤84 ms for every board the first grid pass solves outright. The 60 s/50-part north-star
budget is intact, but the rescue path is where any future headroom has to come from.

## After

Same command on `lane/local-algos`, RELEASE build, KiCAD DRC included. Critic single sample
again, so read ±1.

| board | parts | layers | place ms | route ms | vias | wirelength mm | bends | off-angle | kicad faults | critic before → after |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| rc-divider | 3 | 2 | 1 | 1 | 0 | 24.83 | 3 | 0 | 0 | 6 → 7 |
| transistor-led-driver | 6 | 2 | 10 | 9 | 0 | 76.98 | 11 | 0 | 0 | 6 → 6 |
| keepout-route | 2 | 2 | 2 | 4 | 0 | 36.00 | 0 | 0 | 0 | 6 → 5 |
| rc-lowpass-chain | 8 | 2 | 25 | 5 | 0 | 92.92 | 11 | 0 | 0 | 6 → 8 |
| power-buck | 8 | 4 | 32 | 537 | 5 | 78.04 | 16 | 0 | 1 | 4 → 7 |
| led-array | 9 | 2 | 8 | 11 | 0 | 116.75 | 14 | 0 | 0 | 9 → 7 |
| bga25-route | 2 | 4 | 9 | 999 | 23 | 113.90 | 38 | 0 | 0 | 5 → 5 |

Critic total 42 → 45. Deterministic totals: bends 140 → 93 (−34%), vias 36 → 28, and
`off_angle` is 0 on every board and asserted as an invariant inside both router crates
rather than merely observed. The whole `--required` set runs in 18 s wall including KiCAD
DRC; without it, 1.6 s.

### What the corpus can and cannot show

Every remaining major defect on six of seven boards is "the outline is far larger than the
parts need". The fixtures **prescribe** `bounds`, so no placer change can move it — the
corpus ceiling is about 7-8 and the routing-specific complaints from the before-run
("short diagonal stubs into D1/D3", "a wide bottom-layer trace crosses U1 at a diagonal")
are gone from every summary. Board sizing only bites on the end-to-end path, where
`sync_board` auto-sizes, and there it is already snug.

`led-array` 9 → 7 is the one score that fell; its baseline sampled both 9 and 7, and its
only remaining fault is an empty band, so it is inside the critic's own noise.

### Timing, release

| board | parts | place ms | route ms |
| --- | ---: | ---: | ---: |
| led-array-60 | 61 | 40 | 155 |
| bga-system50 | 50 | 8 998 | 17 765 |
| mcu-board | 27 | 1 847 | 62 928 |
| soc-system | 69 | 272 573 | — |

A 50-part board is inside the 60 s budget (`led-array-60` 0.2 s, `bga-system50` 27 s).
Two boards are not, and neither regressed in this lane — `soc-system` places in 289.8 s on
main against 272.6 s here, and `mcu-board` spends 63 s in `adaptive_grid_rescue` and still
fails with 7 nets unrouted. Failing fast and honestly would be a better product than
failing slowly; a wall-clock deadline is the wrong instrument for it, because routing is
deliberately machine-independent, so the cap has to be a deterministic work budget.
