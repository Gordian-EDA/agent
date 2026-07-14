//! The schematic + PCB tool registry the agent drives.
//!
//! Each tool is a thin, deterministic wrapper over logic that already lives in
//! `circuit-lang`, `kicad-footprint`/`kicad-cli`, and `sch-floorplan`/`sch-io`. The registry
//! exposes two free functions, both driven directly by the [`crate::Agent`] loop:
//!
//! - [`tool_defs`] — the JSON-Schema genai [`Tool`]s handed to the LLM.
//! - [`run_tool`] — dispatch a tool by name with a JSON input, returning the
//!   result the model reads back. Most results are structured JSON for
//!   **self-repair**: failures carry
//!   diagnostic strings and "did you mean" suggestions rather than just an error
//!   flag, so the model can correct itself on the next turn.
//!
//! The schematic side covers `search_symbols` / `get_symbol_info`
//! / `validate_design` / `apply_design` / `review_design` / `run_erc` /
//! `project_info` / `read_schematic` / `render_schematic` / `create_design` /
//! `edit_design`; the PCB side (in [`crate::tools_pcb`]) covers the footprint
//! search/info, `regenerate_board`, and the place/route/export/interactive flow.
//!
//! ## `apply_design`: preview vs approved write
//!
//! Model-facing `apply_design` has no write flag. The human apply-gate lives in
//! the [`crate::Agent`] loop: it first runs an internal preview to return a diff,
//! then, after approval, re-runs the same tool with a private write switch. Direct
//! dispatcher calls remain preview-only unless that private switch is supplied.
//!
//! ## Symbol-index caching
//!
//! `search_symbols` is backed by [`SymbolIndex`], whose `build` scans every
//! installed `.kicad_sym` (~0.5 s). The index is built **once per `AgentRuntime`**
//! and cached in a [`OnceLock`]; subsequent searches reuse it.
//!
//! ## Threading
//!
//! [`AgentRuntime`] is `Send + Sync` (asserted below) so the agent loop can run
//! tool calls on `spawn_blocking` threads — keeping a single-threaded UI
//! responsive while a tool compiles, renders, or shells out to `kicad-cli`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use circuit_lang::compile;
use circuit_lang::model::{Component, Design, PinTarget};
use kicad_cli::{ErcReport, KicadCli};
use kicad_footprint::FootprintId;
use sch_io::read::lift;

use crate::{AgentRuntime, Tool};

/// The JSON-Schema definitions for every tool, in a stable order. The
/// [`crate::Agent`] loop hands these to the model as genai [`Tool`]s.
pub fn tool_defs() -> Vec<Tool> {
    /// One tool definition, mapped to a genai [`Tool`] below. Mirrors the fields
    /// a [`Tool`] carries (name + description + JSON schema) so the table reads
    /// as plain data.
    struct Def {
        name: String,
        description: String,
        input_schema: Value,
    }
    let defs = vec![
            Def {
                name: "search_symbols".into(),
                description: "Find `Lib:Name`; reuse hits. Built-ins: Device:R/C/LED, power:GND/+3V3, Connector:Conn_01x02_Pin..01x06_Pin."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" },
                        "limit": { "type": "integer", "description": "Max hits (default 5).", "minimum": 1 }
                    },
                    "required": ["query"]
                }),
            },
            Def {
                name: "get_symbol_info".into(),
                description: "Return symbol ratings/datasheet/default footprint and pins."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "lib_id": { "type": "string", "description": "E.g. Device:R." }
                    },
                    "required": ["lib_id"]
                }),
            },
            Def {
                name: "validate_design".into(),
                description: "Recheck YAML/draft; authoring tools already return validation. Omit yaml for draft."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "yaml": { "type": "string" }
                    }
                }),
            },
            Def {
                name: "apply_design".into(),
                description: "Compile/render, approve/write the schematic, and run ERC. Omit yaml for draft."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "yaml": { "type": "string", "description": "Optional; defaults to draft." }
                    }
                }),
            },
            Def {
                name: "review_design".into(),
                description: "Costly electrical review of the complete draft; required once before PCB work. Fix high-confidence defects."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "intent": { "type": "string", "description": "Goal, rails, key parts/interfaces." }
                    }
                }),
            },
            Def {
                name: "run_erc".into(),
                description: "Fresh KiCAD ERC; do not call immediately after a clean apply_design."
                    .into(),
                input_schema: json!({ "type": "object", "properties": {} }),
            },
            Def {
                name: "project_info".into(),
                description: "Return project paths/state."
                    .into(),
                input_schema: json!({ "type": "object", "properties": {} }),
            },
            Def {
                name: "read_schematic".into(),
                description: "Read circuit-YAML text. source='draft' reads/seeds the draft; a .kicad_sch path does not."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "source": { "type": "string", "description": "'draft' (default) or .kicad_sch path." }
                    }
                }),
            },
            Def {
                name: "render_schematic".into(),
                description: "Render current schematic to PNG for visual inspection."
                    .into(),
                input_schema: json!({ "type": "object", "properties": {} }),
            },
            Def {
                name: "create_design".into(),
                description: "Create a draft and return validation; overwrite=true replaces one."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "yaml": { "type": "string" },
                        "overwrite": { "type": "boolean" }
                    },
                    "required": ["yaml"]
                }),
            },
            Def {
                name: "edit_design".into(),
                description: "Full YAML; part loss needs allow_component_removal. Patch to delete."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "yaml": { "type": "string" },
                        "allow_component_removal": { "type": "boolean" },
                        "old_string": { "type": "string" },
                        "new_string": { "type": "string" },
                        "replace_all": { "type": "boolean" }
                    }
                }),
            },
            // ── PCB tools (slice 5) ─────────────────────────────────────────
            Def {
                name: "search_footprints".into(),
                description: "Find real footprint `Lib:Name` ids before apply_design; reuse hits."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" },
                        "limit": { "type": "integer", "description": "Max hits (default 5).", "minimum": 1 }
                    },
                    "required": ["query"]
                }),
            },
            Def {
                name: "get_footprint_info".into(),
                description: "Return footprint pad numbers and compact geometry summary for a `Lib:Name`."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "lib_id": { "type": "string" }
                    },
                    "required": ["lib_id"]
                }),
            },
            Def {
                name: "assign_footprints".into(),
                description: "Batch-set draft footprints; apply_design before regenerate_board."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "assignments": {
                            "type": "array",
                            "description": "Use even for one component.",
                            "minItems": 1,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "reference": { "type": "string" },
                                    "footprint": { "type": "string", "description": "Lib:Name." }
                                },
                                "required": ["reference", "footprint"]
                            }
                        }
                    },
                    "required": ["assignments"]
                }),
            },
            Def {
                name: "open_board".into(),
                description: "Open project PCB for live IPC edits; returns get_board state."
                    .into(),
                input_schema: json!({ "type": "object", "properties": {} }),
            },
            Def {
                name: "move_parts".into(),
                description: "Batch-move live footprints by absolute to, relative by, near, or edge; supports rotation/offsets."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "moves": {
                            "type": "array",
                            "description": "Sequential moves; one movement mode per item.",
                            "minItems": 1,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "reference": { "type": "string" },
                                    "to": {
                                        "type": "array",
                                        "description": "Absolute [x,y] mm.",
                                        "items": { "type": "number" },
                                        "minItems": 2,
                                        "maxItems": 2
                                    },
                                    "by": {
                                        "type": "array",
                                        "description": "Relative [dx,dy] mm.",
                                        "items": { "type": "number" },
                                        "minItems": 2,
                                        "maxItems": 2
                                    },
                                    "near": { "type": "string", "description": "Target refdes." },
                                    "side": { "type": "string", "enum": ["left", "right", "above", "below"] },
                                    "edge": { "type": "string", "enum": ["left", "right", "top", "bottom"] },
                                    "gap": { "type": "number", "description": "near/edge gap, mm." },
                                    "rotation": { "type": "number", "description": "Absolute degrees." },
                                    "horizontal_offset": { "type": "number", "description": "mm; +right." },
                                    "vertical_offset": { "type": "number", "description": "mm; +down." }
                                },
                                "required": ["reference"]
                            }
                        }
                    },
                    "required": ["moves"]
                }),
            },
            Def {
                name: "route_track".into(),
                description: "Route one live connection with obstacle avoidance, layer changes, and optional via anchors."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "from": {
                            "type": "array",
                            "items": {"type":"number"},
                            "minItems": 2,
                            "maxItems": 2,
                            "description": "Start [x,y] mm."
                        },
                        "to": {
                            "type": "array",
                            "items": {"type":"number"},
                            "minItems": 2,
                            "maxItems": 2,
                            "description": "End [x,y] mm."
                        },
                        "net": { "type": "string" },
                        "from_layer": { "type": "string", "description": "F.Cu/B.Cu/In1.Cu/top/bottom; default F.Cu." },
                        "to_layer": { "type": "string", "description": "Same forms; default from_layer." },
                        "width": { "type": "number", "description": "mm; default net width." },
                        "vias": {
                            "type": "array",
                            "description": "Anchors changing current layer to to_layer.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "at": {
                                        "type": "array",
                                        "items": {"type":"number"},
                                        "minItems": 2,
                                        "maxItems": 2
                                    },
                                    "to_layer": { "type": "string" }
                                },
                                "required": ["at", "to_layer"]
                            }
                        }
                    },
                    "required": ["from", "to", "net"]
                }),
            },
            Def {
                name: "delete_copper".into(),
                description: "Delete live track/via copper near a point; optional kind/net/layer filters."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "at": {
                            "type": "array",
                            "items": {"type":"number"},
                            "minItems": 2,
                            "maxItems": 2,
                            "description": "Point [x,y] mm."
                        },
                        "radius": { "type": "number", "description": "mm; default 0.4." },
                        "kinds": {
                            "type": "array",
                            "items": { "type": "string", "enum": ["track", "via"] },
                            "description": "Default both."
                        },
                        "net": { "type": "string" },
                        "layer": { "type": "string" },
                        "all": { "type": "boolean", "description": "All matches; default nearest." }
                    },
                    "required": ["at"]
                }),
            },
            Def {
                name: "set_net_width".into(),
                description: "Set live net-class width/clearance; prefer regenerate_board rules before routing."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "width": { "type": "number", "description": "mm; default 0.5." },
                        "clearance": { "type": "number", "description": "mm; default 0.2." },
                        "nets": { "type": "array", "items": {"type":"string"} }
                    },
                    "required": ["name", "nets"]
                }),
            },
            Def {
                name: "update_board_outline".into(),
                description: "Edit existing Edge.Cuts: rectangle bounds, polygon outline, or fit_to_geometry plus margin."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "bounds": {
                            "type": "object",
                            "description": "Rectangle, mm.",
                            "properties": {
                                "min_x": { "type": "number" }, "max_x": { "type": "number" },
                                "min_y": { "type": "number" }, "max_y": { "type": "number" }
                            }
                        },
                        "outline": {
                            "type": "array",
                            "description": "Closed [[x,y],...] polygon, mm.",
                            "minItems": 3,
                            "items": {
                                "type": "array",
                                "items": { "type": "number" },
                                "minItems": 2,
                                "maxItems": 2
                            }
                        },
                        "fit_to_geometry": {
                            "type": "boolean",
                            "description": "Fit rectangle around parts/copper."
                        },
                        "margin": { "type": "number", "description": "Fit margin, mm; default 2." }
                    }
                }),
            },
            Def {
                name: "regenerate_board".into(),
                description: "Seed PCB destructively from committed schematic.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "bounds": {
                            "type": "object",
                            "description": "Outline rectangle, mm.",
                            "properties": {
                                "min_x": { "type": "number" }, "max_x": { "type": "number" },
                                "min_y": { "type": "number" }, "max_y": { "type": "number" }
                            }
                        },
                        "rules": {
                            "type": "object",
                            "description": "Copper rules (mm). Pours: top/bottom/innerN; 6+ layers default GND/V3V3.",
                            "properties": {
                                "layer_count": { "type": "integer", "enum": [2, 4, 6, 8] },
                                "clearance": { "type": "number" },
                                "min_trace_width": { "type": "number" },
                                "via_diameter": { "type": "number" },
                                "via_drill": { "type": "number" },
                                "net_widths": {
                                    "type": "object",
                                    "additionalProperties": { "type": "number" }
                                },
                                "pours": {
                                    "type": "array",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "net": { "type": "string" },
                                            "layer": { "type": "string", "description": "top, bottom, or innerN" },
                                            "connect": { "type": "string", "enum": ["thermal", "solid"], "description": "Pad attachment; default thermal." }
                                        },
                                        "required": ["net", "layer"]
                                    }
                                }
                            }
                        }
                    }
                }),
            },
            Def {
                name: "get_board".into(),
                description: "Return board state. A net filter adds pad centers; include_copper adds copper."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "include_copper": { "type": "boolean" },
                        "kinds": {
                            "type": "array",
                            "items": { "type": "string", "enum": ["track", "via"] },
                            "description": "Default both."
                        },
                        "net": { "type": "string" },
                        "layer": { "type": "string" }
                    }
                }),
            },
            Def {
                name: "place_board".into(),
                description: "Auto-place regenerated board; run after regenerate_board."
                    .into(),
                input_schema: json!({ "type": "object", "properties": {} }),
            },
            Def {
                name: "route_board".into(),
                description: "Auto-route placed board; returns failed nets/metrics."
                    .into(),
                input_schema: json!({ "type": "object", "properties": {} }),
            },
            Def {
                name: "render_board".into(),
                description: "Render board PNG for visual inspection."
                    .into(),
                input_schema: json!({ "type": "object", "properties": {} }),
            },
            Def {
                name: "check_board".into(),
                description: "Run PCB DRC. If ok=true, stop; do not reroute unchanged."
                    .into(),
                input_schema: json!({ "type": "object", "properties": {} }),
            },
            Def {
                name: "export_fab".into(),
                description: "Export Gerbers/drill/position/BOM after check_board passes."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Default project PCB." },
                        "out_dir": { "type": "string", "description": "Default fab/." }
                    }
                }),
            },
    ];
    defs.into_iter()
        .map(|d| {
            Tool::new(d.name)
                .with_description(d.description)
                .with_schema(d.input_schema)
        })
        .collect()
}

/// Dispatch a tool by name (synchronous). `input` is the model-supplied JSON
/// arguments; the returned `Value` is fed back to the model. The [`crate::Agent`]
/// loop off-loads this onto the blocking pool.
pub fn run_tool(name: &str, input: Value, ctx: &AgentRuntime) -> Result<Value> {
    match name {
        "search_symbols" => search_symbols(input, ctx),
        "get_symbol_info" => get_symbol_info(input, ctx),
        "validate_design" => validate_design(input, ctx),
        "apply_design" => apply_design(input, ctx),
        "run_erc" => run_erc(ctx),
        "project_info" => project_info(ctx),
        "read_schematic" => read_schematic(input, ctx),
        "render_schematic" => render_schematic(ctx),
        "create_design" => create_design(input, ctx),
        "edit_design" => edit_design(input, ctx),
        "search_footprints" => crate::tools_pcb::search_footprints(input, ctx),
        "get_footprint_info" => crate::tools_pcb::get_footprint_info(input, ctx),
        "regenerate_board" => crate::tools_pcb::regenerate_board(input, ctx),
        "assign_footprints" => crate::tools_pcb::assign_footprints(input, ctx),
        "get_board" => crate::tools_pcb::get_board(input, ctx),
        "place_board" => crate::tools_pcb::place_board(input, ctx),
        "route_board" => crate::tools_pcb::route_board(input, ctx),
        "check_board" => crate::tools_pcb::check_board(input, ctx),
        "export_fab" => crate::tools_pcb::export_fab(input, ctx),
        "open_board" => crate::tools_pcb::open_board(input, ctx),
        "move_parts" => crate::tools_pcb::move_parts(input, ctx),
        "route_track" => crate::tools_pcb::route_track(input, ctx),
        "delete_copper" => crate::tools_pcb::delete_copper(input, ctx),
        "set_net_width" => crate::tools_pcb::set_net_width(input, ctx),
        "update_board_outline" => crate::tools_pcb::update_board_outline(input, ctx),
        "render_board" => crate::tools_pcb::render_board(input, ctx),
        other => bail!("unknown tool: {other}"),
    }
}

/// Pull a required string field out of the input, with a clear error.
pub(crate) fn require_str(input: &Value, key: &str) -> Result<String> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("missing required string field `{key}`"))
}

// ── 1. search_symbols ──────────────────────────────────────────────────────

fn search_symbols(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let query = require_str(&input, "query")?;
    let limit = input
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(ctx.config().tools.default_search_limit);
    if let Some((lib_id, pin_count)) = common_connector_symbol_alias(&query) {
        return Ok(json!({
            "hits": [{ "lib_id": lib_id, "pin_count": pin_count }],
            "note": "Connector symbols and physical footprints are separate choices. Use this symbol for the schematic pins; use search_footprints for the physical connector footprint.",
        }));
    }
    if let Some((lib_id, pin_count)) = builtin_symbol_alias(&query) {
        return Ok(json!({
            "hits": [{ "lib_id": lib_id, "pin_count": pin_count }],
            "note": "built-in alias/canonical symbol; use it directly and do not repeat this search",
        }));
    }

    let hits: Vec<Value> = ctx
        .index()?
        .search(&query, limit)
        .into_iter()
        .map(|h| json!({ "lib_id": h.lib_id, "pin_count": h.pin_count }))
        .collect();

    Ok(json!({ "hits": hits }))
}

fn builtin_symbol_alias(query: &str) -> Option<(&'static str, usize)> {
    let q = query.trim().to_ascii_lowercase();
    match q.as_str() {
        "r" | "device:r" => Some(("Device:R", 2)),
        "c" | "device:c" => Some(("Device:C", 2)),
        "l" | "device:l" => Some(("Device:L", 2)),
        "d" | "device:d" => Some(("Device:D", 2)),
        "led" | "device:led" => Some(("Device:LED", 2)),
        "gnd" | "power:gnd" => Some(("power:GND", 1)),
        "vcc" | "power:vcc" => Some(("power:VCC", 1)),
        _ => None,
    }
}

/// Map the small, ubiquitous single-row connector family without asking the
/// symbol index to rank a broad `connector` search. `PinHeader_*` is accepted
/// here because models often carry a physical footprint name into symbol
/// discovery; the result deliberately remains a footprint-agnostic symbol.
fn common_connector_symbol_alias(query: &str) -> Option<(&'static str, usize)> {
    let q = query.trim().to_ascii_lowercase();
    let compact: String = q.chars().filter(char::is_ascii_alphanumeric).collect();
    let connectorish =
        compact.contains("connector") || compact.contains("header") || compact.starts_with("conn");

    let pin_count = (2..=6).find(|pin_count| {
        let padded = format!("{pin_count:02}");
        let plain = pin_count.to_string();
        let dimensions = [
            format!("1x{padded}"),
            format!("01x{padded}"),
            format!("1x{plain}"),
            format!("01x{plain}"),
            format!("{padded}x1"),
            format!("{padded}x01"),
            format!("{plain}x1"),
            format!("{plain}x01"),
        ];
        dimensions
            .iter()
            .any(|shape| contains_bounded_number(&compact, shape))
            || (connectorish
                && [format!("{padded}pin"), format!("{plain}pin")]
                    .iter()
                    .any(|pins| contains_bounded_number(&compact, pins)))
    })?;

    match pin_count {
        2 => Some(("Connector:Conn_01x02_Pin", 2)),
        3 => Some(("Connector:Conn_01x03_Pin", 3)),
        4 => Some(("Connector:Conn_01x04_Pin", 4)),
        5 => Some(("Connector:Conn_01x05_Pin", 5)),
        6 => Some(("Connector:Conn_01x06_Pin", 6)),
        _ => None,
    }
}

/// Avoid treating `1x20` as `1x2`, or `16-pin` as `6-pin`.
fn contains_bounded_number(haystack: &str, needle: &str) -> bool {
    haystack.match_indices(needle).any(|(start, matched)| {
        let before_is_digit = haystack[..start]
            .chars()
            .next_back()
            .is_some_and(|ch| ch.is_ascii_digit());
        let end = start + matched.len();
        let after_is_digit = haystack[end..]
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_digit());
        !before_is_digit && !after_is_digit
    })
}

// ── 2. get_symbol_info ─────────────────────────────────────────────────────

fn get_symbol_info(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let lib_id = require_str(&input, "lib_id")?;

    match ctx.provider().symbol(&lib_id) {
        Some(meta) => {
            let pins: Vec<Value> = meta
                .pins
                .iter()
                .map(|p| {
                    json!({
                        "number": p.number,
                        "name": p.name,
                        "type": pin_type_str(p.etype),
                        "unit": p.unit,
                    })
                })
                .collect();
            Ok(json!({
                "lib_id": lib_id,
                "description": meta.description,
                "keywords": meta.keywords,
                "datasheet": meta.datasheet,
                "default_footprint": meta.footprint,
                "pins": pins,
            }))
        }
        None => {
            let suggestions = ctx.provider().suggest(&lib_id);
            Ok(json!({
                "error": format!("unknown symbol `{lib_id}`"),
                "suggestions": suggestions,
            }))
        }
    }
}

/// Render a [`circuit_lang::PinType`] as a stable lowercase string for the LLM.
fn pin_type_str(t: circuit_lang::PinType) -> &'static str {
    use circuit_lang::PinType::*;
    match t {
        PowerInput => "power_input",
        PowerOutput => "power_output",
        Passive => "passive",
        Other => "other",
    }
}

// ── 3. read_schematic helpers ──────────────────────────────────────────────

pub(crate) fn current_sch_text(ctx: &AgentRuntime) -> Option<String> {
    std::fs::read_to_string(ctx.sch_path()).ok()
}

struct DraftRead {
    yaml: String,
    stale: bool,
    note: Option<&'static str>,
}

fn read_draft_or_seed(ctx: &AgentRuntime) -> Result<DraftRead> {
    if let Some(draft) = ctx.workspace().read_draft()? {
        let stale = ctx
            .workspace()
            .draft_is_stale(current_sch_text(ctx).as_deref());
        return Ok(DraftRead {
            yaml: draft,
            stale,
            note: stale.then_some(
                "the .kicad_sch changed since this draft was seeded (user edit \
                 in KiCAD?) — call read_schematic on the project schematic to \
                 see the current state, then reconcile your draft deliberately",
            ),
        });
    }
    if !ctx.sch_path().exists() {
        return Ok(DraftRead {
            yaml: String::new(),
            stale: false,
            note: Some("no schematic yet"),
        });
    }
    let yaml = lift(ctx.env(), ctx.sch_path())
        .with_context(|| format!("lifting {}", ctx.sch_path().display()))?;
    // Seed the draft so edit_design is immediately usable.
    ctx.workspace()
        .write_draft(&yaml, current_sch_text(ctx).as_deref())?;
    Ok(DraftRead {
        yaml,
        stale: false,
        note: Some("draft seeded from the schematic; use edit_design for changes"),
    })
}

pub(crate) fn current_design_yaml(ctx: &AgentRuntime) -> Result<String> {
    Ok(read_draft_or_seed(ctx)?.yaml)
}

// ── 4. validate_design ─────────────────────────────────────────────────────

fn validate_design(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let yaml = if input.get("yaml").is_some() {
        require_str(&input, "yaml")?
    } else {
        let Some(draft) = ctx.workspace().read_draft()? else {
            return Ok(json!({
                "error": "no yaml given and no draft exists — pass yaml, or create a draft with create_design/read_schematic first",
            }));
        };
        draft
    };
    let result = compile(&yaml, ctx.provider());
    compile_authoring_report(&result, ctx)
}

/// Build the `{ok, diagnostics, errors, warnings}` report a compile yields.
pub(crate) fn compile_report(diags: &circuit_lang::Diagnostics) -> Value {
    use circuit_lang::Severity;
    const MAX_DIAGNOSTICS: usize = 40;
    const MAX_WARNINGS_WHEN_ERROR_FREE: usize = 20;

    let mut strings = Vec::new();
    let mut omitted = 0usize;
    for d in &diags.0 {
        let is_warning = d.severity == Severity::Warning;
        let cap = if is_warning {
            MAX_WARNINGS_WHEN_ERROR_FREE
        } else {
            MAX_DIAGNOSTICS
        };
        if strings.len() < cap || d.severity == Severity::Error {
            strings.push(d.to_string());
        } else {
            omitted += 1;
        }
    }
    let errors = diags
        .0
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .count();
    let warnings = diags
        .0
        .iter()
        .filter(|d| d.severity == Severity::Warning)
        .count();
    let mut report = json!({
        "ok": errors == 0,
        "diagnostics": strings,
        "errors": errors,
        "warnings": warnings,
    });
    if omitted > 0 {
        report["diagnostics_omitted"] = json!(omitted);
        report["note"] = json!(
            "diagnostics truncated for context efficiency; fix errors first, then run validate_design again if warning detail is needed"
        );
    }
    report
}

/// Add physical package compatibility to the normal circuit-language report.
/// Keeping this beside `compile_report` makes create/edit/validate/apply expose
/// the same pre-compose contract.
fn compile_authoring_report(
    result: &circuit_lang::CompileResult,
    ctx: &AgentRuntime,
) -> Result<Value> {
    let mut report = compile_report(&result.diagnostics);
    if let Some(design) = &result.design {
        report["design_state"] = design_state_summary(design);
        if add_empty_design_error(&mut report, design) {
            return Ok(report);
        }
        add_footprint_compatibility(&mut report, design, ctx)?;
    }
    Ok(report)
}

/// Compact, deterministic state returned after every successful compile.
///
/// Authoring tools already have the kernel design in hand, so surfacing its
/// identity here saves the model from rereading the full YAML merely to recall
/// what it just created or edited. Names are sorted across blocks and bounded
/// to keep large schematics from turning routine validation into a large tool
/// result; the total counts and omitted counts preserve the complete shape.
fn design_state_summary(design: &Design) -> Value {
    use std::collections::BTreeSet;

    const MAX_NAMES: usize = 32;

    let mut refdes: Vec<&str> = design
        .blocks
        .values()
        .flat_map(|block| block.components.keys().map(String::as_str))
        .collect();
    refdes.sort_unstable();

    // `Design::nets` stores authored net attributes, not necessarily every net
    // named by a component pin. Include both sources so this reflects actual
    // connectivity even when the YAML has no top-level `nets` section.
    let mut nets: BTreeSet<&str> = design.nets.keys().map(String::as_str).collect();
    for component in design
        .blocks
        .values()
        .flat_map(|block| block.components.values())
    {
        nets.extend(component.pins.values().filter_map(|target| match target {
            PinTarget::Net(net) => Some(net.as_str()),
            PinTarget::NoConnect => None,
        }));
        nets.extend(
            component
                .units
                .values()
                .flat_map(|pins| pins.values())
                .filter_map(|target| match target {
                    PinTarget::Net(net) => Some(net.as_str()),
                    PinTarget::NoConnect => None,
                }),
        );
    }
    let mut net_names: Vec<&str> = nets.into_iter().collect();

    let component_count = refdes.len();
    let net_count = net_names.len();
    refdes.truncate(MAX_NAMES);
    net_names.truncate(MAX_NAMES);

    let mut state = json!({
        "component_count": component_count,
        "refdes": refdes,
        "net_count": net_count,
        "net_names": net_names,
    });
    if component_count > MAX_NAMES {
        state["refdes_omitted"] = json!(component_count - MAX_NAMES);
    }
    if net_count > MAX_NAMES {
        state["net_names_omitted"] = json!(net_count - MAX_NAMES);
    }
    state
}

/// A syntactically valid document with no components is not an authored
/// schematic. Treating it as clean lets an early/speculative `apply_design`
/// replace a real project with an empty sheet while still reporting ERC 0/0.
fn add_empty_design_error(report: &mut Value, design: &Design) -> bool {
    if !design_is_empty(design) {
        return false;
    }

    let errors = report.get("errors").and_then(Value::as_u64).unwrap_or(0) + 1;
    report["ok"] = json!(false);
    report["errors"] = json!(errors);
    report["diagnostics"]
        .as_array_mut()
        .expect("compile_report diagnostics must be an array")
        .push(json!(
            "error[empty_design]: the draft has no components; author the complete requested circuit before applying it"
        ));
    report["next_tool"] = json!("edit_design");
    report["next"] = json!(
        "replace the empty draft with the complete circuit; an empty schematic cannot be applied"
    );
    true
}

fn design_is_empty(design: &Design) -> bool {
    design
        .blocks
        .values()
        .all(|block| block.components.is_empty())
}

fn design_component_count(design: &Design) -> usize {
    design
        .blocks
        .values()
        .map(|block| block.components.len())
        .sum()
}

/// Returns `true` when at least one incompatible assignment was found.
fn add_footprint_compatibility(
    report: &mut Value,
    design: &Design,
    ctx: &AgentRuntime,
) -> Result<bool> {
    let catalog = ctx.footprint_catalog()?;
    let mut lookup_errors = Vec::new();
    for block in design.blocks.values() {
        for (reference, component) in &block.components {
            let Some(footprint) = component.footprint.as_deref() else {
                continue;
            };
            let id = match FootprintId::parse(footprint) {
                Ok(id) => id,
                Err(error) => {
                    lookup_errors.push(format!(
                        "error[invalid_footprint]: {reference} uses invalid footprint id `{footprint}`: {error}"
                    ));
                    continue;
                }
            };
            if let Err(error) = catalog.footprint(&id) {
                let detail = if error.is_not_found() {
                    let suggestions = catalog
                        .suggest(&id)
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("unknown footprint `{footprint}`; suggestions: {suggestions}")
                } else {
                    format!("footprint `{footprint}` could not be read: {error}")
                };
                lookup_errors.push(format!(
                    "error[unknown_footprint]: {reference} uses {detail}"
                ));
            }
        }
    }
    let mismatches = crate::footprint_compat::design_pin_mismatches(ctx, design)?;
    if lookup_errors.is_empty() && mismatches.is_empty() {
        return Ok(false);
    }

    let errors = report.get("errors").and_then(Value::as_u64).unwrap_or(0)
        + u64::try_from(lookup_errors.len() + mismatches.len()).unwrap_or(u64::MAX);
    let diagnostics = report["diagnostics"]
        .as_array_mut()
        .expect("compile_report diagnostics must be an array");
    diagnostics.extend(lookup_errors.iter().cloned().map(Value::String));
    diagnostics.extend(mismatches.iter().map(|mismatch| {
        let polarity = mismatch
            .polarity_mismatch
            .as_deref()
            .map(|reason| format!("; polarity mismatch: {reason}"))
            .unwrap_or_default();
        format!(
            "error[footprint_pin_mismatch]: {} uses symbol {} with footprint {}; \
             symbol pins absent from footprint: {:?}; footprint pads absent from symbol: {:?}{}",
            mismatch.reference,
            mismatch.symbol,
            mismatch.footprint,
            mismatch.symbol_pins_absent_from_footprint,
            mismatch.footprint_pads_absent_from_symbol,
            polarity,
        )
        .into()
    }));
    report["ok"] = json!(false);
    report["errors"] = json!(errors);
    report["footprint_pin_mismatches"] = serde_json::to_value(mismatches)?;
    report["next_tool"] = json!("edit_design");
    report["next"] = json!(
        "choose an existing Library:Footprint whose named electrical pad numbers match the symbol pins and whose capacitor polarity matches the symbol, then apply_design; use search_footprints/get_footprint_info instead of guessing names; unnumbered mechanical pads and repeated pads with a valid shared number are allowed; use Device:C_Polarized (pin 1 positive) with polarized CP/C_Elec footprints, and Device:C with ordinary non-polarized capacitor footprints"
    );
    Ok(true)
}

// ── 5. apply_design ────────────────────────────────────────────────────────

fn apply_design(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    if input.get("commit").is_some() {
        return Ok(json!({
            "error": "`commit` has been removed from apply_design; call apply_design({yaml?}) through the approval gate",
        }));
    }
    let explicit_yaml = input
        .get("yaml")
        .and_then(Value::as_str)
        .map(str::to_string);
    let yaml = match explicit_yaml.clone() {
        Some(y) => y,
        None => match ctx.workspace().read_draft()? {
            Some(d) => d,
            None => {
                return Ok(json!({
                    "error": "no yaml given and no draft exists — pass yaml, or \
                              create a draft via read_schematic({source:\"draft\"})/create_design",
                }));
            }
        },
    };
    let stale = explicit_yaml.is_none()
        && ctx
            .workspace()
            .draft_is_stale(current_sch_text(ctx).as_deref());

    let commit = input
        .get("__commit")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // Compile first; never render or write a design with errors.
    let result = compile(&yaml, ctx.provider());
    let mut report = compile_report(&result.diagnostics);
    let Some(design) = result.design else {
        // `ok` is already false here (errors > 0), but be explicit for the LLM.
        report["ok"] = json!(false);
        return Ok(report);
    };
    let design_state = design_state_summary(&design);
    report["design_state"] = design_state.clone();
    if add_empty_design_error(&mut report, &design) {
        return Ok(report);
    }
    if add_footprint_compatibility(&mut report, &design, ctx)? {
        return Ok(report);
    }

    // Prior design (for the diff), lifted from the existing schematic.
    let prior_design = if ctx.sch_path().exists() {
        let prior_yaml = lift(ctx.env(), ctx.sch_path())
            .with_context(|| format!("lifting prior {}", ctx.sch_path().display()))?;
        compile(&prior_yaml, ctx.provider()).design
    } else {
        None
    };

    let diff = design_diff(prior_design.as_ref(), &design);

    let composed =
        crate::multisheet::compose_design(ctx.env(), &design).context("composing schematic")?;

    let detected_idioms = serde_json::to_value(&composed.detected_idioms).ok();

    if !commit {
        return Ok(json!({
            "ok": true,
            "would_write": true,
            "stale_draft_warning": stale,
            "diff": diff,
            "design_state": design_state,
            "layout_mode": "composed",
            "rendered_len": composed.sch.len(),
            "layout_warnings": composed.layout_warnings,
            "wire_through_body": composed.crossings.body + composed.crossings.ic,
            "detected_idioms": detected_idioms.unwrap_or(json!([])),
        }));
    }

    std::fs::write(ctx.sch_path(), &composed.sch)
        .with_context(|| format!("writing {}", ctx.sch_path().display()))?;

    // Record the hash of the just-written schematic (current_sch_text reads the
    // file we wrote above) so the applied draft is no longer flagged stale.
    // No-op when no draft exists (an explicit-yaml apply must not create one).
    // Once the schematic write succeeds, later failures must be returned as an
    // honest `written: true` result. Returning `Err` would make the agent report
    // that nothing committed and potentially retry an already-applied change.
    let draft_sync_error = match ctx.workspace().read_draft() {
        Ok(Some(_)) => ctx
            .workspace()
            .write_draft(&yaml, current_sch_text(ctx).as_deref())
            .err()
            .map(|err| format!("updating applied draft metadata: {err}")),
        Ok(None) => None,
        Err(err) => Some(format!("reading applied draft: {err}")),
    };
    let erc = KicadCli::new(ctx.env()).erc(ctx.sch_path());

    let mut out = json!({
        "ok": true,
        "written": true,
        "path": ctx.sch_path().display().to_string(),
        "stale_draft_warning": stale,
        "diff": diff,
        "design_state": design_state,
        "layout_mode": "composed",
        "layout_warnings": composed.layout_warnings,
        "wire_through_body": composed.crossings.body + composed.crossings.ic,
        "detected_idioms": detected_idioms.unwrap_or(json!([])),
    });
    let mut post_write_errors = Vec::new();
    if let Some(err) = draft_sync_error {
        post_write_errors.push(err);
    }
    match erc {
        Ok(report) => {
            let errors = report.error_count();
            let warnings = report.warning_count();
            out["erc"] = json!({
                "errors": errors,
                "warnings": warnings,
                "violations": erc_violation_values(&report),
            });
            out["erc_checked"] = json!(true);
            if errors == 0 && warnings == 0 {
                out["erc_clean"] = json!(true);
                out["next"] = json!(
                    "ERC already ran and passed; do not call run_erc again unless the schematic changes"
                );
            } else {
                out["erc_clean"] = json!(false);
                out["next_tool"] = json!("edit_design");
                out["next"] =
                    json!("inspect the reported ERC findings, fix the draft, and re-apply");
            }
        }
        Err(err) => {
            let err = format!("running ERC on {}: {err}", ctx.sch_path().display());
            out["erc"] = json!({ "error": err });
            post_write_errors.push(err);
        }
    }
    if !post_write_errors.is_empty() {
        out["ok"] = json!(false);
        out["error"] = json!(format!(
            "schematic was written, but post-write validation failed: {}",
            post_write_errors.join("; ")
        ));
    }
    Ok(out)
}

pub(crate) fn schematic_placement_engine() -> Box<dyn sch_floorplan::contract::PlacementEngine> {
    // The cluster engine is the DEFAULT: it runs the annealer, then a strictly-additive
    // pose+floorplanner pass (and the "modules between rails" idiom on power-IC arrays) that
    // PARETO-DOMINATES it. Full-dataset validation: of 40 liftable boards, 13 de-sprawl (the
    // gate-driver array 0cdac −84%) and ZERO regress on warnings/crossings/sprawl — it can only
    // revert to the anneal result, never ship worse. `SCH_ENGINE=anneal` opts back to the bare SA.
    match std::env::var("SCH_ENGINE").as_deref() {
        Ok("anneal") | Ok("sa") => Box::new(anneal_place::Anneal),
        Ok("spine") => Box::new(spine_place::SpinePlace),
        _ => Box::new(cluster_place::ClusterPlace),
    }
}

/// A per-refdes signature used to detect a *changed* component across a re-apply.
///
/// Two components with the same refdes but a different signature are "changed".
/// The signature folds in the fields that the netlist round-trip can carry —
/// part id, value, footprint, dnp, and the connectivity (component-level and
/// per-unit pin maps) — but deliberately ignores placement, which is not part of
/// the kernel model. Pins/units are gathered into sorted maps so iteration order
/// never spuriously flips the signature.
fn component_signature(c: &Component) -> String {
    use std::collections::BTreeMap;

    fn pin_targets(pins: &indexmap::IndexMap<String, PinTarget>) -> BTreeMap<&str, String> {
        pins.iter()
            .map(|(k, t)| {
                let v = match t {
                    PinTarget::Net(n) => n.clone(),
                    PinTarget::NoConnect => "nc".to_string(),
                };
                (k.as_str(), v)
            })
            .collect()
    }

    let pins = pin_targets(&c.pins);
    let units: BTreeMap<&str, BTreeMap<&str, String>> = c
        .units
        .iter()
        .map(|(u, m)| (u.as_str(), pin_targets(m)))
        .collect();

    format!(
        "{}|{}|{}|{}|{:?}|{:?}",
        c.part,
        c.value.as_deref().unwrap_or(""),
        c.footprint.as_deref().unwrap_or(""),
        c.dnp,
        pins,
        units,
    )
}

/// Structured diff between a prior design (possibly `None` for a fresh project)
/// and the new one: which refdes were added, removed, or changed, plus the net
/// count before/after. Refdes are gathered across all blocks.
fn design_diff(prior: Option<&Design>, new: &Design) -> Value {
    use std::collections::BTreeMap;

    fn components(d: &Design) -> BTreeMap<String, &Component> {
        let mut m = BTreeMap::new();
        for block in d.blocks.values() {
            for (refdes, c) in &block.components {
                m.insert(refdes.clone(), c);
            }
        }
        m
    }

    let new_comps = components(new);
    let prior_comps = prior.map(components).unwrap_or_default();

    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut changed = Vec::new();

    for (refdes, c) in &new_comps {
        match prior_comps.get(refdes) {
            None => added.push(refdes.clone()),
            Some(old) if component_signature(old) != component_signature(c) => {
                changed.push(refdes.clone());
            }
            Some(_) => {}
        }
    }
    for refdes in prior_comps.keys() {
        if !new_comps.contains_key(refdes) {
            removed.push(refdes.clone());
        }
    }

    let nets_before = prior.map(|d| d.nets.len()).unwrap_or(0);
    json!({
        "added": added,
        "removed": removed,
        "changed": changed,
        "nets_before": nets_before,
        "nets_after": new.nets.len(),
    })
}

// ── 6. project_info ────────────────────────────────────────────────────────

fn project_info(ctx: &AgentRuntime) -> Result<Value> {
    Ok(json!({
        "project_dir": ctx.project_dir().display().to_string(),
        "sch_path": ctx.sch_path().display().to_string(),
        "sch_exists": ctx.sch_path().exists(),
        "cwd": std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
    }))
}

// ── 7. read_schematic ──────────────────────────────────────────────────────

fn read_schematic(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let raw = input
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("draft");
    if raw == "draft" {
        let draft = read_draft_or_seed(ctx)?;
        return Ok(Value::String(format_schematic_text(
            "draft",
            None,
            Some(draft.stale),
            draft.note,
            &draft.yaml,
        )));
    }

    let path = resolve_user_path(raw, ctx.project_dir());

    if !path.is_file() {
        return Ok(Value::String(format!(
            "error: no file at `{}`\nnote: the source may be `draft`, absolute, start with ~, or be relative to the project dir",
            path.display()
        )));
    }
    if path.extension().and_then(|e| e.to_str()) != Some("kicad_sch") {
        return Ok(Value::String(format!(
            "error: `{}` is not a .kicad_sch schematic",
            path.display()
        )));
    }

    let yaml = match lift(ctx.env(), &path) {
        Ok(yaml) => yaml,
        Err(e) => {
            return Ok(Value::String(format!(
                "error: could not lift `{}`: {e}",
                path.display()
            )));
        }
    };

    Ok(Value::String(format_schematic_text(
        "path",
        Some(&path),
        None,
        None,
        &yaml,
    )))
}

fn format_schematic_text(
    source: &str,
    path: Option<&Path>,
    stale: Option<bool>,
    note: Option<&str>,
    yaml: &str,
) -> String {
    let mut out = format!("source: {source}\n");
    if let Some(path) = path {
        out.push_str(&format!("path: {}\n", path.display()));
    }
    if let Some(stale) = stale {
        out.push_str(&format!("stale: {stale}\n"));
    }
    if let Some(note) = note {
        out.push_str(&format!("note: {note}\n"));
    }
    out.push_str("\n```yaml\n");
    out.push_str(yaml);
    if !yaml.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("```\n");
    out
}

/// Resolve a user-supplied path: expand a leading `~`, and anchor relative
/// paths at the project directory (the agent's natural working root).
fn resolve_user_path(raw: &str, project_dir: &Path) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(base_dirs) = directories::BaseDirs::new()
    {
        return base_dirs.home_dir().join(rest);
    }
    let p = PathBuf::from(raw);
    if p.is_absolute() {
        p
    } else {
        project_dir.join(p)
    }
}

// ── 8. run_erc ─────────────────────────────────────────────────────────────

fn run_erc(ctx: &AgentRuntime) -> Result<Value> {
    if !ctx.sch_path().exists() {
        bail!(
            "no schematic to check at {} — apply a design first",
            ctx.sch_path().display()
        );
    }
    let report = KicadCli::new(ctx.env())
        .erc(ctx.sch_path())
        .with_context(|| format!("running ERC on {}", ctx.sch_path().display()))?;

    let violations = erc_violation_values(&report);

    Ok(json!({
        "errors": report.error_count(),
        "warnings": report.warning_count(),
        "violations": violations,
    }))
}

fn erc_violation_values(report: &ErcReport) -> Vec<Value> {
    report
        .violations
        .iter()
        .map(|v| {
            let mut item = json!({
                "severity": v.severity,
                "type": v.kind,
                "description": v.description,
            });
            if let Some(hint) = erc_hint(&v.kind, &v.description) {
                item["hint"] = json!(hint);
            }
            item
        })
        .collect()
}

/// A resolution hint for the ERC violation classes agents repeatedly fight
/// blind (observed: a model burning 26 design iterations on a driver
/// conflict). Only the classes with one clearly-right next move get a hint.
fn erc_hint(kind: &str, description: &str) -> Option<&'static str> {
    match kind {
        "pin_to_pin" if description.contains("Output and Power output") => Some(
            "Two driving pins share a net. Usual causes: a regulator/IC OUT pin tied \
             directly to a power symbol whose library pin is power-output, or two \
             outputs shorted. Fix by checking get_symbol_info pin types: use a plain \
             net label (not a power symbol) on driven rails, or pick the symbol \
             variant whose pin is power-out only where the rail is truly sourced.",
        ),
        "pin_not_connected" => Some(
            "Mark intentionally-unused pins no-connect in the YAML (pin: NC) instead \
             of leaving them dangling.",
        ),
        "pin_not_driven" | "power_pin_not_driven" => Some(
            "The net has only inputs/power-in pins. Add the sourcing connection, or \
             if the rail is sourced off-board (connector power), KiCAD wants a \
             PWR_FLAG-style source: connect the rail to the connector pin that \
             feeds it.",
        ),
        "different_unit_net" | "multiple_net_names" => Some(
            "The same wire carries two names. Keep one label per net; rename the \
             other uses to match.",
        ),
        _ => None,
    }
}

// ── 9. create_design / edit_design ────────────────────────────────────────

fn create_design(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let yaml = require_str(&input, "yaml")?;
    let overwrite = input
        .get("overwrite")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let prior_draft = ctx.workspace().read_draft()?;
    let draft_exists = prior_draft.is_some();
    if draft_exists && !overwrite {
        return Ok(json!({
            "error": "a draft already exists — pass overwrite=true to replace it, \
                      or use edit_design to modify it",
            "draft_changed": false,
        }));
    }
    let result = compile(&yaml, ctx.provider());
    let mut report = compile_authoring_report(&result, ctx)?;
    if reject_empty_draft_candidate(&mut report, &result, draft_exists, yaml.trim().is_empty()) {
        return Ok(report);
    }
    ctx.workspace()
        .write_draft(&yaml, current_sch_text(ctx).as_deref())?;
    report["draft_written"] = json!(true);
    report["draft_changed"] = json!(prior_draft.as_deref() != Some(yaml.as_str()));
    report["electrical_design_changed"] = json!(electrical_yaml_changed(
        prior_draft.as_deref(),
        &yaml,
        ctx.provider()
    ));
    add_draft_next_step(&mut report);
    Ok(report)
}

fn edit_design(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let full_yaml = input.get("yaml").and_then(Value::as_str);
    if let Some(yaml) = full_yaml {
        let prior_draft = ctx.workspace().read_draft()?;
        let draft_exists = prior_draft.is_some();
        let result = compile(yaml, ctx.provider());
        let mut report = compile_authoring_report(&result, ctx)?;
        if reject_empty_draft_candidate(&mut report, &result, draft_exists, yaml.trim().is_empty())
        {
            report["mode"] = json!(if draft_exists {
                "full_replace"
            } else {
                "full_create"
            });
            return Ok(report);
        }
        let prior_result = prior_draft
            .as_deref()
            .map(|draft| compile(draft, ctx.provider()));
        if let Some(prior) = prior_result
            .as_ref()
            .and_then(|compiled| compiled.design.as_ref())
            && result.design.is_none()
        {
            report["ok"] = json!(false);
            report["error"] = json!(
                "invalid full replacement would discard a valid draft; the existing draft was preserved"
            );
            report["code"] = json!("invalid_replacement_preserved_draft");
            report["current_design_state"] = design_state_summary(prior);
            report["current_diagnostics"] = json!(
                compile_report(&prior_result.as_ref().expect("checked above").diagnostics)["diagnostics"]
            );
            report["draft_written"] = json!(false);
            report["draft_changed"] = json!(false);
            report["mode"] = json!("full_replace");
            report["next_tool"] = json!("edit_design");
            report["next"] = json!(
                "fix the candidate diagnostics and resend the complete yaml, or use a precise patch against the preserved draft"
            );
            return Ok(report);
        }
        let allow_component_removal = input
            .get("allow_component_removal")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !allow_component_removal
            && let (Some(prior), Some(candidate)) = (
                prior_result
                    .as_ref()
                    .and_then(|compiled| compiled.design.as_ref()),
                result.design.as_ref(),
            )
        {
            let current_count = design_component_count(prior);
            let candidate_count = design_component_count(candidate);
            if candidate_count < current_count {
                report["ok"] = json!(false);
                report["error"] = json!(format!(
                    "full replacement would remove {} component(s)",
                    current_count - candidate_count
                ));
                report["code"] = json!("component_removal_requires_confirmation");
                report["current_component_count"] = json!(current_count);
                report["candidate_component_count"] = json!(candidate_count);
                report["current_design_state"] = design_state_summary(prior);
                report["current_diagnostics"] = json!(
                    compile_report(&prior_result.as_ref().expect("checked above").diagnostics)["diagnostics"]
                );
                report["draft_written"] = json!(false);
                report["draft_changed"] = json!(false);
                report["mode"] = json!("full_replace");
                report["next_tool"] = json!("edit_design");
                report["next"] = json!(
                    "the existing draft was preserved; use a precise patch to delete components, or resend the complete yaml with allow_component_removal=true"
                );
                return Ok(report);
            }
        }
        ctx.workspace()
            .write_draft(yaml, current_sch_text(ctx).as_deref())?;
        report["draft_written"] = json!(true);
        report["draft_changed"] = json!(prior_draft.as_deref() != Some(yaml));
        report["electrical_design_changed"] = json!(electrical_yaml_changed(
            prior_draft.as_deref(),
            yaml,
            ctx.provider()
        ));
        report["mode"] = json!(if draft_exists {
            "full_replace"
        } else {
            "full_create"
        });
        add_draft_next_step(&mut report);
        return Ok(report);
    }

    let Some(draft) = ctx.workspace().read_draft()? else {
        return Ok(json!({
            "error": "no draft exists — patch mode requires one; pass a complete yaml to edit_design \
                      to create it, or call read_schematic({source:\"draft\"}) to seed from the current schematic",
            "draft_changed": false,
        }));
    };

    let old = require_str(&input, "old_string")?;
    let new = require_str(&input, "new_string")?;
    let replace_all = input
        .get("replace_all")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let count = draft.matches(&*old).count();
    if count == 0 {
        return Ok(json!({
            "error": "old_string not found in the current draft",
            "old_string": old,
            "hint": "Use read_schematic({source:\"draft\"}) once to copy an exact current snippet, or call edit_design with a full corrected `yaml` for broad/formatting-heavy changes.",
            "draft_chars": draft.len(),
        }));
    }
    if count > 1 && !replace_all {
        return Ok(json!({
            "error": format!("old_string matches {count} times — make it more \
                              specific or pass replace_all=true"),
        }));
    }
    let edited = if replace_all {
        draft.replace(&*old, &new)
    } else {
        draft.replacen(&*old, &new, 1)
    };
    let result = compile(&edited, ctx.provider());
    let prior_result = compile(&draft, ctx.provider());
    if let Some(prior) = prior_result.design.as_ref()
        && result.design.is_none()
    {
        let mut report = compile_authoring_report(&result, ctx)?;
        report["ok"] = json!(false);
        report["error"] =
            json!("invalid patch would corrupt a valid draft; the existing draft was preserved");
        report["code"] = json!("invalid_patch_preserved_draft");
        report["current_design_state"] = design_state_summary(prior);
        report["current_diagnostics"] =
            json!(compile_report(&prior_result.diagnostics)["diagnostics"]);
        report["replacements"] = json!(if replace_all { count } else { 1 });
        report["draft_changed"] = json!(false);
        report["electrical_design_changed"] = json!(false);
        report["next_tool"] = json!("edit_design");
        report["next"] = json!(
            "send one complete valid corrected yaml document, or use a smaller patch that keeps the draft valid"
        );
        return Ok(report);
    }
    ctx.workspace()
        .write_draft(&edited, current_sch_text(ctx).as_deref())?;

    let mut report = compile_authoring_report(&result, ctx)?;
    report["replacements"] = json!(if replace_all { count } else { 1 });
    report["draft_changed"] = json!(edited != draft);
    report["electrical_design_changed"] = json!(electrical_yaml_changed(
        Some(&draft),
        &edited,
        ctx.provider()
    ));
    add_draft_next_step(&mut report);
    Ok(report)
}

/// Formatting, comments, quoting, and mapping order do not invalidate an
/// expensive semantic review. If either document is malformed, fall back to
/// byte identity so repair edits still count as progress.
fn electrical_yaml_changed(
    prior: Option<&str>,
    candidate: &str,
    provider: &circuit_lang::SymbolTable,
) -> bool {
    let Some(prior) = prior else {
        return true;
    };
    match (
        compile(prior, provider).design,
        compile(candidate, provider).design,
    ) {
        (Some(prior), Some(candidate)) => prior != candidate,
        _ => prior != candidate,
    }
}

/// Prevent a speculative empty skeleton from becoming the working draft.
/// Invalid but non-empty candidates remain writable so the model can repair
/// them iteratively; a successfully compiled zero-component design (or blank
/// input) has no useful repair anchor and must not replace prior work.
fn reject_empty_draft_candidate(
    report: &mut Value,
    result: &circuit_lang::CompileResult,
    draft_exists: bool,
    blank_input: bool,
) -> bool {
    let empty_design = result.design.as_ref().is_some_and(design_is_empty);
    if !blank_input && !empty_design {
        return false;
    }

    if blank_input && !empty_design {
        let errors = report.get("errors").and_then(Value::as_u64).unwrap_or(0) + 1;
        report["ok"] = json!(false);
        report["errors"] = json!(errors);
        report["diagnostics"]
            .as_array_mut()
            .expect("compile_report diagnostics must be an array")
            .push(json!(
                "error[empty_design]: the draft has no components; author the complete requested circuit before saving it"
            ));
    }
    report["draft_written"] = json!(false);
    report["draft_changed"] = json!(false);
    report["next_tool"] = json!("edit_design");
    report["next"] = json!(if draft_exists {
        "the empty candidate was rejected and the existing draft was preserved; send one complete non-empty replacement with edit_design({yaml: ...})"
    } else {
        "the empty candidate was rejected and no draft was written; send the complete non-empty circuit with edit_design({yaml: ...})"
    });
    true
}

fn add_draft_next_step(report: &mut Value) {
    let errors = report.get("errors").and_then(Value::as_u64).unwrap_or(0);
    let warnings = report.get("warnings").and_then(Value::as_u64).unwrap_or(0);
    report["validated"] = json!(true);
    if report.get("next_tool").is_some() {
        return; // preserve a more specific physical-compatibility recovery
    }
    if errors == 0 && warnings == 0 {
        report["next_tool"] = json!("apply_design");
        report["next"] = json!(
            "draft already compiled cleanly; call apply_design() next and do not revalidate it unchanged"
        );
    } else {
        report["next_tool"] = json!("edit_design");
        report["next"] = json!("fix the reported diagnostics in one batched edit");
    }
}

// ── 10. render_schematic ────────────────────────────────────────────────────

/// Result key carrying a PNG path for the agent loop to attach as an image
/// block (and strip from the JSON the model sees as text).
pub const IMAGE_PATH_KEY: &str = "_image_path";

fn render_schematic(ctx: &AgentRuntime) -> Result<Value> {
    if !ctx.sch_path().exists() {
        return Ok(json!({
            "error": "no schematic yet — apply a design first",
        }));
    }
    let png =
        crate::render::schematic_png(ctx.env(), ctx.sch_path(), ctx.config().tools.render_max_px)?;
    let path = ctx.workspace().write_render(&png)?;
    let mut obj = json!({
        "ok": true,
        "png_path": path.display().to_string(),
        "note": "image attached; also saved to png_path for the user to open",
    });
    obj[IMAGE_PATH_KEY] = json!(path.display().to_string());
    Ok(obj)
}
