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
