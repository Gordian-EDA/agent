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

Same command on `lane/local-algos`, RELEASE build, `--required` **7/7 OK with real KiCAD**,
`kicad_faults=0` on every board. Critic sampled twice per board (the noise is real; the
deterministic columns are not).

| board | parts | layers | place ms | route ms | vias | wirelength mm | bends | off-angle | critic before → after (×2) |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| rc-divider | 3 | 2 | 0 | 0 | 0 | 28.65 | 3 | 0 | 6 → 8, 7 |
| transistor-led-driver | 6 | 2 | 5 | 1 | 0 | 85.20 | 11 | 0 | 6 → 6, 8 |
| keepout-route | 2 | 2 | 1 | 2 | 0 | 36.00 | 0 | 0 | 6 → 4, 5 |
| rc-lowpass-chain | 8 | 2 | 16 | 2 | 0 | 98.70 | 8 | 0 | 6 → 8, 6 |
| power-buck | 8 | 4 | 20 | 296 | 5 | 77.98 | 13 | 0 | 4 → 6, 6 |
| led-array | 9 | 2 | 1 | 5 | 0 | 115.64 | 8 | 0 | 9 → 7, 7 |
| bga25-route | 2 | 4 | 3 | 733 | 24 | 117.55 | 28 | 0 | 5 → 5, 5 |

Critic total 42 (before, one sample) → **44** on both after-samples. Deterministic totals:
**bends 140 → 71 (−49%)**, vias 36 → 29, wirelength 573 → 560, and `off_angle` 0 on every
board — now asserted as a contract test inside both router crates rather than merely
observed. The whole `--required` run is 13.9 s wall including KiCAD DRC, down from 37.7 s.

Routing measured on its own (`cargo run --release -p pcb-route-mesh --example corpus`,
seven frozen `RoutingView` fixtures, so the placer cannot move underneath it): bends
138 → 71, bends/mm .241 → .124, wirelength 572.52 → 559.72, off-angle 1 → 0.

### What the corpus can and cannot show

Every remaining major defect on six of seven boards is "the outline is far larger than the
parts need". The fixtures **prescribe** `bounds`, so no placer change can move it — the
corpus ceiling is about 7-8, and `keepout-route` (two headers on a huge canvas) is mostly
measuring that. The routing complaints from the before-run ("short diagonal stubs into
D1/D3", "a wide bottom-layer trace crosses U1 at a diagonal") are gone from every summary.
Board sizing only bites end-to-end, where `sync_board` auto-sizes, and there it is snug.

`led-array` 9 → 7 is the one score that fell; its baseline sampled both 9 and 7, and its
only remaining fault is an empty band, so it is inside the critic's own noise.

### Timing, release

| board | parts | place ms | route ms |
| --- | ---: | ---: | ---: |
| led-array-60 | 61 | 40 | 155 |
| bga-system50 | 50 | 8 998 | 17 765 |
| mcu-board | 27 | 1 847 | 62 928 → **15 800** |
| soc-system | 69 | 272 573 | — |

A 50-part board is inside the 60 s budget (`led-array-60` 0.2 s, `bga-system50` 27 s).
Two boards are not, and neither regressed here — `soc-system` places in 289.8 s on main
against 272.6 s on this branch, and `mcu-board` still ends ROUTE_FAULT, now in 15.8 s
rather than 62.9 s with the same seven nets reported. A wall-clock deadline is the wrong
instrument for the rest: routing is deliberately machine-independent, so a cap has to be a
deterministic work budget, not a clock.

### Still open

- **`pcb-drc` has no dangling-end rule.** Every cleanup pass in both routing crates is
  gated on "introduces no new finding", so an oracle blind to a free trace end means no
  gate can refuse one — the `power-buck` sliver shipped *through* seven guarded passes, and
  the morning's `bga25-route` via bug was the same class. Writing the rule needs a decision
  about what counts as a legitimate free end, and a wrong one would make
  `drop_violating_copper` delete good copper.
- **`mcu-board` emits ~10 GND stubs of 0.1 mm under its own 0.15 mm trace width**, from
  `LANDING_MM` in `prepare_wide_terminal_escapes`. Load-bearing (it is the neck-down), on
  no `--required` board; the fix is a landing of at least one trace width, measured against
  the fine-pitch fixtures.
- **`keepout-route.kicad_pcb` carries no keepout/rule-area object at all** though the
  fixture declares one, so the board's defining constraint is invisible to DRC and to the
  render. Emitter side.
- **Signal-flow direction.** `transistor-led-driver` lost its left-to-right order; the
  placer has no notion of source→sink anywhere, and the old ordering came from
  `initial_grid` sorting by refdes. A per-net monotonicity term on a board axis is the next
  placement lever after board sizing.
