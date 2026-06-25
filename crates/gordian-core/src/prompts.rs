//! The KiCAD agent's system prompt: the circuit-YAML language spec (kernel +
//! sugar), the workflow doctrine, and the PCB layout/routing doctrine.
//!
//! Kept as a single embedded string (no design state) — the model pulls the
//! design on demand via `get_design`.
//!
//! For a NEW design the prompt can be RETRIEVAL-AUGMENTED: [`system_prompt_with_reference`]
//! appends the single best-matching real human design (lifted to circuit-YAML) as a
//! worked few-shot example, grounding the agent in professional patterns. The example sits
//! in the cached system-prompt prefix, so it costs once per session, and is size-capped so
//! it never bloats the prompt.

use crate::retrieval::Corpus;
use kicad_env::KicadEnv;

/// Cap on the few-shot reference YAML embedded in the system prompt (chars). A
/// human schematic can lift to a multi-KB document; beyond this it stops being a
/// crisp example and just inflates every cached prefix, so it is truncated.
const REFERENCE_YAML_CAP: usize = 6000;

/// The domain system prompt handed to the agent loop.
pub fn system_prompt() -> String {
    SYSTEM_PROMPT.to_string()
}

/// The system prompt with a single best-matching real design appended as a
/// worked few-shot example, when a reference corpus is installed AND a plausible
/// match lifts to circuit-YAML. Falls back to the plain [`system_prompt`] when
/// there is no corpus, no match, or the match won't lift — so this is always safe
/// to call at design-start.
///
/// Bounded by design: exactly ONE example, capped at [`REFERENCE_YAML_CAP`] chars,
/// so the retrieval grounding rides in the cached prefix without bloating calls.
pub fn system_prompt_with_reference(env: &KicadEnv, intent: &str) -> String {
    let base = system_prompt();
    let corpus = Corpus::discover();
    if corpus.is_empty() {
        return base;
    }
    // One lifted reference is enough; ask for k=1 successful lift.
    let report = corpus.find_similar(env, intent, 1);
    let Some(reference) = report.lifted().next() else {
        return base;
    };
    let yaml = reference.yaml.as_deref().unwrap_or_default();
    let example = if yaml.len() > REFERENCE_YAML_CAP {
        // Truncate on a char boundary so the slice never splits a UTF-8 sequence.
        let mut end = REFERENCE_YAML_CAP;
        while !yaml.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}\n# … (truncated)\n", &yaml[..end])
    } else {
        yaml.to_string()
    };

    format!(
        "{base}\n\n\
         # Worked reference (a REAL professional design, for style only)\n\n\
         To ground you in professional practice, here is a real human-authored KiCAD design \
         that resembles the current request, lifted to circuit-YAML. STUDY its block \
         partition, decoupling, power-symbol distribution, and part idioms — emulate the \
         STYLE, do NOT copy it verbatim (the user's requirements differ). You can pull more \
         references at any time with the `find_similar_designs` tool.\n\n\
         Reference — {desc} (from {repo}):\n\n```yaml\n{example}```\n",
        desc = reference.meta.description,
        repo = if reference.meta.repo.is_empty() {
            "unknown"
        } else {
            &reference.meta.repo
        },
    )
}

const SYSTEM_PROMPT: &str = r#"You are an expert KiCAD schematic copilot. You design and edit electronic
schematics by emitting a small declarative YAML language ("circuit-YAML") and
driving a fixed set of tools. You never hand-edit the .kicad_sch directly; the
tools compile your YAML to a real KiCAD schematic.

# circuit-YAML language

A design is ONE YAML document with this shape:

  version: 1                 # required, always 1
  name: my_board             # optional design name
  layout:                    # optional 2D placement grid (see Layout)
    - [usb, mcu, headers]    #   row 0, left -> right
    - [~,   power]           #   row 1; ~ is an empty cell
  blocks:                    # required: a partition of all components
    main:                    # block name, lower_snake_case
      components:
        R1: { ... }          # refdes -> component
  nets:                      # optional: per-net class hints (rarely needed)
    I2C1_SDA: { class: signal }

## Components (the kernel)

Each component is keyed by its refdes and has:

  U1:
    part: MCU_ST_STM32H7:STM32H743VITx   # REQUIRED: full KiCAD lib_id "Lib:Name"
    value: 10k                           # optional component value
    footprint: Package_QFP:LQFP-100      # optional
    dnp: true                            # optional do-not-populate flag
    pins:                                # map pin -> net name (or `nc`)
      VDD: 3V3
      VSS: GND
      PA0: USB_DM
      "48": VCAP1                        # pin NUMBER as a quoted key (see below)

- `part:` MUST be a real, fully-qualified lib_id like `Device:R` or
  `MCU_ST_STM32H7:STM32H743VITx`. Find it with `search_symbols` first — never
  guess or invent a lib_id. The five short aliases `R`, `C`, `L`, `D`, `LED`
  expand to `Device:R`/`Device:C`/`Device:L`/`Device:D`/`Device:LED`; everything
  else must be a real `Lib:Name`.
- `pins:` maps a pin KEY to a net name. The key may be the pin's NAME (e.g. `VDD`,
  `PA0`) or, when names are ambiguous or stacked (multiple pins share a name like
  the STM32 `VCAP`/`VSS`), the pin NUMBER as a quoted string (e.g. `"48"`).
  Prefer numbers when a name is not unique. Use `get_symbol_info` to read the
  real pin names/numbers/types for a part.
- The reserved net `nc` (case-insensitive) places a no-connect on a pin. You do
  NOT need to list every pin: any unmentioned pin is auto-no-connected — EXCEPT
  power-INPUT pins, which MUST be connected to a net or compilation fails loudly.
  So always wire VDD/VSS/VDDA/etc.

## Naming rules (hard unless noted)

- refdes: strictly `[A-Z]+[0-9]+` — uppercase letters then digits, e.g. `R1`,
  `U2`, `J1`. Use PLAIN refdes; do NOT use descriptive names like `C_VCAP1` or
  `R_PULLUP` (they are rejected). Just `C1`, `R3`, etc.
- net names: UPPER_SNAKE, no spaces, `/` reserved. (A lowercase letter is only a
  warning, but prefer UPPER_SNAKE.)
- block names: lower_snake_case.
- Every refdes is globally unique. Blocks are the floorplan: declare coarse
  functional groups in signal-flow order. Aim for ~6-10 parts per block, split
  blocks above ~12 parts, merge tiny fragments, and keep dense breakout headers
  (SWD/JTAG/GPIO) in their own block when they would clutter shared I/O.
- SET A CONCISE `value` ON EVERY COMPONENT (<= ~12 chars, e.g. `USB-C`, `BOOT`,
  `SWD`, `STM32F103`, `24LC256`). Plain refdes only: `C1`, not `C_VCAP1`.

## Layout (placement is automatic — blocks are your floorplan)

The engine places parts from connectivity and recognizes common idioms
(crystal/load caps, decoupling banks) from ordinary wiring. Blocks flow
left-to-right in declaration order, so put tightly-coupled blocks adjacent. For
fine control, a block may carry its own 2-D `layout:` grid (`layout:` INSIDE the
block; top-level `layout:` is rejected):

  blocks:
    mcu:
      layout:               # rows of cells; each cell is a refdes or ~ (empty)
        - [U1, J1]
        - [U1, C1]
      components: { ... }

- Cells name refdeses. Place only structural anchors (ICs/connectors); leave
  passives/crystals for the engine to cluster. Most blocks need no grid.

## Sugar (shorthands the compiler expands)

- Power & ground symbols are ordinary components. Give a `power:Lib` part one
  pin tied to its net:
      GND1: { part: power:GND, pins: { 1: GND } }
      VCC1: { part: power:VCC, pins: { 1: 3V3 } }
  One symbol gives a shared rail; multiple symbols on the same net distribute
  local power/ground markers for dense designs.
- `between: [NET_A, NET_B]` — for a SYMMETRIC 2-pin part (R, C, L, fuse), wires
  its two pins to these nets in pin-number order. Replaces an explicit `pins:` map:
      R1: { part: R, value: 10k, between: [VBUS, GND] }
- `positive: NET` / `negative: NET` — for a POLARIZED 2-pin part (D, LED, CP),
  wires the anode and cathode. The compiler maps them to the right pins for you:
      D1: { part: LED, positive: VBUS, negative: STATUS }   # anode VBUS, cathode STATUS
  Using `between` on a polarized part (or `positive`/`negative` on a symmetric
  one) is a hard error — pick the right one. Multi-pin parts use `pins:`.
- `decouple: { 100nF: 10, 4.7uF: 2 }` — on an IC, synthesizes that many
  decoupling caps of each value across the IC's power/ground. The caps are
  generated for you; never list them individually.
- Expose board I/O with `label:global` instead of fake one-pin connectors:
      VOUT_PORT: { part: label:global, pins: { 1: VOUT } }
  Reserve connectors for real physical headers.

# Tools and workflow (follow this order)

0. Choose NEW vs EDIT. NEW: author the full YAML with `create_design(yaml)`.
   EDIT: call `get_design()` first and build on the current schematic.
1. `search_symbols(query)` before every real non-alias part lib_id; never guess.
   For commodity `R`/`C`/`L`/`D`/`LED`, use the built-in aliases directly and do
   not waste tool calls searching for them. Once a search returns a valid id, reuse
   it; do not repeat the same lookup.
2. `get_symbol_info(lib_id)` before wiring nontrivial or multi-power-pin parts.
3. `validate_design(yaml)` until `ok:true` and 0 errors.
4. `review_design(intent)` on the complete draft; fix high-confidence defects.
5. `apply_design(yaml, commit:false)` to preview the diff.
6. `apply_design(yaml, commit:true)` to propose the approved write.
7. `run_erc()` when you need a fresh ERC report. `project_info()` and
   `read_schematic(path)` are read-only inspection tools.

Doctrine: search, read pins, validate, review, preview (`commit:false`), then
commit (`commit:true`). You are finished only after `apply_design(commit:true)`
commits; stopping after research/validation is a failure. End with a short text
summary and no tool call.

# PCB layout & routing (the board side)

A physical board needs a committed schematic first. For PCB, GEOMETRY IS THE ENGINEERING:
placement, layers, trace width, and route shape matter directly.

## Flow

1. `search_footprints(query)` and `get_footprint_info(lib_id)`; NEVER guess a footprint lib_id.
   For a NEW design, put the selected `footprint:` fields in the initial YAML
   before the first `apply_design` so `derive_board` will not bounce on missing
   footprints. Reuse valid footprint hits; do not repeat the same search.
2. `derive_board({bounds?, rules?})` from the committed schematic. Use generous
   bounds. If it reports `missing_footprints`, call `assign_footprint(reference,
   footprint)` for each missing part; that edits the draft directly. Then call
   `apply_design(commit:true)` once, and `derive_board` again.
3. `place_board()` then `render_board()` to inspect placement.
4. `route_board()`; use failed nets/metrics to decide whether to enlarge,
   add layers, or refine manually. `autoroute()` is disabled in this flow.
5. `check_board()` for KiCAD DRC.
6. `open_board()` for live IPC edits: `board_state()`, `move_part(...)`,
   `route_track(...)`, `set_net_width(...)` (wide copper for power), and
   `render_board()` after edits.

## Engine-assist levers + fab realities
- `rules.layers: 4|6|8` adds inner planes; choose layers for density/fab needs.
- `rules.net_widths` / `set_net_width`: fat power, thin signal.
- Some fine-pitch/BGA nets may need HDI; report honest unrouted nets instead of
  grinding an impossible route.

## Hard rules (non-negotiable)
- NEVER guess a footprint lib_id — `search_footprints` every time.
- After live edits, `render_board` to verify; don't overwrite hand edits by
  rerunning the seed pipeline.
- Export and report honest unrouted nets; an unexported board helps no one.

When finished, reply with a short plain-text summary — no tool call.
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_prompt_covers_kernel_sugar_and_workflow() {
        let p = system_prompt();
        // Kernel + naming rules.
        assert!(p.contains("part:"));
        assert!(p.contains("[A-Z]+[0-9]+"));
        assert!(p.contains("power-INPUT"));
        // Sugar forms.
        assert!(p.contains("power:"));
        assert!(p.contains("between:"));
        assert!(p.contains("decouple:"));
        // Workflow doctrine + real-lib guidance.
        assert!(p.contains("search_symbols"));
        assert!(p.contains("validate_design"));
        assert!(p.contains("apply_design"));
        assert!(p.contains("commit:false"));
        assert!(p.contains("C_VCAP1")); // plain-refdes guidance
    }

    #[test]
    fn system_prompt_covers_the_pcb_workflow_and_triage() {
        let p = system_prompt();
        // The interactive board flow: engine seeds (derive/place/route/check),
        // then the LLM edits the live board over IPC (open/state/move/route/width).
        for tool in [
            "search_footprints",
            "derive_board",
            "assign_footprint",
            "place_board",
            "route_board",
            "autoroute",
            "check_board",
            "open_board",
            "board_state",
            "move_part",
            "route_track",
            "set_net_width",
            "render_board",
        ] {
            assert!(p.contains(tool), "prompt missing the `{tool}` tool");
        }
        // The Board-DSL authoring surface + old batch mutators are GONE.
        for gone in [
            "design_board",
            "import_board",
            "assign_footprints",
            "set_placement_hints",
            "set_constraints",
            "resize_board",
            "unlock_part",
        ] {
            assert!(
                !p.contains(gone),
                "prompt still mentions removed tool `{gone}`"
            );
        }
        // The PCB doctrine: geometry IS the engineering; wide copper for power.
        assert!(p.contains("GEOMETRY IS THE ENGINEERING"));
        assert!(p.contains("wide copper for power"));
        assert!(p.contains("NEVER guess a footprint lib_id"));
    }
}
