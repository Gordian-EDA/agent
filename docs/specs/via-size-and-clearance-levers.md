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

**FIXED (Jun 19) via the final fidelity pass — option (b), done safely.** The root cause was
structural: the stitch/fanout vias are added in `route_with_planes` (the AGENT layer) AFTER
the engine's `route_auto` reconcile, so they NEVER saw the DRC oracle — their clearance rested
entirely on hand-rolled `stitch_via_clears`/`fanout_seg_clears`, which slip at an unusual
via/clearance. The fix routes the COMPLETE copper (engine route + plane stitches) back through
`pcb_engine::lint::drop_violating_copper` at the end of `route_with_planes`; any net whose
copper still violates clearance/via/width/bounds is dropped and reported honestly unrouted.
Result: via0.5 boards copper_err 4/8 → 0; the 57 default boards are byte-identical (a clean
board lints to zero, so nothing is dropped). The via-size lever is now DRC-safe at ANY config.
The engine now validates ALL its copper — including agent-added stitches — through one oracle.

Option (a) (stamp vias with the via halo so the router never places two too close) remains a
possible *quality* refinement — it would avoid the drop and keep more copper routed — but is
no longer a *fidelity* requirement now that the final pass guarantees DRC-clean output.

## Via size was not propagated to signal vias (FIXED Jun 19) + the router-hole-aware gap it exposed

Adversarial stress (via 1.0/0.6 on a dense BGA) caught the engine shipping 68 `via_diameter`
faults: `to_route_problem` builds the RouteProblem from the PlaceProblem, which has no via field,
so `rp.via_diameter` defaulted to 0.6 — the router's SIGNAL vias ignored `rules.via_diameter`
entirely (only the stitch/fanout vias and the .kicad_pro min-via honoured it). So a 1.0mm rule
set min-via 0.95 while the router emitted 0.6 vias → mismatch, and the via-size lever was a no-op
for signal escape. FIX: propagate `rp.via_diameter`/`rp.via_drill` from rules in route_board, so
every via is one size. The in-house lint never caught it — it doesn't check via SIZE either.

**The deeper gap this exposed (DEFERRED, router change):** with the via size now real, a SMALL
via (0.5mm → annular 0.1) at a FINE clearance (0.13) exposed that the router's trace-near-via
routing is NOT hole-clearance-aware — a foreign track clears a via's COPPER by `clearance` but
its DRILL (annular inside the copper) only by `clearance + annular` = 0.23 < the 0.25 hole-to-
hole rule (lqfp144 regressed exactly here). The 0.6 default via (annular 0.15 → 0.28) masked it.
Fix direction: the grid's via keep-out / via_clear must inflate by `max(clearance, 0.25 −
annular)` so trace-to-via-DRILL clearance holds for any via size (same arithmetic as the fanout
`clr_via`). Until then, lqfp144 uses via 0.6 (it never actually used the 0.5 it declared — a
latent no-op), and bga-decoupled keeps via 0.5 (its config doesn't trip the trace-near-via case).
bga64-bigvia (via 1.0) added as a guard for the propagation fix. NOTE the lint is also blind to
`hole_clearance` (drill-to-drill) — closing that needs the deferred drill-aware obstacle model.

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
