# Fab-class levers for fine-pitch routing — what helps, what doesn't, what's latent

A verify-first sweep (Jun 19) over the dense boards, asking "can a finer standard-fab rule
route more of the fine-pitch pins the engine leaves honestly unrouted?" The results were
NON-OBVIOUS and are captured here so they aren't re-derived (or naively applied) later.

## Lever 1 — smaller via (0.6/0.3 default → 0.5/0.3): board-specific WIN, but exposes a latent gap

A smaller via takes less room, so it can drop between balls a 0.6 via can't. Isolated
single-variable test (only via changed):

| board          | default via 0.6 | via 0.5/0.3        |
|----------------|-----------------|--------------------|
| bga-decoupled  | unconn 9        | **unconn 0, clean** (now its committed config) |
| dual-bga       | unconn 38       | unconn 34, clean   |
| tqfp64-stress  | unconn 24       | unconn 20, clean   |
| bga64-stress   | unconn 7        | unconn 7, clean    |
| bga169-scale   | unconn 51       | unconn 33, **but copper_err** |
| mcu-bga-system | unconn 22       | unconn 14, **but copper_err** |

So 0.5/0.3 routes MORE on dense boards — but it is **not a safe blanket default change**: on
bga169/mcu-bga it produces a `clearance` DRC error (via-vs-via, e.g. "Via [VCC] vs Via [S4]"
at 0.15mm < 0.2). `bga-decoupled` is the clean win and now uses 0.5/0.3 in its config.

### The latent bug it exposes (the real engine lead)

The naive router marks routed copper into the grid as the net's copper, inflated by the
TRACE halo (`clearance + trace_half`). A VIA is wider than a trace by `via_radius -
trace_half`, and the grid does NOT distinguish via-cells from trace-cells. So when net B's
via clears foreign copper, a foreign *via* is treated as trace-sized — B's scan adds B's own
overhang but not the foreign via's overhang. Two foreign vias can therefore sit
`foreign_via_overhang` too close. At the default via 0.6 this never manifests across 57
boards; a smaller via + tighter routing surfaces it. NOTE: the in-house lint
(`ClearanceViaAny`) DOES catch the resulting violation, but `route_auto` ships it anyway —
`drop_unconnected_copper` drops unconnected/shorted copper, NOT clearance-violating copper.

**Deliberate fix (deferred, not a cron-tick change):** either (a) stamp each via into the
grid with the VIA halo (`via_radius + clearance`) rather than the trace halo so foreign vias
self-block correctly, or (b) extend the connectivity-honest drop to also drop
lint-flagged clearance-violating copper (turn a DRC violation into an honest unrouted net).
(a) is the principled fix — the router should never place two vias too close in the first
place. Both are general (help at any via size), but need careful re-gating on all 57 boards.

## Lever 2 — finer CLEARANCE (0.2 → 0.1mm): NOT a general win; often WORSE

Counter-intuitively, less clearance (more room) routed FEWER nets on most boards. Isolated
on dual-bga (clearance-only, via/trace unchanged): unconn 38 → **61** (worse). Finer
clearance → finer grid pitch (`(trace+clearance)/2`) → different grid alignment / greedy
corridor contention → the sequential tree router does worse. The A* itself is unbounded (no
node budget), so it is not premature giving-up; it is grid-alignment variance. Only the
genuinely sub-0.5mm parts (bga100-fine, fixed last round) need a finer clearance to fit a
trace between pads at all. **Match clearance to pitch; do not over-tighten.**

## Takeaway for the agent surface

For a dense BGA that leaves balls unrouted, the reliable lever is a **smaller standard via**
(0.5/0.3), not a finer clearance. Finer clearance is only for genuinely sub-0.5mm pitch.
