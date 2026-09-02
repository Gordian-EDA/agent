# PCB corpus — placer/router baseline before the local-algorithms lane (2026-09-02, main @ de76fb57)

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

Whole run: 37.7 s wall for seven boards including KiCAD DRC.

## What the critic actually faults

Every board but `led-array` loses its points to the same three things, in order:

1. **Board utilisation** (major on six of seven). The corpus fixtures prescribe `bounds`,
   so the placer cannot resize the outline — but it *can* stop stranding the cluster in a
   corner. `power-buck` puts eight parts in the lower-right ~12% of its outline.
2. **Connectors off the edge** (major on `power-buck`, `bga25-route`, `rc-divider`).
   `edge_seek` works on the boards that take the anneal path (`led-array` J1 at x=1.8,
   `transistor-led-driver`/`rc-lowpass-chain` J1 at y=4.3), and is silently dropped on the
   boards where `unified_fanout_place`'s structured fast path fires — that path returns
   before the SA edge cost is ever evaluated, so `power-buck` seats J1/J2 beside U1
   (x=18.0/36.0) instead of on a board edge.
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
