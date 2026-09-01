//! The schematic + PCB tool registry the agent drives.
//!
//! Each tool is a thin, deterministic wrapper over logic that already lives in
//! `sch-check`, `kicad-footprint`/`kicad`, and `sch-floorplan`. The registry
//! exposes two free functions, both driven directly by the [`crate::Agent`] loop:
//!
//! - [`tool_defs`] — the JSON-Schema genai [`Tool`]s handed to the LLM.
//! - [`run_tool`] — dispatch a tool by name with a JSON input, returning the
//!   result the model reads back. Most results are structured JSON for
//!   **self-repair**: failures carry
//!   diagnostic strings and "did you mean" suggestions rather than just an error
//!   flag, so the model can correct itself on the next turn.
//!
//! The live schematic surface — the queries and pin-level mutators the model
//! edits an existing board with — lives in `gordian-tools-sch` and is spliced
//! in by [`tool_defs`] / [`run_tool`]. This module keeps what is left: symbol
//! discovery (`search_symbols` / `get_symbol_info`), `project_info` and
//! `render_schematic`; `pcb-workflow` covers the footprint
//! search/info, `regenerate_board`, and the place/route/export/interactive flow.
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

use gordian_runtime::tool::{IMAGE_PATH_KEY, require_search_query, require_str};

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

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
            description: "Find symbol `Lib:Name`; batch 4 queries. Common parts are built in."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "minLength": 1 },
                    "limit": { "type": "integer", "minimum": 1 },
                    "queries": {
                        "type": "array", "minItems": 1, "maxItems": 4,
                        "items": {
                            "type": "object",
                            "properties": {
                                "query": { "type": "string", "minLength": 1 },
                                "limit": { "type": "integer", "minimum": 1 }
                            },
                            "required": ["query"],
                            "additionalProperties": false
                        }
                    }
                },
                "anyOf": [{ "required": ["query"] }, { "required": ["queries"] }],
                "additionalProperties": false
            }),
        },
        Def {
            name: "get_symbol_info".into(),
            description: "Return symbol ratings, datasheet, footprint, and pins.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "lib_id": { "type": "string", "description": "E.g. Device:R." }
                },
                "required": ["lib_id"]
            }),
        },
        Def {
            name: "project_info".into(),
            description: "Return project paths/state.".into(),
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        Def {
            name: "render_schematic".into(),
            description: "Render schematic PNG, once, at the end.".into(),
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        // ── PCB tools (slice 5) ─────────────────────────────────────────
        Def {
            name: "search_footprints".into(),
            description: "Find footprint `Lib:Name` IDs; batch 4 queries.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "minLength": 1 },
                    "limit": { "type": "integer", "minimum": 1 },
                    "queries": {
                        "type": "array", "minItems": 1, "maxItems": 4,
                        "items": {
                            "type": "object",
                            "properties": {
                                "query": { "type": "string", "minLength": 1 },
                                "limit": { "type": "integer", "minimum": 1 }
                            },
                            "required": ["query"],
                            "additionalProperties": false
                        }
                    }
                },
                "anyOf": [{ "required": ["query"] }, { "required": ["queries"] }],
                "additionalProperties": false
            }),
        },
        Def {
            name: "get_footprint_info".into(),
            description: "Return pads and geometry for footprint `Lib:Name`.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "lib_id": { "type": "string" }
                },
                "required": ["lib_id"]
            }),
        },
        Def {
            name: "open_board".into(),
            description: "Open PCB for live IPC edits; return board state.".into(),
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        Def {
            name: "move_parts".into(),
            description: "Move footprints by to, by, near, or edge, with rotation/offsets.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "moves": {
                        "type": "array",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": {
                                "reference": { "type": "string" },
                                "to": {
                                    "type": "array",
                                    "items": { "type": "number" },
                                    "minItems": 2,
                                    "maxItems": 2
                                },
                                "by": {
                                    "type": "array",
                                    "items": { "type": "number" },
                                    "minItems": 2,
                                    "maxItems": 2
                                },
                                "near": { "type": "string" },
                                "side": { "type": "string", "enum": ["left", "right", "above", "below"] },
                                "edge": { "type": "string", "enum": ["left", "right", "top", "bottom"] },
                                "gap": { "type": "number" },
                                "rotation": { "type": "number" },
                                "horizontal_offset": { "type": "number" },
                                "vertical_offset": { "type": "number" }
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
            description: "Route one connection around obstacles, with layers and optional vias."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "from": {
                        "type": "array",
                        "items": {"type":"number"},
                        "minItems": 2,
                        "maxItems": 2,
                    },
                    "to": {
                        "type": "array",
                        "items": {"type":"number"},
                        "minItems": 2,
                        "maxItems": 2,
                    },
                    "net": { "type": "string" },
                    "from_layer": { "type": "string", "description": "Layer; default F.Cu." },
                    "to_layer": { "type": "string" },
                    "width": { "type": "number" },
                    "vias": {
                        "type": "array",
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
            description: "Delete nearby track/via; filter by kind, net, or layer.".into(),
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
            description: "Set net-class width/clearance; prefer regeneration rules pre-route."
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
            description: "Edit Edge.Cuts by bounds, polygon, or fitted geometry.".into(),
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
                        "description": "Closed [[x,y],...] polygon (mm).",
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
                        "description": "Fit around parts/copper."
                    },
                    "margin": { "type": "number", "description": "Margin mm; default 2." }
                }
            }),
        },
        Def {
            name: "regenerate_board".into(),
            description: "Seed PCB; optional bounds/rules use safe defaults.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "bounds": {
                        "type": "object",
                        "properties": {
                            "min_x": { "type": "number" }, "max_x": { "type": "number" },
                            "min_y": { "type": "number" }, "max_y": { "type": "number" }
                        }
                    },
                    "rules": {
                        "type": "object",
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
                                        "layer": { "type": "string" },
                                        "connect": { "type": "string", "enum": ["thermal", "solid"] }
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
            description: "Return board; net adds pad centers, include_copper adds copper.".into(),
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
            description: "Auto-place PCB; groups steer regions, grids, surrounds, and edges."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "groups": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "name": { "type": "string" },
                                "members": { "type": "array", "items": { "type": "string" } },
                                "region": {
                                    "type": "object",
                                    "properties": {
                                        "min_x": { "type": "number" }, "max_x": { "type": "number" },
                                        "min_y": { "type": "number" }, "max_y": { "type": "number" }
                                    },
                                    "required": ["min_x", "min_y", "max_x", "max_y"],
                                    "additionalProperties": false
                                },
                                "edge": { "type": "string", "enum": ["n", "s", "e", "w"] },
                                "grid": { "type": "boolean" },
                                "rotation": { "type": "number", "enum": [0, 90, 180, 270] },
                                "surround": { "type": "string", "description": "Anchor reference to surround." }
                            },
                            "required": ["name", "members"],
                            "additionalProperties": false
                        }
                    },
                    "edge_seek": { "type": "array", "items": { "type": "string" } },
                    "corner_seek": { "type": "array", "items": { "type": "string" } }
                },
                "additionalProperties": false
            }),
        },
        Def {
            name: "route_board".into(),
            description: "Auto-route board; reports exact failed connections.".into(),
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        Def {
            name: "render_board".into(),
            description: "Render board PNG.".into(),
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        Def {
            name: "check_board".into(),
            description: "Run PCB DRC; stop when ok.".into(),
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        Def {
            name: "export_fab".into(),
            description: "Export fabrication files to <project>/fab after clean check_board."
                .into(),
            input_schema: json!({ "type": "object", "properties": {} }),
        },
    ];
    gordian_tools_sch::tool_defs()
        .into_iter()
        .chain(defs.into_iter().map(|d| {
            Tool::new(d.name)
                .with_description(d.description)
                .with_schema(d.input_schema)
        }))
        .collect()
}

/// Dispatch a tool by name (synchronous). `input` is the model-supplied JSON
/// arguments; the returned `Value` is fed back to the model. The [`crate::Agent`]
/// loop off-loads this onto the blocking pool.
pub fn run_tool(name: &str, input: Value, ctx: &AgentRuntime) -> Result<Value> {
    if let Some(result) = gordian_tools_sch::run(name, input.clone(), ctx) {
        return result;
    }
    match name {
        "search_symbols" => search_symbols(input, ctx),
        "get_symbol_info" => get_symbol_info(input, ctx),
        "project_info" => project_info(ctx),
        "render_schematic" => render_schematic(ctx),
        "search_footprints" => pcb_workflow::search_footprints(input, ctx),
        "get_footprint_info" => pcb_workflow::get_footprint_info(input, ctx),
        "regenerate_board" => pcb_workflow::regenerate_board(input, ctx),
        "get_board" => pcb_workflow::get_board(input, ctx),
        "place_board" => pcb_workflow::place_board(input, ctx),
        "route_board" => pcb_workflow::route_board(input, ctx),
        "check_board" => pcb_workflow::check_board(input, ctx),
        "export_fab" => pcb_workflow::export_fab(input, ctx),
        "open_board" => pcb_workflow::open_board(input, ctx),
        "move_parts" => pcb_workflow::move_parts(input, ctx),
        "route_track" => pcb_workflow::route_track(input, ctx),
        "delete_copper" => pcb_workflow::delete_copper(input, ctx),
        "set_net_width" => pcb_workflow::set_net_width(input, ctx),
        "update_board_outline" => pcb_workflow::update_board_outline(input, ctx),
        "render_board" => pcb_workflow::render_board(input, ctx),
        other => bail!("unknown tool: {other}"),
    }
}

// ── 1. search_symbols ──────────────────────────────────────────────────────

fn search_symbols(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    if let Some(queries) = input.get("queries") {
        let queries = queries
            .as_array()
            .ok_or_else(|| anyhow!("`queries` must be an array"))?;
        if queries.is_empty() || queries.len() > 4 {
            bail!("`queries` must contain 1 to 4 searches");
        }
        let mut results = Vec::with_capacity(queries.len());
        for item in queries {
            let query = require_search_query(item)?;
            let limit = search_limit(item, ctx);
            let mut result = search_symbols_one(&query, limit, ctx)?;
            result["query"] = json!(query);
            results.push(result);
        }
        return Ok(json!({ "results": results }));
    }
    let query = require_search_query(&input)?;
    search_symbols_one(&query, search_limit(&input, ctx), ctx)
}

fn search_limit(input: &Value, ctx: &AgentRuntime) -> usize {
    input
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(ctx.config().tools.default_search_limit)
}

fn search_symbols_one(query: &str, limit: usize, ctx: &AgentRuntime) -> Result<Value> {
    if let Some((lib_id, pin_count)) = common_connector_symbol_alias(query) {
        return Ok(json!({
            "hits": [{ "lib_id": lib_id, "pin_count": pin_count }],
            "note": "Connector symbols and physical footprints are separate choices. Use this symbol for the schematic pins; use search_footprints for the physical connector footprint.",
        }));
    }
    if let Some((lib_id, pin_count)) = builtin_symbol_alias(query) {
        return Ok(json!({
            "hits": [{ "lib_id": lib_id, "pin_count": pin_count }],
            "note": "built-in alias/canonical symbol; use it directly and do not repeat this search",
        }));
    }

    let hits: Vec<Value> = ctx
        .index()?
        .search(query, limit)
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

    match ctx
        .provider()
        .symbol(&lib_id)
        .or_else(|| ctx.index().ok()?.symbol(&lib_id))
    {
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

/// Render a [`sch_check::PinType`] as a stable lowercase string for the LLM.
fn pin_type_str(t: sch_check::PinType) -> &'static str {
    use sch_check::PinType::*;
    match t {
        PowerInput => "power_input",
        PowerOutput => "power_output",
        Passive => "passive",
        Other => "other",
    }
}

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

/// Result key carrying a PNG path for the agent loop to attach as an image
/// block (and strip from the JSON the model sees as text).
fn render_schematic(ctx: &AgentRuntime) -> Result<Value> {
    if !ctx.sch_path().exists() {
        return Ok(json!({
            "error": "no schematic yet — create one with place_parts first",
        }));
    }
    let png = gordian_runtime::render::schematic_png(
        ctx.env(),
        ctx.sch_path(),
        ctx.config().tools.render_max_px,
    )?;
    let path = ctx.workspace().write_render(&png)?;
    let mut obj = json!({
        "ok": true,
        "png_path": path.display().to_string(),
        "note": "image attached; also saved to png_path for the user to open",
    });
    obj[IMAGE_PATH_KEY] = json!(path.display().to_string());
    Ok(obj)
}
