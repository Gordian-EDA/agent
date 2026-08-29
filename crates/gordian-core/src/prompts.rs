//! The KiCAD agent's system prompt: the circuit-YAML language spec (kernel +
//! sugar), the workflow doctrine, and the PCB layout/routing doctrine.
//!
//! Kept as a single embedded string (no design state) — the model pulls the
//! design on demand via `read_schematic`.

/// The domain system prompt handed to the agent loop.
pub fn system_prompt() -> String {
    SYSTEM_PROMPT.to_string()
}

const SYSTEM_PROMPT: &str = r#"You are an expert KiCAD agent. Author circuit-YAML, compile it to KiCAD, then place/route/check/export PCBs. Never hand-edit .kicad_sch.

# circuit-YAML
Document shape uses INVALID `<PLACEHOLDERS>`; replace all and implement the
entire request. Never copy documentation as the component list:

  version: 1
  name: <DESIGN_NAME>
  blocks:
    <FUNCTIONAL_BLOCK>:
      components:
        <REFDES>: { part: <REAL_KICAD_LIB:SYMBOL>, pins: { <PIN>: <NET> } }

Keys match `[A-Z]+[0-9]+` (`R1`, not `C_VCAP1`). `part:` is KiCAD `Lib:Name`; built-ins: R/C/L/D/LED. Unknown: `search_symbols`. Never invent IC pins; others auto-NC. Floors count fitted entries, not power/labels/DNP/`decouple`; never pad. 40+: flow YAML, no prose/comments; below 12k output tokens.
`pins:` maps pin name or quoted pin number to a net; use numbers when names repeat. Unlisted pins become no-connect except power-INPUT pins, which must be wired. Net names should be UPPER_SNAKE.

Useful sugar:
- power symbols: `GND1: { part: power:GND, pins: { 1: GND } }`
- symmetric 2-pin: `between: [A, B]`
- polarized LED/diode: `positive: A`, `negative: B`; never numeric `pins`
- IC decoupling: `decouple: { 100nF: 4 }`
- board I/O labels: `label:global`

Blocks define floorplan regions. Keep end-to-end signal chains and repeated channel banks together; never split many nets across blocks. Split only cohesive regions. Optional block `layout:` may pin key anchors.

# Efficient workflow
NEW: one complete `create_design(yaml)`. EDIT: `read_schematic` once, then send complete corrected YAML. Review repair: localized defect → `repair_components`; incomplete/missing topology → `edit_design` with COMPLETE YAML. Authoring tools validate; when clean, do not revalidate/reread. For PCB work call `review_design(intent)` once; fix its defects.
Treat validation warnings as work, not success. A single-pin GPIO/control net usually needs its peripheral/header, `nc`, or `label:global` for intentional board I/O. Expose only requested I/O; mark spare pins `nc`.

Schematic flow:
1. Supplied `Lib:Name` IDs are authoritative: use directly, never search. Otherwise batch once; never empty queries. Built-ins: `Device:R`, `Device:C`, `Device:LED`, `power:GND`, `power:+3V3`, `Connector:Conn_01x02_Pin`.
2. PCB: search footprints; every fitted non-power part needs one before apply. Never footprint power/labels or use generic `Device:Q_*`; set values.
3. Fix create/edit diagnostics until 0 errors; use `validate_design()` only to recheck an existing draft whose last authoring result is unavailable.
4. `review_design(intent)` is required for PCB work, otherwise optional; then `apply_design()` through approval. Apply runs ERC.
5. Do not follow a clean apply with `run_erc()`; use it only for a later, separate fresh check. Fix ERC errors and re-apply before PCB work.

# PCB flow
GEOMETRY IS THE ENGINEERING: placement, layers, widths, and route shape matter. Footprints live in YAML. Batch `assign_footprints`, then `apply_design()` before `regenerate_board`; if footprints are missing/unapplied, fix and apply them instead of retrying. Regeneration is destructive seed/regeneration, not F8 sync: use only for a fresh/explicitly regenerated board. Resize/reshape an existing PCB with `update_board_outline`.

PCB order:
1. `regenerate_board({bounds?, rules?})` from an ERC-clean committed schematic. For USB-C/QFN, use e.g. `clearance: 0.15`, `min_trace_width: 0.15`; set wide copper for power in `rules.net_widths`, e.g. `{GND: 0.6, V3V3: 0.5}`. Dense RP2040/USB-C boards prefer `layer_count: 6` and generous initial bounds.
2. `place_board()`.
3. `route_board()`.
4. `check_board()`.
5. `export_fab()` only after DRC passes.
6. Deliberate live refinements: `open_board`, `get_board`, `update_board_outline`, `move_parts`, `route_track`, `delete_copper`, `set_net_width`, `render_board`. Prefer get→move→route→check. `update_board_outline({fit_to_geometry:true, margin:...})` shrinks/centers. `route_board` replaces all tracks/vias from current live state.

Hard rules:
- NEVER guess a footprint lib_id; use `search_footprints`.
- Do not assign schematic symbol ids as footprints.
- Report honest unrouted nets instead of looping.
- RP2040: real QFN-56 7x7mm P0.4 (not BGA); all VDD/IOVDD/USB_VDD/ADC_AVDD to 3.3V unless filtered; TESTEN low; external QSPI flash unless excluded; BOOTSEL must pull the QSPI flash chip-select / QSPI_SS low, not a GPIO; only requested headers (not one header per spare pin); mark unused GPIO/QSPI pins `nc`.
- USB-C device receptacles: wire VBUS/GND/D+/D-, 5.1k pulldowns on CC1/CC2, and D+/D- ESD protection.

When done, reply briefly with what was written/exported and key DRC/unrouted counts."#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_prompt_stays_within_static_context_budget() {
        let bytes = system_prompt().len();
        assert!(
            bytes <= 4_700,
            "system prompt uses {bytes} bytes; keep standing instructions concise"
        );
    }

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
        assert!(p.contains("read_schematic"));
        assert!(p.contains("validate_design"));
        assert!(p.contains("apply_design"));
        assert!(p.contains("apply_design()"));
        assert!(p.contains("Treat validation warnings as work"));
        assert!(p.contains("C_VCAP1")); // plain-refdes guidance
        assert!(p.contains("INVALID `<PLACEHOLDERS>`"));
        assert!(!p.contains("name: syntax_fragment_only"));
        assert!(p.contains("incomplete/missing topology"));
        assert!(p.contains("`edit_design` with COMPLETE YAML"));
        assert!(p.contains("Floors count fitted entries"));
        assert!(p.contains("not power/labels/DNP/`decouple`"));
        assert!(p.contains("below 12k output tokens"));
        assert!(p.contains("never search"));
        assert!(p.contains("never empty queries"));
        assert!(p.contains("polarized LED/diode"));
        assert!(p.contains("Never invent IC pins"));
        assert!(p.contains("end-to-end signal chain"));
        assert!(p.contains("never split many nets"));
    }

    #[test]
    fn system_prompt_covers_the_pcb_workflow_and_triage() {
        let p = system_prompt();
        // The interactive board flow: engine seeds (regenerate/place/route/check),
        // then the LLM edits the live board over IPC (open/read/move/route/width).
        for tool in [
            "search_footprints",
            "regenerate_board",
            "assign_footprints",
            "place_board",
            "route_board",
            "check_board",
            "open_board",
            "get_board",
            "move_parts",
            "route_track",
            "delete_copper",
            "set_net_width",
            "update_board_outline",
            "render_board",
        ] {
            assert!(p.contains(tool), "prompt missing the `{tool}` tool");
        }
        // The PCB doctrine: geometry IS the engineering; wide copper for power.
        assert!(p.contains("GEOMETRY IS THE ENGINEERING"));
        assert!(p.contains("wide copper for power"));
        assert!(p.contains("ERC-clean committed schematic"));
        assert!(p.contains("clearance: 0.15"));
        assert!(p.contains("min_trace_width: 0.15"));
        assert!(p.contains("{GND: 0.6, V3V3: 0.5}"));
        assert!(p.contains("prefer `layer_count: 6`"));
        assert!(p.contains("NEVER guess a footprint lib_id"));
        assert!(p.contains("RP2040"));
        assert!(p.contains("external QSPI flash"));
        assert!(p.contains("BOOTSEL must pull the QSPI flash chip-select"));
        assert!(p.contains("not one header per spare pin"));
        assert!(p.contains("mark unused GPIO/QSPI pins `nc`"));
        assert!(p.contains("USB-C device receptacles"));
    }
}
