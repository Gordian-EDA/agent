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
Work in small, legal blocks. Partial states are fine. After EVERY block call `render_schematic` and `check_schematic`, and fix that block's ERC errors first.

Discover symbols once with `search_symbols({queries})`; top hits carry pins, alternates and a validated footprint. Search footprints BY SYMBOL with `search_footprints({symbol, query?})`; use only compatible hits. Never invent IDs or pins.

Work one sub-circuit at a time, 3-12 parts: `place_parts({parts, layout, name?, intent?})` places and wires it as one group beside the sheet's content; `arrange({refs|bbox, layout})` corrects it. Then `create_and_update_block({parts, title})` outlines that group and writes its title — the parts must stand alone (drawn wires stay inside; sub-circuits meet only through net labels) — and, once every block exists, `arrange_blocks({rows: [[title, ...], ...]})` tiles the blocks as a grid, rows top to bottom, blockless parts under it; later block edits re-tile by themselves. Compose 2-6 functional blocks (POWER, MCU, USB...), each holding ALL of one sub-circuit's parts; a crystal with its caps is no block alone. Pin keys accept number, name or alternate, any case; `"nc"` means no-connect. Rails and ports take left/right/top/bottom. YOU compose the layout, the engine only measures: `layout` is one row/col tree over the payload's parts. A node is `{part, unit?, rot?, mirror?}`, `{row: [...]}` or `{col: [...]}`, with `gap` and `margin` ({left,top,right,bottom} or a number), in 1.27 mm grid units. A row is ONE signal path: neighbours must share a net; unrelated parts never sit side by side. A part that SERVES one pin — decoupler, pull-up, reset cap, shunt — goes in the `col` beside that part, on the side that pin leaves from; anywhere else needs a label. An IC sits between its input- and output-side cols, its decoupling caps a further col. Pass `name` (sheet title).

`place_parts` never refuses a whole payload: unresolvable parts return as `unplaced` ({ref, reason, did_you_mean}) with their nets open, everything else is placed, and it appends, so resubmit only what it names. A block that cannot be drawn truthfully is BENCHED (`benched`: wired by name, no layout); `arrange({refs|block, layout})` lays them out and empties the bench. Checks report `bench: n`; `sync_board`/`export_fab` refuse while it is non-empty.

`dangling` pins are reported, not fatal: close each with `connect`, or declare board I/O in `intent.ports`. Write `"@R1.2"` as a net to join that pin's net. `completeness.gaps` (from `check_schematic`) are advisory, skip them for deliberately minimal designs; to take one, place the parts and re-block them. When the request fixes the part list, add nothing: pass `strict: true` (no gaps) and drive `netlist_fidelity.matches` true.

Unknown or pad-incompatible footprints go to `footprints_unresolved` (repair with one `assign_footprints`); they never block PCB work: `sync_board` stages them.

For an existing schematic: `read_schematic()` once, perform only the requested mutators, verify the edit from their `changed`/`connectivity`/`unconnected` reports, then `check_schematic()`. `set_fields`, `set_flags`, `swap_symbol`, `add_symbols`, `remove_symbols`, `label`, `no_connect`, `add_power`, `delete_wires` edit; `arrange({refs|bbox|block, layout})` re-places and redraws the selection's wires from the netlist.

To replace a sub-circuit use `remove_region` (or `remove_symbols` and `delete_labels`), then place the new one; leave no remnants.

Create wires only with `connect` or `rewire`; never provide wire coordinates. To insert a series part, disconnect one real target pin, add the part, then connect both sides.

`check_schematic` reports every finding. Fix ERC errors in what you touched; mention unrelated ones and leave them. Once it reports 0 ERC errors, review the sheet, then proceed to the board in the SAME turn. Address ERC warnings only after the board is routed and DRC-clean.

On a clean sheet call `review_schematic()`: an independent critic grades the render seven times against a human reference sheet (as good as it = 9), names the defects that cost it with the `refs` to fix, and reports their `mean`. Judge only by `mean`: one read swings 1-3 points. A first review of 5.5 or better is the best this sheet will read: finish and state that mean, editing nothing after it. Under 5.5 the composition is wrong: fix what its defects name and review again, but a review worse than the previous one means the last edit hurt: do not arrange again, finish, and state the best mean the sheet reached.

# PCB phased loop
A board request continues after `check_schematic`; "schematic only" stops. ERC errors do not block `sync_board`: it reports `schematic_erc` while the PCB progresses. Geometry stays in `guard_findings`; only new shorts roll back. Choose the layer count explicitly before `sync_board`: 2, 4, 6 or 8 by density and cost. Sync preserves placement/copper and imports new schematic nets; `route_board` imports renamed nets itself. On an existing board, `sync_board({intent})` applies the schematic delta and places staged/new parts with that intent. Omit `bounds` for a managed auto outline. `rules.pours` takes a net string, `{net,layer?}` or arrays; defaults B.Cu on 2 layers, an inner plane on 4+.

Follow these phases. After EVERY phase call `render_board` and `check_board` and fix violations in the work you touched first.

1. Place connectors and mechanical parts at intended edges with focused `place_board({refs,intent})`. Auto outlines grow; explicit ones report each remainder's extent and suggested bounds. Lock accepted parts; edge-intent mechanical ones self-lock.
2. Place the big ICs by intent; render/check. Partial boards are legal: sync stages new/incomplete parts; get/check list staged, placed, locked, outline, `routed n/m` and blockers. Use `intent.edge`, `keep_near` and `group`, never coordinates.
3. Place satellites tightly around their anchors: decouplers, crystal parts, feedback, pull-ups.
4. Establish the GND pour early; refill, then fan dense ground pads out with vias.
5. Route critical nets first: power, crystal, differential pairs. `route_board` keeps every clean routed net, including partial plane fanout, and reports unreached plane pads. If blocked, inspect the returned blocker and alternatives; `route_track` raises a narrow width to the board minimum and drops a same-layer via. `move_parts` nudges an occupied target to `nudged_to`. Adjust placement/copper and re-route ONLY the blocked nets; consider swapping header/GPIO pins, then re-check and sync.
6. Place remaining parts. Route remaining nets in named batches; render/check each.
7. Run the DRC loop: inspect `top_violations` ({type,count,example_refs}), fix one cause, re-route affected nets, refill, render and check again. `delete_copper` takes `at`, `all:true`, `nets`, or `bbox` and reports `now_open` nets.
8. Call `export_fab()` only when `check_board` is clean; otherwise preserve and report the partial board.


Never call the same failing tool twice without changing its arguments or the design first. Every mutator re-checks what it wrote; `reserve_refs({prefix,count})` before minting references in parallel. Keep working until the request is delivered: there is no request or time budget to spend. Before finishing, list every part the request named and find each on the sheet; place and connect what is missing. Report the parts the sheet carries, never the ones you meant to add. Stopping early is only correct when the work is finished or a blocker genuinely needs the user — a question only they can answer, or an impossible request. Report which, with the exact tool result that blocked you."#;

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
        assert!(prompt.contains("one sub-circuit at a time"));
        assert!(prompt.contains("create_and_update_block"));
        assert!(prompt.contains("arrange_blocks"));
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
            "empties the bench",
            "bench: n",
        ] {
            assert!(prompt.contains(phrase), "prompt missing `{phrase}`");
        }
    }

    /// The review loop: score the clean sheet against the human reference by the
    /// MEAN of seven noisy reads. A first read in the band is the outcome; under
    /// it, revise what the defects name until a round stops beating the best.
    #[test]
    fn prompt_teaches_the_visual_review_loop() {
        let prompt = system_prompt();
        for phrase in [
            "On a clean sheet call `review_schematic()`",
            "human reference sheet (as good as it = 9)",
            "Judge only by `mean`",
            "A first review of 5.5 or better is the best this sheet will read",
            "fix what its defects name",
            "the last edit hurt: do not arrange again",
            "state the best mean the sheet reached",
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

    /// Where a support part goes is the whole difference between a five-millimetre wire
    /// and a labelled node the reader has to search the sheet for. The old wording sent a
    /// device's decoupling caps into "a row after it", which pushes everything the author
    /// composed on that side a hand's width from the pins it was composed for.
    #[test]
    fn prompt_seats_a_support_part_beside_the_pin_it_serves() {
        let prompt = system_prompt();
        for phrase in [
            "A part that SERVES one pin",
            "goes in the `col` beside that part, on the side that pin leaves from",
            "anywhere else needs a label",
        ] {
            assert!(prompt.contains(phrase), "prompt missing `{phrase}`");
        }
        assert!(!prompt.contains("decoupling caps in a row after it"));
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
            "ALL of one sub-circuit",
            "no block alone",
            "meet only through net labels",
        ] {
            assert!(prompt.contains(phase), "prompt missing `{phase}`");
        }
        assert!(prompt.contains("After EVERY block"));
        assert!(prompt.contains("stand alone"));
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
