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

mod bulk;
mod check;
mod edit;
mod place;
mod query;
mod refs;
mod session;
mod wiring;

use anyhow::Result;
use gordian_llm::Tool;
use gordian_runtime::AgentRuntime;
use serde_json::{Value, json};

/// The wall-clock promise `place_parts` / `arrange` keep — the agent loop sizes its
/// own timeout from it.
pub use sch_floorplan::live::PlacementBudget;

/// Hash the live schematic bytes, distinguishing a missing file from an empty one.
pub fn schematic_content_hash(ctx: &AgentRuntime) -> Result<Option<u64>> {
    match std::fs::read(ctx.sch_path()) {
        Ok(bytes) => Ok(Some(geom::fnv1a(&bytes))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// The tools that write the schematic.
pub const MUTATORS: [&str; 15] = [
    "place_parts",
    "arrange",
    "rewire",
    "add_symbols",
    "remove_symbols",
    "move_symbols",
    "set_fields",
    "assign_footprints",
    "set_flags",
    "swap_symbol",
    "connect",
    "label",
    "no_connect",
    "add_power",
    "delete_wires",
];

/// Whether `name` is one of this crate's tools.
pub fn handles(name: &str) -> bool {
    tool_names().contains(&name)
}

fn tool_names() -> Vec<&'static str> {
    let mut names = vec!["read_schematic", "get_symbol", "get_net", "check_schematic"];
    names.extend(MUTATORS);
    names
}

const PIN: &str = "A pin as \"<ref>.<number>\" or \"<ref>.<name>\", e.g. \"U1.VDD\".";

/// The JSON-Schema tool definitions handed to the model.
pub fn tool_defs() -> Vec<Tool> {
    let side = json!({ "type": "string", "enum": ["left", "right", "above", "below"] });
    let point =
        json!({ "type": "array", "items": { "type": "number" }, "minItems": 2, "maxItems": 2 });
    let part = json!({
        "lib_id": { "type": "string", "description": "KiCAD Lib:Name, e.g. Device:R." },
        "ref": { "type": "string" },
        "value": { "type": "string" },
        "footprint": { "type": "string" },
        "near": { "type": "string", "description": "Reference to sit beside." },
        "side": side.clone(),
        "rot": { "type": "number", "enum": [0, 90, 180, 270] }
    });
    let defs: Vec<(&str, &str, Value)> = vec![
        (
            "place_parts",
            "The ONLY way to create a new design or add a multi-part block. Submit the COMPLETE electrically finished block in one call, including every requested support, protection, decoupling, bias, termination, indicator, and connector part. State connectivity only: real KiCAD parts and pin-to-net mappings, never coordinates or wires. Before placement, the complete payload is validated and writes nothing on failure: explicit refs must be unused, and every new named signal pin must land on a net with at least one other pin across the payload and existing sheet; power rails, declared ports, and `nc` are terminal nets. Omit `ref` to auto-assign the lowest unused designator from the library symbol. One valid call lays out the whole new sheet, or places the block as a region while freezing existing symbols. The result's `gaps` are deterministic missing-support findings; for a complete powered/interface design, add the listed parts in one coherent follow-up place_parts call. They are advisory for deliberately minimal designs and focused edits. Use `intent.relations` for relative placement: kinds `left_of`/`right_of`/`above`/`below` {a, b}, `group` {name, members, side?: [left|right|top|bottom, anchor]}, `align` {members, axis}. If rejected, correct every reported diagnostic before retrying; unknown-pin errors list valid physical pins.",
            sch_check::place_parts_input_schema(),
        ),
        (
            "arrange",
            "Re-place selected symbols and redraw only their wiring while every unselected symbol stays frozen. Select by refs or bbox; the placement engine owns all coordinates.",
            bulk::selection_schema(true),
        ),
        (
            "rewire",
            "Redraw selected symbols' wiring in place without moving any symbol. Select by refs or bbox; wires are solver-generated, never coordinate-authored.",
            bulk::selection_schema(false),
        ),
        (
            "read_schematic",
            "Read the live schematic as aligned plain text grouped into sorted parts and units, summarized power symbols, nets, loose pins, and warnings.",
            json!({
                "type": "object",
                "properties": {
                    "region": { "type": "array", "items": { "type": "number" }, "minItems": 4, "maxItems": 4,
                                "description": "Only symbols inside [x1,y1,x2,y2] mm." },
                    "detail": { "type": "string", "enum": ["compact", "full"],
                                "description": "full adds footprints and uuids." }
                },
                "additionalProperties": false
            }),
        ),
        (
            "get_symbol",
            "Read one part as aligned plain text with fields, flags, per-unit pose and body size, and pin geometry and nets.",
            json!({
                "type": "object",
                "properties": { "ref": { "type": "string" } },
                "required": ["ref"],
                "additionalProperties": false
            }),
        ),
        (
            "get_net",
            "The pins on a net and where it is named.",
            json!({
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"],
                "additionalProperties": false
            }),
        ),
        (
            "check_schematic",
            "Lint + electrical rules + KiCAD ERC over the live file. `completeness.gaps` lists advisory missing support circuitry; resolve it when the request implies a complete powered/interface design, but never expand a deliberately minimal or focused edit.",
            json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        ),
        (
            "add_symbols",
            "Place one or many parts at collision-free grid positions and report each final spot. \
             `near`+`side` puts a series part by its upstream part and faces it; keep the reported \
             spot. `rot` overrides; `ref` is optional. All-or-nothing; wire with `connect`.",
            json!({
                "type": "object",
                "properties": {
                    "parts": {
                        "type": "array", "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": part.clone(),
                            "required": ["lib_id"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["parts"],
                "additionalProperties": false
            }),
        ),
        (
            "remove_symbols",
            "Delete parts, plus the wire stubs and labels that only served their pins; reports what \
             it retracted.",
            json!({
                "type": "object",
                "properties": { "refs": { "type": "array", "items": { "type": "string" }, "minItems": 1 } },
                "required": ["refs"],
                "additionalProperties": false
            }),
        ),
        (
            "move_symbols",
            "Move or turn parts, wires and rails following along; `rot`/`mirror` alone reverses a \
             diode in place. A taken spot is slid to final `nudged_to`; success is \
             collision-free and needs no follow-up. Refused if no nearby spot fits or a net changes.",
            json!({
                "type": "object",
                "properties": {
                    "moves": {
                        "type": "array", "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": {
                                "ref": { "type": "string" },
                                "unit": { "type": "integer", "minimum": 1 },
                                "rot": { "type": "number", "enum": [0, 90, 180, 270] },
                                "mirror": { "type": "string", "enum": ["none", "x", "y"] },
                                "to": point.clone(),
                                "by": point.clone(),
                                "near": { "type": "string" },
                                "side": side.clone()
                            },
                            "required": ["ref"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["moves"],
                "additionalProperties": false
            }),
        ),
        (
            "set_fields",
            "Set properties on one part (Value, Footprint, Reference, user fields), on every unit \
             of it — how a resistor's value or footprint changes. Moves nothing.",
            json!({
                "type": "object",
                "properties": {
                    "ref": { "type": "string" },
                    "fields": { "type": "object", "minProperties": 1,
                                "additionalProperties": { "type": ["string", "null"] } }
                },
                "required": ["ref", "fields"],
                "additionalProperties": false
            }),
        ),
        (
            "assign_footprints",
            "Set footprint fields directly on one or more live schematic parts. Use search_footprints first; the complete batch is validated and written atomically.",
            json!({
                "type": "object",
                "properties": {
                    "assignments": {
                        "type": "array",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": {
                                "reference": { "type": "string" },
                                "footprint": { "type": "string", "description": "KiCAD Lib:Name." }
                            },
                            "required": ["reference", "footprint"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["assignments"],
                "additionalProperties": false
            }),
        ),
        (
            "set_flags",
            "Set a part's do-not-populate / in-BOM attributes.",
            json!({
                "type": "object",
                "properties": {
                    "ref": { "type": "string" },
                    "dnp": { "type": "boolean" },
                    "in_bom": { "type": "boolean" }
                },
                "required": ["ref"],
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
             instead — a same-named part in an unrelated library is not proven pin-compatible.",
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
            "Join two ends — a pin like \"R1.1\" / \"U1.VDD\", or a point [x,y] — or every pair in \
             `pairs` at once. For series insertion, delete the old wire then join both sides in one \
             `pairs` call. The route is solved around the existing \
             drawing and junctions are added for you; if nothing fits, both ends are named with \
             `net` instead and the result says so. Never draw wires by coordinate.",
            json!({
                "type": "object",
                "properties": {
                    "from": { "description": PIN },
                    "to": { "description": PIN },
                    "net": { "type": "string", "description": "Name for the resulting net." },
                    "pairs": {
                        "type": "array", "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": {
                                "from": { "description": PIN },
                                "to": { "description": PIN },
                                "net": { "type": "string" }
                            },
                            "required": ["from", "to"],
                            "additionalProperties": false
                        }
                    }
                },
                "additionalProperties": false
            }),
        ),
        (
            "label",
            "Name the net at one pin. Two pins carrying the same local label are connected.",
            json!({
                "type": "object",
                "properties": {
                    "pin": { "type": "string", "description": PIN },
                    "net": { "type": "string" },
                    "kind": { "type": "string", "enum": ["local", "global", "hierarchical"] }
                },
                "required": ["pin", "net"],
                "additionalProperties": false
            }),
        ),
        (
            "no_connect",
            "Mark a pin deliberately unconnected, so ERC stops reporting it.",
            json!({
                "type": "object",
                "properties": { "pin": { "type": "string", "description": PIN } },
                "required": ["pin"],
                "additionalProperties": false
            }),
        ),
        (
            "add_power",
            "Drop a power/ground symbol straight onto a pin, which puts that pin on the rail.",
            json!({
                "type": "object",
                "properties": {
                    "net": { "type": "string", "description": "Rail name, e.g. GND, +3V3." },
                    "pin": { "type": "string", "description": PIN },
                    "lib_id": { "type": "string", "description": "Override the power symbol choice." }
                },
                "required": ["net", "pin"],
                "additionalProperties": false
            }),
        ),
        (
            "delete_wires",
            "Remove wires by pin, net, touching part, or uuid; reports loose pins. For a part IN \
             SERIES cut ONE pin — {pins:[\"RX.1\"]} — then connect through it. RX.1 is a \
             placeholder. `net` cuts the whole net and loosens every pin.",
            json!({
                "type": "object",
                "properties": {
                    "pins": { "type": "array", "items": { "type": "string" } },
                    "net": { "type": "string" },
                    "refs": { "type": "array", "items": { "type": "string" } },
                    "uuids": { "type": "array", "items": { "type": "string" } }
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
    if !ctx.sch_path().is_file() && name != "place_parts" {
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
        "rewire" => bulk::rewire(input, ctx),
        "read_schematic" => query::read_schematic(input, ctx),
        "get_symbol" => query::get_symbol(input, ctx),
        "get_net" => query::get_net(input, ctx),
        "check_schematic" => check::check_schematic(input, ctx),
        "add_symbols" => edit::add_symbols(input, ctx),
        "remove_symbols" => edit::remove_symbols(input, ctx),
        "move_symbols" => edit::move_symbols(input, ctx),
        "set_fields" => edit::set_fields(input, ctx),
        "assign_footprints" => edit::assign_footprints(input, ctx),
        "set_flags" => edit::set_flags(input, ctx),
        "swap_symbol" => edit::swap_symbol(input, ctx),
        "connect" => wiring::connect_tool(input, ctx),
        "label" => wiring::label_tool(input, ctx),
        "no_connect" => wiring::no_connect(input, ctx),
        "add_power" => wiring::add_power(input, ctx),
        "delete_wires" => wiring::delete_wires(input, ctx),
        _ => return None,
    })
}
