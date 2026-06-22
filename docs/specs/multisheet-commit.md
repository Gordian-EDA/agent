# Multi-sheet commit (the deliverable that lets the agent ship dense designs at 8-9/sheet)

Status: **design, format reverse-engineered** (2026-06-22). Validated finding: a cramped single-sheet
dense board scores 4-7; split into per-block sheets each scores 8-9 (motordrv 5 → six sheets avg ~8.5,
three 9s). `crates/agent/examples/render_multisheet.rs` already does the per-block emit + block-split +
tiny-block merge and renders each as a clean PNG. The missing piece for PRODUCTION is a committable
KiCAD hierarchical schematic (root + sub-sheet files), so `apply_design` can ship dense designs this way.

## Where to wire it
`crates/agent/src/tools.rs:1154` `apply_design`: currently `emit_anneal(&design, &ir)` → ONE flattened
`.kicad_sch`. Add: if `design.blocks.len() >= 2` AND total parts > ~40 (dense), emit MULTI-SHEET instead.
Reuse render_multisheet's block-prep (extract to a shared fn): split over-crammed blocks to a uniform
target, merge tiny non-port-rich blocks. Then per group: `infer_ir(sub)` + `emit_anneal(sub)` → sub-sheet.

## KiCAD format (reverse-engineered from engine emit + a human multi-sheet board)
Connectivity is by **global_label** — the engine already emits cross-block (port) nets as `global_label`
(`emit.rs` `render_label`, `label.global`), and same-named global labels connect across sheets. So NO
hierarchical sheet pins are needed (flat-with-global-labels); the root `(sheet)` symbols are navigation only.

Engine single-sheet emit: header `(uuid "<SUB_ROOT>")`; every symbol instance is
`(instances (project "" (path "/<SUB_ROOT>" (reference "U1") (unit 1))))`; tail `(sheet_instances (path "/" (page "1")))`.

Per sub-sheet, to splice into a hierarchy under root uuid `MAIN_ROOT`, with the sheet's `(sheet)` symbol
uuid `SHEET_UUID` and page `N`:
- rewrite every `(path "/<SUB_ROOT>"` → `(path "/<MAIN_ROOT>/<SHEET_UUID>"` (symbol instances join the hierarchy);
- rewrite `(sheet_instances (path "/"` → `(path "/<SHEET_UUID>"` and set `(page "N")`;
- keep the file's own header `(uuid "<SUB_ROOT>")` (the sub-sheet file's identity).
Refdes are globally unique across blocks (the agent assigns them), so no annotation conflict.

Root `.kicad_sch`: standard header `(kicad_sch (version 20250114) (generator "eeschema") (uuid "<MAIN_ROOT>") (paper "A4") (title_block ...))`,
empty `(lib_symbols)`, then one `(sheet)` per block laid out in a grid:
```
(sheet (at X Y) (size 30 18) (fields_autoplaced yes)
  (stroke (width 0.1524) (type solid)) (fill (color 0 0 0 0.0000)) (uuid "<SHEET_UUID>")
  (property "Sheetname" "<name>" (at X Y-0.7 0) (effects (font (size 1.27 1.27) (bold yes)) (justify left bottom)))
  (property "Sheetfile" "<name>.kicad_sch" (at X Y+18.6 0) (effects (font (size 1.27 1.27)) (justify left top) (hide yes)))
  (instances (project "<proj>" (path "/<MAIN_ROOT>" (page "N")))))
```
then `(sheet_instances (path "/" (page "1")) (path "/<SHEET_UUID>" (page "N")) ...)` listing root + every sheet.

UUIDs must be deterministic (engine forbids random; keeps re-emits stable): derive each from a hash of the
project name + block name (UUIDv5-style or two FNV-1a hashes formatted 8-4-4-4-12). The engine's existing
uuids are already deterministic hashes (e.g. `78c1c215-99f1-55bb-...`, version nibble `5`).

## VALIDATE (do not ship unvalidated — KiCAD's path system is finicky)
1. `kicad-cli sch erc root.kicad_sch` — must run clean (every symbol annotated, paths resolve, global
   labels connect across sheets → no unconnected). This is the real gate; a wrong path → "symbol not
   annotated" / duplicate-instance errors.
2. `kicad-cli sch export svg root.kicad_sch` — renders all sheets.
3. Per-sheet VLM critic (already proven 8-9). Round-trip via `lift` should reconstruct the same netlist.

## Why this is THE deliverable
It's the only thing tested across 10 iterations that gets dense circuits to professional quality (8-9),
and it's how humans do it (53/500 of the dataset is multi-sheet). Single-sheet 9 remains a per-sheet
research problem (the ~8 ceiling hits even the tuned references), but multi-sheet makes it moot for the goal.
