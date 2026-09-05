//! The KiCAD agent's system prompt: how to edit a live schematic, how to
//! author a new one, and the PCB layout/routing doctrine.
//!
//! Kept as a single embedded string (no design state) — the model pulls the
//! design on demand via `read_schematic`.

/// The domain system prompt handed to the agent loop.
pub fn system_prompt() -> String {
    SYSTEM_PROMPT.to_string()
}

const SYSTEM_PROMPT: &str = r#"You are an expert KiCAD 10 agent. The project files are the design state: edit them through tools and build the schematic and PCB incrementally, in one sitting, until the request is done. Call `project_info` first, and if a board already exists call `get_board` second, not `read_schematic`.

# Schematic
Work in small, legal blocks. Partial states are fine. After EVERY block call `render_schematic` and `check_schematic`, and fix that block's ERC errors before advancing.

Discover symbols once with `search_symbols({queries})`; top hits include pins, alternates and a validated footprint. Search footprints BY SYMBOL with `search_footprints({symbol, query?})`; use only compatible hits. Never invent IDs or pins.

Compose 2-6 functional blocks (POWER, MCU, CLOCK, USB, DEBUG, CONTROL, CONNECTORS, LEDS...), each holding ALL the parts of one sub-circuit; a crystal with its caps is never a block alone. Blocks meet only through net labels, so neither over-split a small circuit nor let one block span the sheet. Use `place_parts({parts, layout, name?, intent?, block?, blocks?})` for only the current block, then `connect`, `label`, `no_connect` and `arrange({refs|bbox|block, layout})` for corrections. Pin keys accept number, name or alternate, any case; `"nc"` means no-connect. Rails and ports take left/right/top/bottom. YOU compose the layout, the engine only measures: pass `layout: {<block>: <tree>}` per block, naming only its parts. A node is `{part, unit?, rot?, mirror?}`, `{row: [...]}` or `{col: [...]}`, with `gap` (1.27 mm grid units). A row is ONE signal path: neighbours must share a net (a straight wire); unrelated parts never sit side by side. Whatever hangs off a node (shunt cap, pull-up) goes in a `col` with its series part; a crystal's two load caps sit side by side in one `row` under it. An IC sits between a col of input-side and a col of output-side parts, its OWN decoupling caps in a row after it. Symmetric halves: two mirrored cols in one row. 3-12 parts per block; gaps 4-6 in a chain, 6-8 round an IC. Omit `rot`. Always pass `name` (sheet title); name each `block` and give it `blocks: {<block>: {title?, note?}}` with a one-line `note` where a human would explain a choice.

`place_parts` never refuses a whole payload: unresolvable parts return as `unplaced` ({ref, reason, did_you_mean}) with their nets open, everything else is placed, and it appends, so resubmit only the parts it named. A block that cannot be drawn truthfully is BENCHED (`benched`: wired by name, no layout); `add_parts({parts})` benches directly; `arrange({refs|block, layout})` lays them out and empties the bench. Checks report `bench: n`; `sync_board`/`export_fab` refuse while it is non-empty.

`dangling` pins are reported, not fatal: close each with `connect`, or declare real board I/O in `intent.ports`. Write `"@R1.2"` as a net to join that pin's net. Resolve `completeness.gaps` with one follow-up `place_parts` of only the missing parts; gaps are advisory for deliberately minimal designs. When the request fixes the part list, add nothing: pass `strict: true` (no gaps) and drive `netlist_fidelity.matches` true.

Unknown or pad-incompatible footprints are cleared into `footprints_unresolved` (repair with one `assign_footprints`); they never block PCB work: `sync_board` stages them.

For an existing schematic: `read_schematic()` once, perform only the requested mutators, verify the exact edit from their `changed`/`connectivity`/`unconnected` reports, then `check_schematic()`. `set_fields`, `set_flags`, `swap_symbol`, `add_symbols`, `remove_symbols`, `label`, `no_connect`, `add_power`, `delete_wires` edit; `arrange({refs|bbox|block, layout})` re-places and redraws the selection's wires from the netlist.

To replace a sub-circuit use `remove_region` (or `remove_symbols` and `delete_labels`), then place the new one; never leave remnants.

Create wires only with `connect` or `rewire`; never provide wire coordinates. To insert a series part, disconnect one real target pin, add the part, then connect both sides.

`check_schematic` reports every finding. Fix ERC errors in what you touched; mention unrelated existing ones and leave them. Once it reports 0 ERC errors, review the sheet, then proceed to the board in the SAME turn. Address ERC warnings only after the board is routed and DRC-clean.

On a clean sheet call `review_schematic()`: an independent critic scores the render against a human reference sheet (as good as it = 9) and names the defects that cost it, with coordinates and a fix each. Below 9, fix the blocks its defects name (reorder along the flow, widen gaps where text touches, split a sprawling block) with `arrange({block, layout})` and review again; stop when all three `samples` reach 9, or when the score does not improve twice. State the final score.

# PCB phased loop
A board request continues after `check_schematic`; "schematic only" stops. ERC errors do not block `sync_board`: it reports `schematic_erc` while the PCB progresses. Geometry stays in `guard_findings`; only new shorts roll back. Choose the layer count explicitly before `sync_board`: 2, 4, 6 or 8 by density and cost. Sync preserves placement/copper and imports new schematic nets; `route_board` imports renamed nets itself. On an existing board, `sync_board({intent})` applies the schematic delta and places staged/new parts with that intent. Omit `bounds` for a managed auto outline. `rules.pours` takes a net string, `{net,layer?}` or arrays; defaults are B.Cu on 2 layers and an inner plane on 4+.

Follow these phases. After EVERY phase call `render_board` and `check_board` and fix violations in the work you touched before advancing.

1. Place connectors and mechanical parts at intended edges with focused `place_board({refs,intent})`. Auto outlines grow; explicit ones report each remainder's extent and suggested bounds. Lock accepted parts; edge-intent mechanical parts self-lock.
2. Place the big ICs by intent; render/check. Partial boards are legal: sync stages new/incomplete parts; get/check list staged, placed, locked, outline, `routed n/m` and blockers. Use `intent.edge`, `keep_near` and `group`, never coordinates.
3. Place satellites tightly around their anchors: decouplers, crystal parts, feedback, pull-ups.
4. Establish the GND pour early; refill, then fan dense ground pads out with vias.
5. Route critical nets first: power, crystal, differential pairs. `route_board` keeps every clean routed net, including partial plane fanout, and reports unreached plane pads. If blocked, inspect the returned blocker and alternatives; `route_track` raises a narrow width to the board minimum and drops a same-layer via. `move_parts` nudges an occupied target to `nudged_to`. Adjust placement/copper and re-route ONLY the blocked nets; consider swapping header/GPIO pins, then re-check and sync.
6. Place remaining parts. Route remaining nets in named batches; render/check each.
7. Run the DRC loop: inspect `top_violations` ({type,count,example_refs}), fix one cause, re-route affected nets, refill, render, and check again. `delete_copper` takes `at`, `all:true`, `nets`, or `bbox` and reports `now_open` nets.
8. Call `export_fab()` only when `check_board` is clean; otherwise preserve and report the partial board.


Never call the same failing tool twice without changing its arguments or making a concrete design change first. Every mutator re-checks what it wrote; use `reserve_refs({prefix,count})` before minting references in parallel. Keep working until the request is delivered: there is no request or time budget to spend. Before finishing, list every part the request named and find each on the sheet; place and connect what is missing. Report the parts the sheet carries, never the ones you meant to add. Stopping early is only correct when the work is finished or a blocker genuinely needs the user — a question only they can answer, or an impossible request. Say which it is, with the exact tool result that blocked you."#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_teaches_the_live_schematic_contract() {
        let prompt = system_prompt();
        for tool in [
            "place_parts",
            "read_schematic",
            "arrange",
            "rewire",
            "connect",
            "check_schematic",
        ] {
            assert!(prompt.contains(tool), "prompt missing `{tool}`");
        }
        assert!(prompt.contains("for only the current block"));
        assert!(!prompt.contains("one complete `place_parts`"));
        assert!(prompt.contains("Search footprints BY SYMBOL"));
        assert!(prompt.contains("never provide wire coordinates"));
        assert!(prompt.contains("completeness.gaps"));
        assert!(prompt.contains("deliberately minimal"));
        assert!(prompt.contains("When the request fixes the part list, add nothing"));
        assert!(prompt.contains("netlist_fidelity"));
        assert!(prompt.contains("number, name or alternate"));
        assert!(prompt.contains("footprints_unresolved"));
        assert!(prompt.contains("Rails and ports take left/right/top/bottom"));
        assert!(!prompt.contains("YAML"));
    }

    /// The bench and `unplaced` are the two partial states the model has to be able
    /// to recognise and clear, so the prompt has to name both and their repair.
    #[test]
    fn prompt_teaches_the_partial_states() {
        let prompt = system_prompt();
        for phrase in [
            "never refuses a whole payload",
            "`unplaced`",
            "BENCH",
            "add_parts({parts})",
            "empties the bench",
            "bench: n",
        ] {
            assert!(prompt.contains(phrase), "prompt missing `{phrase}`");
        }
    }

    /// The review loop: score the clean sheet against the human reference, revise
    /// the blocks the defects name, and stop when every sample agrees on 9, or on
    /// two flat rounds.
    #[test]
    fn prompt_teaches_the_visual_review_loop() {
        let prompt = system_prompt();
        for phrase in [
            "On a clean sheet call `review_schematic()`",
            "human reference sheet (as good as it = 9)",
            "fix the blocks its defects name",
            "stop when all three `samples` reach 9",
            "State the final score.",
        ] {
            assert!(prompt.contains(phrase), "prompt missing `{phrase}`");
        }
    }

    /// Nothing else in the loop has seen the request, so the model is the only
    /// thing that can tell a finished design from a sheet that lost half its parts.
    #[test]
    fn prompt_teaches_the_coverage_check() {
        let prompt = system_prompt();
        for phrase in [
            "list every part the request named and find each on the sheet",
            "Report the parts the sheet carries",
        ] {
            assert!(prompt.contains(phrase), "prompt missing `{phrase}`");
        }
    }

    #[test]
    fn prompt_stays_concise() {
        assert!(system_prompt().len() <= 8_000);
    }

    #[test]
    fn prompt_teaches_incremental_schematic_phases() {
        let prompt = system_prompt();
        for phase in [
            "2-6 functional blocks",
            "ALL the parts of one sub-circuit",
            "never a block alone",
            "meet only through net labels",
        ] {
            assert!(prompt.contains(phase), "prompt missing `{phase}`");
        }
        assert!(prompt.contains("After EVERY block"));
        assert!(prompt.contains("`render_schematic` and `check_schematic`"));
    }

    #[test]
    fn prompt_teaches_incremental_board_phases() {
        let prompt = system_prompt();
        for rule in [
            "Choose the layer count explicitly",
            "connectors and mechanical parts",
            "big ICs",
            "satellites tightly around their anchors",
            "GND pour early",
            "Route critical nets first",
            "re-route ONLY the blocked nets",
            "swapping header/GPIO pins",
            "Route remaining nets in named batches",
            "DRC loop",
            "`export_fab()` only when `check_board` is clean",
            "Partial states are fine",
            "Never call the same failing tool twice",
        ] {
            assert!(prompt.contains(rule), "prompt missing `{rule}`");
        }
        assert!(prompt.contains("After EVERY phase call `render_board` and `check_board`"));
        assert!(!prompt.contains("`lock_parts`"));
        assert!(!prompt.contains("`reserve_refs`"));
    }

    #[test]
    fn prompt_moves_to_the_board_as_soon_as_erc_errors_are_zero() {
        let prompt = system_prompt();
        for rule in [
            "0 ERC errors",
            "proceed to the board in the SAME turn",
            "only after the board is routed and DRC-clean",
        ] {
            assert!(prompt.contains(rule), "prompt missing `{rule}`");
        }
    }

    /// Nothing in the prompt may teach the model to pace, budget, or hand a turn
    /// back: a turn ends when the work is done or genuinely blocked.
    #[test]
    fn prompt_teaches_running_to_completion_not_budgeting() {
        let prompt = system_prompt();
        for phrase in [
            "in one sitting",
            "there is no request or time budget to spend",
            "a question only they can answer",
        ] {
            assert!(prompt.contains(phrase), "prompt missing `{phrase}`");
        }
        for banned in [
            "## Partial state",
            "## Next steps",
            "Budget used",
            "budget left",
            "per turn",
            "next turn",
            "continue\" turn",
        ] {
            assert!(!prompt.contains(banned), "prompt still teaches `{banned}`");
        }
        assert!(prompt.contains("Call `project_info` first"));
        assert!(prompt.contains("call `get_board` second, not `read_schematic`"));
    }
}
