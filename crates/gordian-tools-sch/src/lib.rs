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

/// The tools that write the schematic. The turn loop approves these before
/// they run — they mutate the project and have no dry-run.
pub const MUTATORS: [&str; 11] = [
    "add_symbol",
    "remove_symbols",
    "move_symbols",
    "set_fields",
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
    let mut names = vec![
        "read_schematic",
        "get_symbol",
        "get_net",
        "free_space",
        "check_schematic",
        "undo",
    ];
    names.extend(MUTATORS);
    names
}

const PIN: &str = "A pin as \"<ref>.<number>\" or \"<ref>.<name>\", e.g. \"U1.VDD\".";

/// The JSON-Schema tool definitions handed to the model.
pub fn tool_defs() -> Vec<Tool> {
    let side = json!({ "type": "string", "enum": ["left", "right", "above", "below"] });
    let point = json!({ "type": "array", "items": { "type": "number" }, "minItems": 2, "maxItems": 2 });
    let defs: Vec<(&str, &str, Value)> = vec![
        (
            "read_schematic",
            "Read the live schematic: one line per symbol with its position and pin→net map, \
             then the nets, loose pins and warnings. Call this first on any existing board.",
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
            "One part in full: position, fields, body extents, and every pin with its side and net.",
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
            "free_space",
            "A free [x,y] with room for a w×h block, optionally near a part.",
            json!({
                "type": "object",
                "properties": {
                    "w": { "type": "number" }, "h": { "type": "number" },
                    "near": { "type": "string", "description": "Reference to stay close to." }
                },
                "required": ["w", "h"],
                "additionalProperties": false
            }),
        ),
        (
            "check_schematic",
            "Lint + electrical rules + KiCAD ERC over the live file. Run this before you finish.",
            json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        ),
        (
            "add_symbol",
            "Place a part. Position it with `near`+`side` (preferred) or `at`; with neither it \
             lands in free space. `ref` is auto-assigned when omitted. The part starts unconnected \
             — wire it with `connect`.",
            json!({
                "type": "object",
                "properties": {
                    "lib_id": { "type": "string", "description": "KiCAD Lib:Name, e.g. Device:R." },
                    "ref": { "type": "string" },
                    "value": { "type": "string" },
                    "footprint": { "type": "string" },
                    "near": { "type": "string", "description": "Reference to sit beside." },
                    "side": side.clone(),
                    "at": point.clone()
                },
                "required": ["lib_id"],
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
            "Move parts. Refused if a part would overlap another or if the move changed any net.",
            json!({
                "type": "object",
                "properties": {
                    "moves": {
                        "type": "array", "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": {
                                "ref": { "type": "string" },
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
            "Set properties on one part (Value, Footprint, Reference, user fields). This is how you \
             change a resistor's value or footprint — it moves nothing.",
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
            "Retarget a part at a different library symbol, keeping every pin's net by number then \
             by name. Use `pin_map` {old_pin: new_pin} when the pinout differs; unmapped pins are \
             reported. For a value or footprint change alone, use set_fields instead.",
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
            "Join two ends. Each end is a pin like \"R1.1\" / \"U1.VDD\", or a point [x,y]. The route \
             is solved around the existing drawing and junctions are added for you; if nothing fits, \
             both ends are named with `net` instead and the result says so. NEVER draw wires by \
             coordinate — this is the only way to connect.",
            json!({
                "type": "object",
                "properties": {
                    "from": { "description": PIN },
                    "to": { "description": PIN },
                    "net": { "type": "string", "description": "Name for the resulting net." }
                },
                "required": ["from", "to"],
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
            "Remove drawn wires by net, by the parts they touch, or by uuid.",
            json!({
                "type": "object",
                "properties": {
                    "net": { "type": "string" },
                    "refs": { "type": "array", "items": { "type": "string" } },
                    "uuids": { "type": "array", "items": { "type": "string" } }
                },
                "additionalProperties": false
            }),
        ),
        (
            "undo",
            "Restore the schematic to the `snapshot` id a previous mutator returned.",
            json!({
                "type": "object",
                "properties": { "snapshot": { "type": "string" } },
                "required": ["snapshot"],
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
    if !ctx.sch_path().is_file() {
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
        "read_schematic" => query::read_schematic(input, ctx),
        "get_symbol" => query::get_symbol(input, ctx),
        "get_net" => query::get_net(input, ctx),
        "free_space" => query::free_space(input, ctx),
        "check_schematic" => check::check_schematic(input, ctx),
        "add_symbol" => edit::add_symbol(input, ctx),
        "remove_symbols" => edit::remove_symbols(input, ctx),
        "move_symbols" => edit::move_symbols(input, ctx),
        "set_fields" => edit::set_fields(input, ctx),
        "set_flags" => edit::set_flags(input, ctx),
        "swap_symbol" => edit::swap_symbol(input, ctx),
        "connect" => wiring::connect_tool(input, ctx),
        "label" => wiring::label_tool(input, ctx),
        "no_connect" => wiring::no_connect(input, ctx),
        "add_power" => wiring::add_power(input, ctx),
        "delete_wires" => wiring::delete_wires(input, ctx),
        "undo" => session::undo(input, ctx),
        _ => return None,
    })
}
