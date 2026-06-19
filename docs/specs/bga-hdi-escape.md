# Dense-BGA (≤0.8mm) inner-ball escape — the HDI frontier

## Status: documented frontier, NOT a quick fix (microvia hypothesis refuted)

The engine routes BGAs DRC-clean and honestly leaves un-escapable inner balls unrouted.
For pitch **≥ 1.0mm** a through-via seats in the diagonal channel between balls, so the
escape works (see `bga-escape-routing.md`, the `via_clear_radius` fix). For pitch
**≤ 0.8mm** the inner balls can't escape with standard fab, and this is the last real
routing frontier. This spec records what was tried and what it would actually take.

## Refuted: "just use a smaller via" (tested, Jun 19)

Hypothesis: the ≤0.8mm escape is blocked only by the through-via footprint, so a microvia
(0.3mm/0.15mm) would let the inner balls escape. Tested on `bga64-stress` (BGA-64, 0.8mm)
by temporarily lowering the fab-class floor and routing with a 0.3/0.15 via:

- **It made routing WORSE**, not better: failed nets 7 → 15.
- **158 KiCAD DRC errors**: 78 `annular_width` (0.3−0.15 gives 0.075mm ring < 0.1mm min),
  40 `hole_clearance` (0.15mm via barrels at 0.8mm pitch collide hole-to-hole), 40
  `clearance`.

So a smaller **through**-via is not the lever — its barrel still collides with neighbours
at 0.8mm, and its annular ring is sub-fab. The escape is geometry-limited, not via-size.

## What real HDI escape requires (a major feature, scoped here)

1. **Blind/buried laser microvias** — a via spanning only F→In1 (not the whole stack), so
   the inner ball drops one layer and escapes there without a barrel through every layer.
   Needs: a via *type/span* model in `RouteSolution`/synth (KiCAD `(via blind)` / `(via
   micro)` with a layer pair), the router emitting layer-pair vias, and the .kicad_pro
   declaring HDI rules (microvia dia/drill, lower annular).
2. **Via-in-pad** — the microvia sits ON the ball pad (filled/capped), not in the channel.
3. **Even then it's tight**: at 0.8mm pitch the hole-to-hole budget is small; laser drills
   (~0.1mm) are needed and inner rows beyond the 2nd typically still can't all escape on
   one inner layer (needs several HDI build-up layers — 6+ layer stackup with sequential
   lamination).

This is a multi-subsystem feature (problem model + router + synth + DRC rules + fab class),
not a tuning change. It is the right next big lever IF dense-BGA full-escape is a goal;
until then the engine's honest "inner balls unrouted" is the correct behaviour.

## Recommendation for the agent (today)

- Prefer **≥ 1.0mm-pitch** BGAs when full escape is required (through-via fab, works now).
- For ≤ 0.8mm parts, the perimeter + first inner ring escape; deeper balls are honestly
  unrouted (not a fault). Don't lower the via below the fab floor — it trades a few escapes
  for annular/hole-clearance DRC errors (net negative, as measured above).
