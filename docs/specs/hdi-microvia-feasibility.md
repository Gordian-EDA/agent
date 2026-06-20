# HDI / blind-microvia feasibility — de-risk spike (Jun 19)

The recurring "honestly unrouted" nets on dense fine-pitch BGAs (the inner balls a 0.5mm
through-via can't drop between, and the plane-pierce fragmentation) all point at one frontier:
**HDI — blind/buried vias and microvias**. Before committing to that (large) build, this spike
answered the critical unknown: *does the engine's DRC gate even work for HDI boards?*

## Result: HDI is VIABLE through the existing kicad-cli DRC gate

`kicad-cli pcb drc --exit-code-violations --format json -o report.json board.kicad_pcb`
(the exact command the engine's `KicadCli::drc` runs) **accepts a blind/micro via and writes the
JSON report** — verified on KiCAD 9.0.2, injecting a via into a real 4-layer board:

| via | result |
|-----|--------|
| through (control) | rc=5, JSON written ✓ |
| `(via micro (at…) (size 0.4)(drill 0.2)(layers "F.Cu" "In1.Cu")…)` | rc=5, JSON written ✓ |
| `(via blind …)` | rc=5, JSON written ✓ |

(rc=5 is just "violations found" — the injected via was unconnected; the engine's drc already
keys success on the report parsing, not the exit code.)

## The syntax gotcha that cost half the spike (write it down)

The via TYPE is a **bare keyword immediately after `via`**, NOT a `(type …)` sub-node:

- ✅ `(via micro (at X Y) (size S) (drill D) (layers "F.Cu" "In1.Cu") (net N) (uuid …))`
- ❌ `(via (type micro) (at …) …)` → kicad-cli still *parses + DRCs to stdout* (rc=0), but the
  `--format json -o` report writer then fails (rc=3, **no file**), which would silently break the
  engine's DRC gate. So an HDI export MUST emit the bare-keyword form, and the board harness would
  have caught a wrong encoding as a "DRC could not run" error, not a fault.

A `(uuid …)` is also required or kicad-cli rejects the via outright (rc=3).

## What the HDI build still needs (now de-risked, deliberate, multi-turn)

1. ~~**Via span on the engine `Via`** — done (commit 11e19b3): `ViaSpan` enum {Through, Partial{from,to,micro}}, `#[serde(default)]`, all 8 construction sites default to Through.~~
2. ~~**Export the bare-keyword form** — done (commit 11e19b3): `render_via` emits `(via micro|blind …)` for a Partial span (byte-identical for Through); 3 unit tests guard it.~~
3. ~~**Use it in routing** — done (commit 7a889eb): `route_with_planes` places a 0.4/0.2 micro
   via-in-pad after the through-via in-place+fanout fail, for a pad whose plane is the ADJACENT
   layer (F→In1). soc-system: 7 inner GND balls escape, 0 faults.~~ KEY LIMIT FOUND: KiCAD holds
   blind/buried vias to the full netclass via min (only MICRO gets the relaxed floor), so a blind
   via is never smaller than the through that already failed — deeper planes (In2…) need STACKED
   microvias with isolated landing pads (future increment), not a single blind via.
4. ~~**Microvia DRC sizing** — done (commit 7a889eb): DrcViolation::ViaDiameterBelowMin; lint
   checks each via vs netclass via_diameter (through/blind) or the 0.3 micro floor (micro). This
   closed a real gap that shipped 56 via_diameter faults from an undersized blind via.~~

The validation path (the scary unknown) is proven to work. The remaining work is a contained,
deliberate engine build — worth a focused effort, not a single tick.

## Stacked-microvia spike + the signal-congestion finding (Jun 19, increment-5 research)

Investigated the next lever — reaching the DEEPER plane (In2 on a 4-layer; the planes on 6-layer+).

**Stacked microvias are kicad-cli-DRC-valid.** A spike injecting two micro vias at one point
(`F→In1` + `In1→In2`) produced NO stacked/microvia/hole-co-located violation — kicad-cli does not
require staggering. So the deeper plane is reachable in principle by a stack of adjacent micro vias.

**But the in-house lint kills it (verify-by-implementing).** Implementing the stack made soc-system
catastrophically WORSE (unconnected 74→277, VCC 420): the stack's CO-LOCATED holes trip the in-house
lint's hole-clearance check (which is span-blind — every via is modelled as a full through-hole), and
`drop_violating_copper` then drops the WHOLE net. So a stacked-microvia escape needs a **span-aware
hole exemption first**: two SAME-NET micro vias at the same point with adjacent, chained spans are a
legal stack, not a hole violation. Reverted the stack; kept the single-micro (adjacent-plane) escape.

**The bigger finding — the remaining unconnected is SIGNAL congestion, not plane-ball HDI.** On
4-layer soc-system the 74 residual unconnected are only 14 plane balls (stack-able) + ~60 SIGNALS
(A0–A11…). Signals don't go to a plane, so HDI can't help them — they're unrouted because
`route_with_planes` hardcodes `rp.layer_count = 2` (signals on F/B only). At 6-layer that WASTES the
two inner SIGNAL layers (In1/In4; only In2/In3 are planes), so 6-layer soc-system is WORSE (130 vs
74), not better. The router already avoids plane layers via `plane_mask` (astar.rs:367), so the fix
is to route signals on ALL non-plane layers instead of forcing 2 — but the stitch/retag logic
(`ob.layers = [top, bottom]`) assumes exactly two signal faces, so this is a substantial, careful
build, not a one-liner.

### Next-lever priority (both deliberate builds, scoped here)
1. **Inner-signal-layer routing at 6-layer+** (the big one): let a dense board actually use its inner
   signal layers so 6-layer beats 4-layer. Restructure `route_with_planes` to route on
   `layer_count − |planes|` signal layers (rely on `plane_mask`), retag plane pads onto all signal
   faces, and keep the micro escape (single for In1; stacked for deeper, after #2).
2. **Span-aware via hole exemption** in the lint, then re-enable stacked micro vias for deeper planes
   (rescues the ~14 same-as-deeper-plane balls on 4-layer + every deeper-plane ball at 6-layer+).
