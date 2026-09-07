//! `gordian-tools-sch` — the agent's schematic tool surface.
//!
//! The `.kicad_sch` file is the design. Every tool here opens it, changes one
//! thing, and writes it back; there is no draft, no intermediate language and
//! no whole-file regeneration, so a part the model did not name keeps its
//! position byte for byte.
//!
//! ## The contract every mutator keeps
//!
//! A call declares what it is about to touch — a net name, a part — and the
//! [`session::Edit`] transaction enforces it: snapshot, edit, re-extract the
//! net partition, diff. A change to connectivity the call did not name is
//! rolled back and reported instead of written. That is what makes "replace
//! this resistor" safe on a hand-drawn sheet with a hundred wires on it.
//!
//! Wires are never accepted by coordinate. [`wiring::connect_tool`] routes over
//! the live obstacle scene and falls back to a matched pair of labels, saying
//! so; that is the only way copper is drawn.

mod blocks;
mod bulk;
mod check;
mod edit;
mod place;
mod query;
mod refs;
pub mod render;
pub mod review;
mod session;
mod wiring;

use anyhow::Result;
use gordian_llm::Tool;
use gordian_runtime::AgentRuntime;
use serde_json::{Value, json};

/// The tools that write the schematic.
pub const MUTATORS: [&str; 10] = [
    "place_parts",
    "arrange",
    "create_and_update_block",
    "arrange_blocks",
    "remove_symbols",
    "set_fields",
    "swap_symbol",
    "connect",
    "no_connect",
    "delete_wires",
];

/// Whether `name` is one of this crate's tools.
pub fn handles(name: &str) -> bool {
    tool_names().contains(&name)
}

fn tool_names() -> Vec<&'static str> {
    let mut names = vec![
        "read_schematic",
        "check_schematic",
        "search_footprints",
        "render_schematic",
        "review_schematic",
    ];
    names.extend(MUTATORS);
    names
}

const PIN: &str = "A pin as \"<ref>.<number>\" or \"<ref>.<name>\", e.g. \"U1.VDD\".";

/// The JSON-Schema tool definitions handed to the model.
pub fn tool_defs() -> Vec<Tool> {
    let defs: Vec<(&str, &str, Value)> = vec![
        (
            "place_parts",
            "The ONLY way to create a new design or add a multi-part block. Submit a COMPLETE electrically finished functional block in one call, including its support, protection, decoupling, bias, termination, indicator, and connector parts. If the call would leave 60 or more parts on the sheet, split the design into named functional blocks and set `block` on every payload; add one block per call. Those later calls use region placement and freeze every existing symbol. State connectivity only: real KiCAD parts and pin-to-net mappings, never coordinates or wires. Pin keys accept physical numbers, names, or alternate functions case-insensitively; `PH0-OSC_IN` selects PH0 by its alternate. Before placement, the complete payload is validated and writes nothing on electrical failure: explicit refs must be unused, and every new named signal pin must land on a net with at least one other pin across the payload and existing sheet; power rails, declared ports, and `nc` are terminal nets. A requested net on a library no-connect pin becomes `nc` and is returned under `nc_overridden` plus `gaps`; use a functional pin if that connection matters. A footprint is metadata: a unique same-library pad-compatible repair is applied and reported as `footprint_resolved`; otherwise the part is placed without it and `footprints_unresolved` plus a completeness gap name the follow-up for one `set_fields({footprints})` call. Omit `ref` to auto-assign the lowest unused designator from the library symbol. The result reports extractor-verified `connectivity` and `unconnected` pins; trust it instead of re-reading. Its `gaps` are this call's own unresolved items (footprints, no-connect overrides); missing-support findings come from check_schematic. Rails and ports accept left, right, top, or bottom. A multi-unit symbol (dual op-amp, quad gate) is placed ONCE, in the call that holds every sub-circuit using it, with one `{part, unit}` leaf per unit it uses; `unit` belongs on layout leaves, not on parts. YOU compose the drawing: give `layout` a row/col tree per block, keyed by that block's own name and naming every one of its parts exactly once — a block without one is drawn as a bare row and says so. If rejected, correct every reported diagnostic before retrying; unknown-pin errors return ranked suggestions.",
            sch_check::place_parts_input_schema(),
        ),
        (
            "arrange",
            "Re-place selected symbols and redraw their wiring FROM THE NETLIST, while every unselected symbol stays frozen. This is also how a symbol leaves the bench. Select by `refs` (bench included), `bbox`, or `block`; `layout` gives the selection's row/col tree and `intent` its rails and ports, exactly as in place_parts. The typesetter owns all coordinates; a net it cannot draw as a wire is left as a matching label and reported.",
            bulk::selection_schema(true),
        ),
        (
            "create_and_update_block",
            "Make the listed placed parts one block: outline them and write the title inside the outline's bottom edge. The parts must stand alone — every drawn wire from one of them ends on another of them (blocks meet only through net labels) — and connect to each other. A title the sheet already has is redefined with these parts. Power flags, rails and refs not on the sheet named among the parts are ignored and reported. Standalone parts outside every block are allowed.",
            blocks::create_block_schema(),
        ),
        (
            "arrange_blocks",
            "Lay the blocks out as a grid: `rows` of block titles, top to bottom, each left to right. Every block moves rigidly, its outline is re-fitted to its cell with the contents centred, blocks in a row share a height, and whatever the rows leave out — parts of no block, blocks not named — goes in a row under the grid. Call it once the blocks exist; from then on every block edit (create_and_update_block, arrange) re-tiles to the same rows by itself, a new block joining the last row — call it again only to change the rows.",
            blocks::arrange_blocks_schema(),
        ),
        (
            "read_schematic",
            "Read the live schematic as aligned plain text grouped into sorted parts and units, individually addressable power symbols and labels with UUIDs, nets, loose pins, and warnings. `ref` reads one part instead (fields, flags, per-unit pose, pin geometry and nets); `net` reads one net (its pins and every label naming it; a pin like \"R8.2\" reads that pin's net).",
            json!({
                "type": "object",
                "properties": {
                    "region": { "type": "array", "items": { "type": "number" }, "minItems": 4, "maxItems": 4,
                                "description": "Only symbols inside [x1,y1,x2,y2] mm." },
                    "detail": { "type": "string", "enum": ["compact", "full"],
                                "description": "full adds footprints and uuids." },
                    "ref": { "type": "string", "description": "Read this one part." },
                    "net": { "type": "string", "description": "Read this one net, or the net of this pin." }
                },
                "additionalProperties": false
            }),
        ),
        (
            "check_schematic",
            "Lint + electrical rules + KiCAD ERC over the live file. Every finding includes its source and an executable `{tool,args}` fix or `null` with a reason; `fix_groups` coalesces findings closed by one call. `ok` and `erc_clean` consider every live error. Compact results use at most forty finding lines; pass `detail: true` to get all. Fix findings in what you touched; leave unrelated existing findings alone and mention them. Completeness findings are advisory: resolve them when the request implies a complete powered/interface design, but never expand a deliberately minimal or focused edit.",
            json!({
                "type": "object",
                "properties": {
                    "detail": {
                        "type": "boolean",
                        "description": "Return every finding instead of the compact forty-finding view."
                    }
                },
                "additionalProperties": false
            }),
        ),
        (
            "render_schematic",
            "Render the schematic to a PNG with mm axes to check the visual result; use it whenever you want to see what an edit did. Not a substitute for `check_schematic`.",
            json!({ "type": "object", "properties": {} }),
        ),
        (
            "review_schematic",
            "An independent visual critic scores the rendered sheet 0-10 against a human-drawn reference sheet rated 9 (as good as it = 9, clearly better = 10) and returns the defects that cost it, each with `at_mm` sheet coordinates, the `refs` involved and a concrete `fix`. Call it once `check_schematic` is clean on a sheet you drew; below 9, re-lay-out the blocks the defects name and review again. Not for an edit of someone's existing sheet: its parts are not yours to move.",
            json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        ),
        (
            "remove_symbols",
            "Delete symbols by reference or UUID, including #PWR/#FLG symbols, plus no-connects, \
             welded power flags, wire runs and labels that only served their pins; reports counts \
             by kind and surviving pins made loose. Or delete a whole region by `block` name or \
             `bbox`: every symbol, label, wire, junction and text in it goes, wires crossing the \
             boundary are cut there, and each surviving outside endpoint is reported with its old \
             net so a replacement can reconnect it.",
            json!({
                "type": "object",
                "properties": {
                    "refs": { "type": "array", "items": { "type": "string" }, "minItems": 1,
                              "description": "Reference designators or symbol UUIDs." },
                    "block": { "type": "string", "description": "A block title or region name: remove the whole block." },
                    "bbox": { "type": "array", "items": {"type": "number"}, "minItems": 4, "maxItems": 4,
                              "description": "Remove everything inside [x1,y1,x2,y2] mm." }
                },
                "anyOf": [{"required": ["refs"]}, {"required": ["block"]}, {"required": ["bbox"]}],
                "additionalProperties": false
            }),
        ),
        (
            "set_fields",
            "Set properties on one part (Value, Reference, Footprint, user fields) on every unit of it, and its DNP / in-BOM flags. A Footprint is resolved and checked against the symbol's pads before anything is written; a refusal names the closest compatible footprint. `footprints` sets footprints on many parts atomically. Moves nothing.",
            json!({
                "type": "object",
                "properties": {
                    "ref": { "type": "string" },
                    "fields": { "type": "object", "minProperties": 1,
                                "additionalProperties": { "type": ["string", "null"] } },
                    "dnp": { "type": "boolean" },
                    "in_bom": { "type": "boolean" },
                    "footprints": { "type": "object", "minProperties": 1,
                                    "description": "Reference -> KiCAD Lib:Name footprint, for many parts at once.",
                                    "additionalProperties": { "type": "string" } }
                },
                "anyOf": [{"required": ["ref", "fields"]}, {"required": ["ref", "dnp"]}, {"required": ["ref", "in_bom"]}, {"required": ["footprints"]}],
                "additionalProperties": false
            }),
        ),
        (
            "swap_symbol",
            "Retarget a part at a different library symbol, keeping every pin's net by number. \
             When a number has no counterpart, names map automatically case-insensitively while \
             ignoring `~`, `_`, and `-`; the result reports `mapped_by_name`. `ref` names the whole \
             part, so every unit of a dual or quad swaps at once. Use `pin_map` {old_pin: new_pin} \
             when the pinout differs. A refusal returns `suggestion.pin_map`, old pins with no \
             counterpart, and the new symbol's unassigned pins with number, name, and type. For a \
             value/footprint change alone, or when no real match exists anywhere, use set_fields \
             instead — a same-named part in an unrelated library is not proven pin-compatible. \
             A supplied or inherited footprint is metadata: a unique same-library pad-compatible \
             repair is applied and reported as `footprint_resolved`; otherwise it is cleared and \
             `footprints_unresolved` plus a completeness gap names the follow-up. Success reports \
             extractor-verified `connectivity` and `unconnected` pins.",
            json!({
                "type": "object",
                "properties": {
                    "ref": { "type": "string" },
                    "lib_id": { "type": "string" },
                    "value": { "type": "string" },
                    "footprint": { "type": "string" },
                    "pin_map": { "type": "object", "additionalProperties": { "type": "string" } }
                },
                "required": ["ref", "lib_id"],
                "additionalProperties": false
            }),
        ),
        (
            "connect",
            "Join two ends — a pin like \"R1.1\" / \"U1.VDD\", a net name, or a point [x,y] — or every pair in \
             `pairs` at once. `from` with `net` and no `to` puts that one pin on the named net: a power \
             rail (GND, +3V3, VBUS...) gets a rail symbol on the pin (or a PWR_FLAG when it is already \
             on that rail), any other net gets a label (`kind` local/global/hierarchical, default by \
             scope). Every net carried by the two named endpoints is stated intent, including two \
             authored nets; a one-pin `net` still refuses replacing a different authored name and \
             returns a `delete_wires` fix. For series insertion, delete the old wire then join both \
             sides in one `pairs` call. The route is solved around the existing drawing and \
             junctions are added for you; if nothing fits, both ends are named with `net` instead \
             and the result says so. Never draw wires by coordinate.",
            json!({
                "type": "object",
                "properties": {
                    "from": { "description": PIN },
                    "to": { "description": PIN },
                    "net": { "type": "string", "description": "Name for the resulting net." },
                    "kind": { "type": "string", "enum": ["local", "global", "hierarchical"],
                              "description": "Label kind for a one-pin `net`; default by the net's scope." },
                    "lib_id": { "type": "string", "description": "Rail symbol to draw for a one-pin power `net`, when the default is not wanted." },
                    "pairs": {
                        "type": "array", "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": {
                                "from": { "description": PIN },
                                "to": { "description": PIN },
                                "net": { "type": "string" }
                            },
                            "required": ["from"],
                            "additionalProperties": false
                        }
                    }
                },
                "additionalProperties": false
            }),
        ),
        (
            "no_connect",
            "Mark one or many pins deliberately unconnected, so ERC stops reporting them. A single-pin labelled net is retracted automatically; a net shared with another pin is refused.",
            json!({
                "type": "object",
                "properties": {
                    "pin": { "type": "string", "description": PIN },
                    "pins": { "type": "array", "items": { "type": "string", "description": PIN }, "minItems": 1 }
                },
                "anyOf": [{"required": ["pin"]}, {"required": ["pins"]}],
                "additionalProperties": false
            }),
        ),
        (
            "delete_wires",
            "Remove wires by pin, net, touching part, rectangle, or uuid; reports loose pins. For a part IN \
             SERIES cut ONE pin — {pins:[\"RX.1\"]} — then connect through it. RX.1 is a \
             placeholder. `net` cuts the whole net and loosens every pin. With `names` (label \
             texts) or `labels: true`, the call removes LABELS instead of wires — the ones `names`, \
             `net`, `bbox` or `uuids` match; a removal that would merge two nets is refused.",
            json!({
                "type": "object",
                "properties": {
                    "pins": { "type": "array", "items": { "type": "string" } },
                    "net": { "type": "string" },
                    "refs": { "type": "array", "items": { "type": "string" } },
                    "uuids": { "type": "array", "items": { "type": "string" } },
                    "bbox": {
                        "type": "array", "items": {"type": "number"},
                        "minItems": 4, "maxItems": 4,
                        "description": "Delete wires entering [x1,y1,x2,y2] in mm."
                    },
                    "names": { "type": "array", "items": { "type": "string" },
                               "description": "Label texts to remove." },
                    "labels": { "type": "boolean", "description": "Remove the labels `net`, `bbox` or `uuids` match, instead of wires." }
                },
                "additionalProperties": false
            }),
        ),
    ];
    defs.into_iter()
        .map(|(name, description, schema)| {
            Tool::new(name)
                .with_description(description)
                .with_schema(schema)
        })
        .collect()
}

/// Dispatch one of this crate's tools. `None` when the name is not ours.
pub fn run(name: &str, input: Value, ctx: &AgentRuntime) -> Option<Result<Value>> {
    // `place_parts` is the way a schematic comes into existence; everything else
    // needs one to already be there.
    if !ctx.sch_path().is_file()
        && !matches!(name, "place_parts" | "search_footprints")
    {
        return handles(name).then(|| {
            Ok(json!({
                "error": format!(
                    "no schematic at {} yet — create one before editing it",
                    ctx.sch_path().display()
                ),
            }))
        });
    }
    Some(match name {
        "place_parts" => bulk::place_parts(input, ctx),
        "arrange" => bulk::arrange(input, ctx),
        "create_and_update_block" => blocks::create_and_update_block(input, ctx),
        "arrange_blocks" => blocks::arrange_blocks(input, ctx),
        "read_schematic" => query::read_schematic(input, ctx),
        "search_footprints" => query::search_footprints(input, ctx),
        "check_schematic" => check::check_schematic(input, ctx),
        "render_schematic" => render::render_schematic(ctx),
        // The critic is the model itself, so the agent loop serves this one; the
        // name lives here so `handles` and `tool_defs` agree on the surface.
        "review_schematic" => Ok(json!({
            "error": "review_schematic is served by the agent loop, not by the tool registry",
        })),
        "remove_symbols" => edit::remove_symbols(input, ctx),
        "set_fields" => edit::set_fields(input, ctx),
        "swap_symbol" => edit::swap_symbol(input, ctx),
        "connect" => wiring::connect_tool(input, ctx),
        "no_connect" => wiring::no_connect(input, ctx),
        "delete_wires" => wiring::delete_wires(input, ctx),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_advertised_tool_is_one_this_crate_owns() {
        for tool in tool_defs() {
            let name = tool.name.to_string();
            assert!(handles(&name), "`{name}` is advertised but not handled");
        }
    }
}
