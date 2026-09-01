# Schematic live-edit rewrite — plan

Decision (2026-08-31): the `.kicad_sch` file is the only schematic source of truth.
The agent edits it in place through tools. The LLM owns connectivity + layout
*intent* (relative placement, groups, flow); solvers own all coordinates. Wires
are never drawn by coordinate.

## Target crates

| Crate | Role |
|---|---|
| `sch-doc` (new) | Lossless `.kicad_sch` document over `kiutils_kicad` CST: typed model for what tools touch, unmodeled nodes retained verbatim. Parse/edit/write, snapshot/undo, and the pure-Rust connectivity extractor (geometry → net partition). |
| `sch-check` (new, from `circuit-lang`) | `model.rs`/`lint.rs`/`erc.rs`/`diag.rs`/`provider.rs` rerooted to run over a `Design` built from the extractor. `circuit-graph` unchanged. |
| `sch-place` | `LayoutIr` gains relational intent: `left_of/right_of/above/below`, `group(members, side_of anchor)`, `align`. |
| `sch-floorplan` + engines | Honour relational constraints; new `region::arrange(items, frozen neighbours, obstacles)` adapter; realiser emits `sch-doc` items instead of a whole file. |
| `gordian-core` tools | Pin-level mutators, constraint placement, queries, `place_parts` bulk create, `arrange`, `rewire`, `connect`. Per-call net delta. |

## Deleted at the end
`circuit-lang` parse/desugar/canon/yaml/surface; `sch-io` (writer becomes realiser inside sch-floorplan); the draft workspace; `apply_design`/`edit_design`/`create_design`/`read_schematic(yaml)`/`validate_design`; `multisheet.rs`; the YAML apply-gate diff.

## Waves

**Wave 1 (parallel, worktrees):**
- L1 `sch-doc`: model/parse/write + round-trip gate on the 157-file corpus (kicad-cli netlist + ERC counts identical; hand content survives) + connectivity extractor gated against kicad-cli partitions.
- L2 engine constraints + region adapter on `sch-place::Item` (independent of sch-doc).
- L3 validate the PCB interactive tools: get `local-board-move`, `replace-pcb-component`, `finish-existing-pcb` quality cases to graded runs; fix what breaks.
- L4 `sch-check`: extract model/lint/erc from circuit-lang; define `place_parts` input type (kernel model JSON, keep `decouple` sugar).

**Wave 2:** tools + queries + snapshot/undo wired into `tool_defs`; prompts; quality edit cases pass with untouched parts byte-stable.

**Wave 3:** `place_parts`/`arrange`/`rewire`/`connect` over L1+L2; ten-prompt campaign parity; truthfulness gate green.

**Wave 4:** delete the old path; full quality suite + `validate_pcb_corpus --required`.

## Gates
- Extractor must equal kicad-cli netlist partition on every corpus file; kicad-cli stays the oracle in tests and at save.
- Any placement change passes the truthfulness gate before it is called a win.
