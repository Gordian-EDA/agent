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

1. **Via span on the engine `Via`** — `from_layer`/`to_layer` (default through = F↔B). Today every
   via is through; this is the data model change.
2. **Export the bare-keyword form** in `synth.rs` — `micro` for an adjacent-layer span, `blind`
   for a non-adjacent inner span, nothing for through. Plus a `(uuid …)` per via.
3. **Use it in routing** — a fine-pitch inner ball that can't fit a through-via between neighbours
   drops to the nearest inner signal/plane layer via a microvia ON its pad (via-in-pad). This is
   the actual escape win; the stitch/route logic in `route_with_planes` chooses micro when the
   through-via `stitch_via_clears` fails for room.
4. **Microvia DRC sizing** — microvias have their own min size/drill; mirror kicad-cli's defaults
   in a create_board pre-check + the in-house lint (same pattern as the through-via minimums; note
   kicad-cli ignores the .kicad_pro rules block, so the in-house values are the gate).

The validation path (the scary unknown) is proven to work. The remaining work is a contained,
deliberate engine build — worth a focused effort, not a single tick.
