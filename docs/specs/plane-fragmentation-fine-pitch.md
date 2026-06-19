# Plane fragmentation on a fine-pitch checkerboard BGA (found Jun 19, verify-first)

## The finding

A clean single-BGA probe — TFBGA-100 (0.8 mm pitch), perimeter balls = signals to spread-out
loads, **inner balls = an alternating GND/VCC checkerboard** (the common power-integrity pattern),
4-layer with GND/VCC planes — routes with `failed=0` from the in-house route oracle, **but KiCAD
DRC reports 36 unconnected**, every one of the form `Zone [GND] on In1.Cu <-> Zone [GND] on
In1.Cu`. The GND **plane is fragmented into disconnected islands**.

## Root cause — anti-pad overlap at fine pitch (a physical limit)

On a 4-layer board the GND plane (In1) must carve an **anti-pad** around every FOREIGN drilled
hole — i.e. around each VCC ball's stitch via. The anti-pad radius is
`via_diameter/2 + clearance + PLANE_ANTIPAD_MARGIN` ≈ `0.25 + 0.13 + 0.15 = 0.53 mm` for a 0.5 mm
via at 0.13 clearance. On a 0.8 mm-pitch **checkerboard**, adjacent VCC vias sit 0.8 mm apart, so
their anti-pads (radius 0.53) **overlap** — `0.8 − 2·0.53 = −0.26 mm` — forming a continuous wall
that pinches the GND plane into islands. This is the plane-side analog of the HDI inner-ball escape
limit: at this pitch the geometry simply does not leave a contiguous-plane channel between
alternating-net vias. It is NOT a routing slip — `prune_islands` correctly keeps every *anchored*
island; the islands are just mutually disconnected.

## It is NOT a fidelity violation (the engine stays honest)

Important: the contract ("0 copper ERRORS, honest about connectivity") HOLDS. The fragmentation
surfaces as KiCAD **unconnected** items (not a copper fault), and `export_board` reports
`drc.unconnected_items` to the agent with a "re-route or triage" note. So the engine does not ship
a connectivity LIE — the agent learns the truth at export. Confirmed narrow: the committed dense
boards (soc-system, bga121-planes-scale, bga256-201parts-scale, mcu-bga-system) have **zero**
zone-island disconnects — only this synthetic fine-pitch checkerboard triggers it.

## The real gap — the in-house ROUTE oracle under-reports

The one genuine inaccuracy: `route_board` (the in-house oracle) treats every plane-net pin as
"connected via the plane" and so reports `failed=0`, deferring plane CONTIGUITY entirely to the
export DRC. The agent therefore doesn't learn about the fragmentation until one step later (export)
instead of at route time. The route oracle should check that each plane net's anchored fill is a
SINGLE connected component and report the off-main-island pins as honestly unrouted.

## Fix options (deliberate, deferred — no committed board manifests it)

1. **Route-oracle plane-contiguity check (honesty, bounded).** Compute the plane fill at route time
   (reuse `plane_fill_rects` + a `prune_islands` variant that returns the anchored-component
   count), and when a plane is multi-component, report the smaller islands' pins in `failed`. Makes
   the route oracle as honest as the export DRC. Cross-cutting (the fill currently lives at export),
   so threading it into the route result is the bulk of the work.
2. **Physical fix (HDI / layer-aware).** Avoid the fragmentation: dedicate a full plane layer per
   power net (so a plane only carves anti-pads for ONE foreign net, halving anti-pad density), or
   use blind microvias so a ball's via doesn't pierce the far plane. This is the HDI frontier.

Guard: `bga-checkerboard-plane-frag.json` keeps this case in the harness — it must stay
`copper_err=0` (the fragmentation is honest-unconnected, never a copper fault).
