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
use circuit_lang::model::{Block, Component, Design, Origin, PinTarget};
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
                description: "Return symbol ratings, datasheet, footprint, and pins."
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
                description: "Validate YAML or draft; omit yaml for draft."
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
                description: "Compile, render, write draft, and run ERC; edit first."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
            },
            Def {
                name: "review_design".into(),
                description: "Full electrical review before PCB; fix defects."
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
                description: "Run fresh KiCAD ERC; skip after clean apply_design."
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
                description: "Read circuit YAML from draft or .kicad_sch; draft reads/seeds state."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "source": { "type": "string", "description": "draft (default) or .kicad_sch." }
                    }
                }),
            },
            Def {
                name: "render_schematic".into(),
                description: "Render schematic PNG."
                    .into(),
                input_schema: json!({ "type": "object", "properties": {} }),
            },
            Def {
                name: "create_design".into(),
                description: "Create complete requested circuit; no examples or fragments."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "yaml": { "type": "string", "description": "Complete top-level circuit-YAML for the request." },
                        "overwrite": { "type": "boolean" }
                    },
                    "required": ["yaml"]
                }),
            },
            Def {
                name: "edit_design".into(),
                description: "Replace full draft; part loss needs allow_component_removal."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "yaml": { "type": "string" },
                        "allow_component_removal": { "type": "boolean" }
                    },
                    "required": ["yaml"],
                    "additionalProperties": false
                }),
            },
            // ── PCB tools (slice 5) ─────────────────────────────────────────
            Def {
                name: "search_footprints".into(),
                description: "Find footprint `Lib:Name` IDs; batch 4 queries."
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
                name: "get_footprint_info".into(),
                description: "Return pads and geometry for footprint `Lib:Name`."
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
                description: "Set draft footprints; apply before PCB regeneration."
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
                description: "Open PCB for live IPC edits; return board state."
                    .into(),
                input_schema: json!({ "type": "object", "properties": {} }),
            },
            Def {
                name: "move_parts".into(),
                description: "Move footprints by to, by, near, or edge, with rotation/offsets."
                    .into(),
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
                description: "Delete nearby track/via; filter by kind, net, or layer."
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
                description: "Edit Edge.Cuts by bounds, polygon, or fitted geometry."
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
                description: "Return board; net adds pad centers, include_copper adds copper."
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
                description: "Auto-route board; return metrics, diagnostic failure records, and exact unique failed-connection counts."
                    .into(),
                input_schema: json!({ "type": "object", "properties": {} }),
            },
            Def {
                name: "render_board".into(),
                description: "Render board PNG."
                    .into(),
                input_schema: json!({ "type": "object", "properties": {} }),
            },
            Def {
                name: "check_board".into(),
                description: "Run PCB DRC; stop when ok."
                    .into(),
                input_schema: json!({ "type": "object", "properties": {} }),
            },
            Def {
                name: "export_fab".into(),
                description: "Export fabrication files after clean check_board."
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

/// On-demand transactional repair surface. It is intentionally absent from
/// [`tool_defs`]: the agent advertises it only while repairing a valid draft,
/// avoiding permanent schema cost on discovery and PCB-only turns.
pub(crate) fn repair_components_tool() -> Tool {
    let component_schema = json!({
        "type": "object",
        "properties": {
            "part": { "type": "string" },
            "value": { "type": "string" },
            "footprint": { "type": "string" },
            "dnp": { "type": "boolean" },
            "props": { "type": "object", "additionalProperties": { "type": "string" } },
            "pins": { "type": "object", "description": "Pin name/number to net name or nc.", "additionalProperties": { "type": "string" } },
            "units": {
                "type": "object",
                "additionalProperties": {
                    "type": "object",
                    "properties": { "pins": { "type": "object", "additionalProperties": { "type": "string" } } },
                    "required": ["pins"],
                    "additionalProperties": false
                }
            },
            "between": { "type": "array", "items": { "type": "string" }, "minItems": 2, "maxItems": 2 },
            "positive": { "type": "string" },
            "negative": { "type": "string" },
            "decouple": { "type": "object", "additionalProperties": { "type": "integer", "minimum": 1 } }
        },
        "required": ["part"],
        "additionalProperties": false
    });
    let component_map_schema = json!({
        "type": "object",
        "minProperties": 1,
        "additionalProperties": component_schema
    });
    Tool::new("repair_components")
        .with_description(
            "Repair localized defects across a substantive durable draft in one batch. Existing refs are routed to their current blocks automatically. NOT for an incomplete/missing circuit; use edit_design with complete YAML for that. Prefer update for existing refs. Components is only a direct refdes map for additions/replacements.",
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "block": { "type": "string", "description": "Destination block for new refs; defaults to main. Existing refs are repaired in their current blocks, so one update/remove/components batch may span blocks." },
                "components": {
                    "description": "DIRECT refdes-to-component map for additions/replacements; no version/blocks/main/components wrapper. `part` is required. Existing metadata is preserved when omitted. Example: {\"D1\":{\"part\":\"Device:D\",\"pins\":{\"1\":\"VIN\",\"2\":\"VOUT\"}}}.",
                    "type": component_map_schema["type"].clone(),
                    "minProperties": component_map_schema["minProperties"].clone(),
                    "additionalProperties": component_map_schema["additionalProperties"].clone()
                },
                "update": {
                    "type": "object",
                    "description": "DIRECT refdes-to-update map for existing authored refs; no YAML wrapper. Use this—not components—for pin/value/footprint changes. Omitted fields/pins are preserved. Pin updates must use an exact existing pin key from the draft. Example: {\"D1\":{\"pins\":{\"1\":\"VIN\",\"2\":\"VPROT\"},\"footprint\":\"Diode_SMD:D_SOD-123\"},\"TP1\":{\"pins\":{\"1\":\"VPROT\"}}}.",
                    "additionalProperties": {
                        "type": "object",
                        "properties": {
                            "pins": { "type": "object", "description": "Merge targets for exact pin keys already present on this component in the draft; values are a net name or nc.", "additionalProperties": { "type": "string" }, "minProperties": 1 },
                            "value": { "type": "string" },
                            "footprint": { "type": "string" }
                        },
                        "minProperties": 1,
                        "additionalProperties": false
                    },
                    "minProperties": 1
                },
                "remove": {
                    "type": "array",
                    "description": "One or more explicit authored refs to remove. Example: [\"R7\"].",
                    "items": { "type": "string" },
                    "minItems": 1,
                    "uniqueItems": true
                },
                "replace_existing": { "type": "boolean", "description": "Required only when components changes an existing ref's symbol part." }
            },
            "additionalProperties": false,
            "anyOf": [
                { "required": ["components"], "properties": { "components": { "minProperties": 1 } } },
                { "required": ["update"], "properties": { "update": { "minProperties": 1 } } },
                { "required": ["remove"], "properties": { "remove": { "minItems": 1 } } }
            ]
        }))
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
        "repair_components" => repair_components(input, ctx),
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

pub(crate) fn require_search_query(input: &Value) -> Result<String> {
    let query = require_str(input, "query")?;
    if query.trim().is_empty() {
        bail!("search query must contain non-whitespace text");
    }
    Ok(query)
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
    use std::collections::{BTreeMap, HashMap, HashSet};

    const MAX_DIAGNOSTICS: usize = 40;
    const MAX_REPRESENTATIVES_PER_CODE: usize = 6;

    let mut code_counts = BTreeMap::<&str, (usize, usize)>::new();
    for d in &diags.0 {
        let counts = code_counts.entry(d.code).or_default();
        match d.severity {
            Severity::Error => counts.0 += 1,
            Severity::Warning => counts.1 += 1,
        }
    }

    // Reserve one representative for every diagnostic class before allowing a
    // repetitive class to consume the remaining context budget. This keeps a
    // large syntax-error family from hiding later pin or electrical errors.
    let mut selected = vec![false; diags.0.len()];
    let mut represented_codes = HashSet::new();
    let mut selected_count = 0usize;
    for (index, d) in diags.0.iter().enumerate() {
        if selected_count == MAX_DIAGNOSTICS {
            break;
        }
        if represented_codes.insert(d.code) {
            selected[index] = true;
            selected_count += 1;
        }
    }
    let mut representatives_per_code = represented_codes
        .into_iter()
        .map(|code| (code, 1usize))
        .collect::<HashMap<_, _>>();
    for (index, d) in diags.0.iter().enumerate() {
        if selected_count == MAX_DIAGNOSTICS {
            break;
        }
        let count = representatives_per_code.entry(d.code).or_default();
        if !selected[index] && *count < MAX_REPRESENTATIVES_PER_CODE {
            selected[index] = true;
            selected_count += 1;
            *count += 1;
        }
    }
    let strings = diags
        .0
        .iter()
        .zip(selected)
        .filter(|(_, selected)| *selected)
        .map(|(d, _)| d.to_string())
        .collect::<Vec<_>>();
    let omitted = diags.0.len() - strings.len();
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
        "diagnostic_code_counts": code_counts.into_iter().map(|(code, (errors, warnings))| {
            (code.to_owned(), json!({ "errors": errors, "warnings": warnings }))
        }).collect::<serde_json::Map<_, _>>(),
        "errors": errors,
        "warnings": warnings,
    });
    if omitted > 0 {
        report["diagnostics_omitted"] = json!(omitted);
        report["note"] = json!(
            "diagnostics are representative and truncated by code for context efficiency; diagnostic_code_counts preserves exact totals"
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

fn invalid_compile_quality_regressed(
    prior: &circuit_lang::Diagnostics,
    candidate: &circuit_lang::Diagnostics,
) -> bool {
    let prior = compile_diagnostic_quality(prior);
    let candidate = compile_diagnostic_quality(candidate);
    prior.0 > 0 && candidate > prior
}

fn compile_diagnostic_quality(diagnostics: &circuit_lang::Diagnostics) -> (usize, usize) {
    use circuit_lang::Severity;

    let errors = diagnostics
        .0
        .iter()
        .filter(|diagnostic| diagnostic.severity == Severity::Error)
        .count();
    let warnings = diagnostics
        .0
        .iter()
        .filter(|diagnostic| diagnostic.severity == Severity::Warning)
        .count();
    (errors, warnings)
}

fn authoring_report_quality(report: &Value) -> (u64, u64) {
    (
        report.get("errors").and_then(Value::as_u64).unwrap_or(0),
        report.get("warnings").and_then(Value::as_u64).unwrap_or(0),
    )
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
            "error": "`commit` has been removed from apply_design; call apply_design({}) through the approval gate",
        }));
    }
    if input.get("yaml").is_some() {
        return Ok(json!({
            "error": "inline YAML has been removed from apply_design",
            "code": "inline_apply_yaml_removed",
            "note": "Author the complete durable draft with edit_design({yaml}), then call apply_design({}).",
        }));
    }
    let yaml = match ctx.workspace().read_draft()? {
        Some(draft) => draft,
        None => {
            return Ok(json!({
                "error": "no draft exists — create the complete draft with edit_design({yaml}) before apply_design({})",
            }));
        }
    };
    let stale = ctx
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

fn symbol_for_misplaced_footprint(footprint: &str, pad_count: usize) -> Option<String> {
    let (library, name) = footprint.split_once(':')?;
    let symbol = if library.starts_with("Resistor_") {
        "Device:R"
    } else if library.starts_with("Capacitor_") {
        "Device:C"
    } else if library.starts_with("Inductor_") {
        "Device:L"
    } else if library.starts_with("LED_") {
        "Device:LED"
    } else if matches!(library, "Diode_SMD" | "Diode_THT") {
        "Device:D"
    } else if library == "MountingHole" && name.starts_with("MountingHole") {
        "Mechanical:MountingHole"
    } else if (library.contains("TestPoint") || name.starts_with("TestPoint")) && pad_count == 1 {
        "Connector:TestPoint"
    } else if library.starts_with("Connector_PinHeader_")
        && name.starts_with("PinHeader_1x")
        && (1..=40).contains(&pad_count)
    {
        return Some(format!("Connector_Generic:Conn_01x{pad_count:02}"));
    } else {
        return None;
    };
    Some(symbol.to_owned())
}

fn normalize_misplaced_footprint_parts(
    yaml: &str,
    ctx: &AgentRuntime,
) -> Result<(String, Vec<Value>)> {
    let Some(surface) = circuit_lang::parse::parse_str(yaml).0 else {
        return Ok((yaml.to_owned(), Vec::new()));
    };
    let catalog = ctx.footprint_catalog()?;
    let mut replacements = Vec::new();
    for block in surface.blocks.values() {
        for (reference, component) in &block.components {
            let Ok(id) = FootprintId::parse(&component.part) else {
                continue;
            };
            let Ok(footprint) = catalog.footprint(&id) else {
                continue;
            };
            let Some(symbol) = symbol_for_misplaced_footprint(&component.part, footprint.pads.len())
            else {
                continue;
            };
            replacements.push((reference.clone(), component.part.clone(), symbol));
        }
    }

    let mut normalized = yaml.to_owned();
    let mut report = Vec::new();
    for (reference, footprint, symbol) in replacements {
        normalized = crate::tools_pcb::patch_part_and_footprint(
            &normalized,
            &reference,
            &symbol,
            &footprint,
        )
        .map_err(anyhow::Error::msg)?;
        report.push(json!({
            "reference": reference,
            "original_part": footprint,
            "inferred_symbol": symbol,
            "assigned_footprint": footprint,
        }));
    }
    Ok((normalized, report))
}

fn add_footprint_part_normalizations(report: &mut Value, normalizations: Vec<Value>) {
    if !normalizations.is_empty() {
        const MAX_EXAMPLES: usize = 8;
        let count = normalizations.len();
        let examples = normalizations
            .into_iter()
            .take(MAX_EXAMPLES)
            .collect::<Vec<_>>();
        report["normalized_misplaced_footprints"] = json!({
            "count": count,
            "examples": examples,
            "omitted": count.saturating_sub(MAX_EXAMPLES),
        });
    }
}

/// Exact, common shorthand names that KiCad does not ship. Keep this list
/// deliberately narrow: package aliases are safe for these two-pin passives,
/// but active-device aliases can silently change pin mappings.
fn common_footprint_alias(footprint: &str) -> Option<&'static str> {
    match footprint {
        "Resistor_SMD:R_0603" => Some("Resistor_SMD:R_0603_1608Metric"),
        "Capacitor_SMD:C_0603" => Some("Capacitor_SMD:C_0603_1608Metric"),
        "LED_SMD:LED_0603" => Some("LED_SMD:LED_0603_1608Metric"),
        "Diode_SMD:SOD-123" => Some("Diode_SMD:D_SOD-123"),
        "Connector:PinHeader_1x02_P2.54mm_Vertical" => {
            Some("Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical")
        }
        "Connector:PinHeader_1x08_P2.54mm_Vertical" => {
            Some("Connector_PinHeader_2.54mm:PinHeader_1x08_P2.54mm_Vertical")
        }
        "MountingHole:MountingHole_M3" => Some("MountingHole:MountingHole_3.2mm_M3"),
        _ => None,
    }
}

fn verified_common_footprint_alias(
    footprint: &str,
    catalog: &kicad_footprint::FootprintCatalog,
) -> Option<&'static str> {
    let canonical = common_footprint_alias(footprint)?;
    let id = FootprintId::parse(canonical).ok()?;
    catalog.footprint(&id).ok()?;
    Some(canonical)
}

fn normalize_common_footprint_aliases(
    yaml: &str,
    ctx: &AgentRuntime,
) -> Result<(String, Vec<Value>)> {
    let Some(surface) = circuit_lang::parse::parse_str(yaml).0 else {
        return Ok((yaml.to_owned(), Vec::new()));
    };
    let catalog = ctx.footprint_catalog()?;
    let mut replacements = Vec::new();
    for block in surface.blocks.values() {
        for (reference, component) in &block.components {
            let Some(original) = component.footprint.as_deref() else {
                continue;
            };
            let Some(canonical) = verified_common_footprint_alias(original, catalog) else {
                continue;
            };
            replacements.push((reference.clone(), original.to_owned(), canonical));
        }
    }

    let mut normalized = yaml.to_owned();
    let mut report = Vec::new();
    for (reference, original, canonical) in replacements {
        normalized = crate::tools_pcb::patch_footprint(&normalized, &reference, canonical)
            .map_err(anyhow::Error::msg)?
            .0;
        report.push(json!({
            "reference": reference,
            "original_footprint": original,
            "canonical_footprint": canonical,
        }));
    }
    Ok((normalized, report))
}

fn normalize_common_footprint_aliases_in_component_map(
    components: &mut serde_json::Map<String, Value>,
    ctx: &AgentRuntime,
) -> Result<Vec<Value>> {
    let catalog = ctx.footprint_catalog()?;
    let mut report = Vec::new();
    for (reference, component) in components {
        let Some(fields) = component.as_object_mut() else {
            continue;
        };
        let Some(original) = fields
            .get("footprint")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            continue;
        };
        let Some(canonical) = verified_common_footprint_alias(&original, catalog) else {
            continue;
        };
        fields.insert("footprint".into(), json!(canonical));
        report.push(json!({
            "reference": reference,
            "original_footprint": original,
            "canonical_footprint": canonical,
        }));
    }
    Ok(report)
}

fn add_footprint_alias_normalizations(report: &mut Value, normalizations: Vec<Value>) {
    if !normalizations.is_empty() {
        const MAX_EXAMPLES: usize = 8;
        let count = normalizations.len();
        report["normalized_footprint_aliases"] = json!({
            "count": count,
            "examples": normalizations.into_iter().take(MAX_EXAMPLES).collect::<Vec<_>>(),
            "omitted": count.saturating_sub(MAX_EXAMPLES),
        });
    }
}

/// Conservative physical defaults for dense, implementation-oriented drafts.
/// These are canonical package families with predictable symbol/pad mappings;
/// less universal choices (ICs, polarized capacitors, switches, and terminal
/// blocks) deliberately remain explicit author decisions.
fn common_default_footprint(part: &str) -> Option<String> {
    let fixed = match part {
        "Device:R" => "Resistor_SMD:R_0603_1608Metric",
        "Device:C" => "Capacitor_SMD:C_0603_1608Metric",
        "Device:D" => "Diode_SMD:D_SOD-123",
        "Device:LED" => "LED_SMD:LED_0603_1608Metric",
        "Mechanical:MountingHole" => "MountingHole:MountingHole_3.2mm_M3",
        _ => {
            let (columns, pins) = if let Some(pins) = part
                .strip_prefix("Connector_Generic:Conn_01x")
                .or_else(|| {
                    part.strip_prefix("Connector:Conn_01x")
                        .and_then(|pins| pins.strip_suffix("_Pin"))
                }) {
                (1, pins)
            } else if let Some(pins) = part
                .strip_prefix("Connector_Generic:Conn_02x")
                .and_then(|pins| pins.strip_suffix("_Odd_Even"))
            {
                (2, pins)
            } else {
                return None;
            };
            let rows = pins.parse::<usize>().ok()?;
            if rows == 0 || rows > 40 {
                return None;
            }
            return Some(format!(
                "Connector_PinHeader_2.54mm:PinHeader_{columns}x{rows:02}_P2.54mm_Vertical"
            ));
        }
    };
    Some(fixed.to_owned())
}

/// Fill conventional footprints only when a draft is large enough that it is
/// clearly intended for physical implementation. Sparse examples and early
/// sketches retain their useful footprint-free behavior.
fn normalize_dense_default_footprints(
    yaml: &str,
    ctx: &AgentRuntime,
) -> Result<(String, Vec<Value>)> {
    const DENSE_PHYSICAL_COMPONENTS: usize = 40;

    let Some(surface) = circuit_lang::parse::parse_str(yaml).0 else {
        return Ok((yaml.to_owned(), Vec::new()));
    };
    let physical_count = surface
        .blocks
        .values()
        .flat_map(|block| block.components.values())
        .filter(|component| {
            !component.dnp
                && !component.part.starts_with("power:")
                && !component.part.starts_with("label:")
        })
        .count();
    if physical_count < DENSE_PHYSICAL_COMPONENTS {
        return Ok((yaml.to_owned(), Vec::new()));
    }

    let catalog = ctx.footprint_catalog()?;
    let mut assignments = Vec::new();
    for block in surface.blocks.values() {
        for (reference, component) in &block.components {
            if component.dnp
                || component.footprint.is_some()
                || component.part.starts_with("power:")
                || component.part.starts_with("label:")
            {
                continue;
            }
            let Some(footprint) = common_default_footprint(&component.part) else {
                continue;
            };
            let Ok(id) = FootprintId::parse(&footprint) else {
                continue;
            };
            if catalog.footprint(&id).is_err() {
                continue;
            }
            assignments.push((reference.clone(), component.part.clone(), footprint));
        }
    }

    let mut normalized = yaml.to_owned();
    let mut report = Vec::new();
    for (reference, part, footprint) in assignments {
        normalized = crate::tools_pcb::patch_footprint(&normalized, &reference, &footprint)
            .map_err(anyhow::Error::msg)?
            .0;
        report.push(json!({
            "reference": reference,
            "part": part,
            "assigned_footprint": footprint,
        }));
    }
    Ok((normalized, report))
}

fn add_default_footprint_normalizations(report: &mut Value, normalizations: Vec<Value>) {
    if !normalizations.is_empty() {
        const MAX_EXAMPLES: usize = 8;
        let count = normalizations.len();
        report["assigned_default_footprints"] = json!({
            "count": count,
            "examples": normalizations.into_iter().take(MAX_EXAMPLES).collect::<Vec<_>>(),
            "omitted": count.saturating_sub(MAX_EXAMPLES),
        });
    }
}

/// Repair a narrow but costly authoring slip: models sometimes turn a rail name
/// into a logical power-symbol reference by appending an instance number (for
/// example `V3V3` -> `V3V31`).  That is not a legal KiCad refdes, and one such
/// key otherwise invalidates an entire large replacement document.  Power
/// symbols have no user-significant physical reference, so give only invalid
/// `power:*` entries a deterministic, collision-free `PWRn` reference before
/// compilation.  Physical component references remain strict.
fn normalize_invalid_power_references(yaml: &str) -> (String, Vec<Value>) {
    let (surface, diagnostics) = circuit_lang::parse::parse_str(yaml);
    let Some(surface) = surface else {
        return (yaml.to_owned(), Vec::new());
    };

    let mut used = surface
        .blocks
        .values()
        .flat_map(|block| block.components.keys().cloned())
        .collect::<std::collections::HashSet<_>>();
    let mut next_power = 1usize;
    let mut edits = Vec::new();
    for diagnostic in diagnostics.0.iter().filter(|d| d.code == "bad-refdes") {
        let Some(span) = diagnostic.span else { continue };
        let Some(reference) = diagnostic
            .message
            .split('`')
            .nth(1)
            .map(str::to_owned)
        else {
            continue;
        };
        let is_power = surface.blocks.values().any(|block| {
            block
                .components
                .get(&reference)
                .is_some_and(|component| component.part.starts_with("power:"))
        });
        if !is_power {
            continue;
        }
        let replacement = loop {
            let candidate = format!("PWR{next_power}");
            next_power += 1;
            if used.insert(candidate.clone()) {
                break candidate;
            }
        };
        edits.push((span, reference, replacement));
    }

    // Work from the end of the document so earlier source coordinates remain
    // valid. Diagnostics point at the component-map key, including in flow YAML.
    edits.sort_by_key(|(span, _, _)| (std::cmp::Reverse(span.line), std::cmp::Reverse(span.col)));
    let mut normalized = yaml.to_owned();
    let mut report = Vec::new();
    for (span, reference, replacement) in edits {
        let Some(line_start) = normalized
            .split_inclusive('\n')
            .take(span.line.saturating_sub(1))
            .map(str::len)
            .reduce(|a, b| a + b)
            .or_else(|| (span.line == 1).then_some(0))
        else {
            continue;
        };
        let line_end = normalized[line_start..]
            .find('\n')
            .map(|offset| line_start + offset)
            .unwrap_or(normalized.len());
        let line = &normalized[line_start..line_end];
        let Some(column_offset) = line
            .char_indices()
            .nth(span.col.saturating_sub(1))
            .map(|(offset, _)| offset)
            .or_else(|| (span.col == line.chars().count() + 1).then_some(line.len()))
        else {
            continue;
        };
        let start = line_start + column_offset;
        let suffix = &normalized[start..line_end];
        let literals = [
            reference.clone(),
            format!("'{reference}'"),
            format!("\"{reference}\""),
        ];
        let Some(literal) = literals.into_iter().find(|literal| {
            suffix.starts_with(literal)
                && suffix[literal.len()..].trim_start().starts_with(':')
        }) else {
            continue;
        };
        normalized.replace_range(start..start + literal.len(), &replacement);
        report.push(json!({
            "original_reference": reference,
            "normalized_reference": replacement,
            "part_kind": "logical_power_symbol",
        }));
    }
    report.reverse();
    (normalized, report)
}

fn add_power_reference_normalizations(report: &mut Value, normalizations: Vec<Value>) {
    if !normalizations.is_empty() {
        report["normalized_power_references"] = json!({
            "count": normalizations.len(),
            "items": normalizations,
        });
    }
}

fn normalize_misplaced_footprint_component_map(
    components: &mut serde_json::Map<String, Value>,
    ctx: &AgentRuntime,
) -> Result<Vec<Value>> {
    let catalog = ctx.footprint_catalog()?;
    let mut normalizations = Vec::new();
    for (reference, component) in components {
        let Some(fields) = component.as_object_mut() else {
            continue;
        };
        let Some(footprint) = fields.get("part").and_then(Value::as_str).map(str::to_owned) else {
            continue;
        };
        let Ok(id) = FootprintId::parse(&footprint) else {
            continue;
        };
        let Ok(metadata) = catalog.footprint(&id) else {
            continue;
        };
        let Some(symbol) = symbol_for_misplaced_footprint(&footprint, metadata.pads.len()) else {
            continue;
        };
        fields.insert("part".into(), json!(symbol));
        fields.insert("footprint".into(), json!(footprint));
        normalizations.push(json!({
            "reference": reference,
            "original_part": footprint,
            "inferred_symbol": symbol,
            "assigned_footprint": footprint,
        }));
    }
    Ok(normalizations)
}

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
    let (yaml, power_reference_normalizations) = normalize_invalid_power_references(&yaml);
    let (yaml, normalizations) = normalize_misplaced_footprint_parts(&yaml, ctx)?;
    let (yaml, footprint_alias_normalizations) =
        normalize_common_footprint_aliases(&yaml, ctx)?;
    let (yaml, default_footprint_normalizations) =
        normalize_dense_default_footprints(&yaml, ctx)?;
    let result = compile(&yaml, ctx.provider());
    let mut report = compile_authoring_report(&result, ctx)?;
    add_power_reference_normalizations(&mut report, power_reference_normalizations);
    add_footprint_part_normalizations(&mut report, normalizations);
    add_footprint_alias_normalizations(&mut report, footprint_alias_normalizations);
    add_default_footprint_normalizations(&mut report, default_footprint_normalizations);
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

fn available_authored_refs(ctx: &AgentRuntime) -> Vec<String> {
    let Some(design) = ctx
        .workspace()
        .read_draft()
        .ok()
        .flatten()
        .and_then(|yaml| compile(&yaml, ctx.provider()).design)
    else {
        return Vec::new();
    };
    let mut refs = design
        .blocks
        .values()
        .flat_map(|block| block.components.iter())
        .filter_map(|(reference, component)| {
            matches!(component.origin, Origin::Authored).then_some(reference.clone())
        })
        .collect::<Vec<_>>();
    refs.sort();
    refs
}

fn repair_components(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let block_name = input.get("block").and_then(Value::as_str).unwrap_or("main");
    if block_name.is_empty()
        || !block_name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return Ok(json!({
            "error": "block must be a non-empty lower_snake name",
            "code": "invalid_repair_block",
            "draft_written": false,
            "draft_changed": false,
            "mode": "component_repair",
        }));
    }
    // `components` is the advertised operation. Retain the older `upsert`
    // spelling for backward compatibility with saved/tool-replay histories.
    let upsert_input = input.get("upsert").or_else(|| input.get("components"));
    let mut upsert = match upsert_input {
        None => serde_json::Map::new(),
        Some(Value::Object(map)) => map.clone(),
        Some(_) => {
            return Ok(json!({
                "error": "components must be an object mapping refdes to component objects",
                "code": "invalid_repair_upsert",
                "draft_written": false,
                "draft_changed": false,
                "mode": "component_repair",
            }));
        }
    };
    if upsert.len() == 1
        && let Some(Value::Object(components)) = upsert.get("components")
    {
        upsert = components.clone();
    }
    if upsert.values().any(|component| !component.is_object()) {
        return Ok(json!({
            "error": "every components value must be a component object",
            "code": "invalid_repair_upsert",
            "draft_written": false,
            "draft_changed": false,
            "mode": "component_repair",
        }));
    }
    let footprint_normalizations =
        normalize_misplaced_footprint_component_map(&mut upsert, ctx)?;
    let mut footprint_alias_normalizations =
        normalize_common_footprint_aliases_in_component_map(&mut upsert, ctx)?;
    let mut update = match input.get("update") {
        None => serde_json::Map::new(),
        Some(Value::Object(map)) => map.clone(),
        Some(_) => {
            return Ok(json!({
                "error": "update must be an object mapping existing refdes to partial updates",
                "code": "invalid_repair_update",
                "draft_written": false,
                "draft_changed": false,
                "mode": "component_repair",
            }));
        }
    };
    footprint_alias_normalizations.extend(
        normalize_common_footprint_aliases_in_component_map(&mut update, ctx)?,
    );
    for (reference, fields) in &update {
        let Some(fields) = fields.as_object() else {
            return Ok(json!({
                "error": format!("update for {reference} must be an object"),
                "code": "invalid_repair_update",
                "draft_written": false,
                "draft_changed": false,
                "mode": "component_repair",
            }));
        };
        if fields.is_empty()
            || fields
                .keys()
                .any(|field| !matches!(field.as_str(), "pins" | "value" | "footprint"))
        {
            return Ok(json!({
                "error": format!("update for {reference} needs at least one of pins, value, or footprint and no other fields"),
                "code": "invalid_repair_update",
                "draft_written": false,
                "draft_changed": false,
                "mode": "component_repair",
            }));
        }
        if fields.get("value").is_some_and(|value| !value.is_string())
            || fields
                .get("footprint")
                .is_some_and(|footprint| !footprint.is_string())
        {
            return Ok(json!({
                "error": format!("value and footprint updates for {reference} must be strings"),
                "code": "invalid_repair_update",
                "draft_written": false,
                "draft_changed": false,
                "mode": "component_repair",
            }));
        }
        if let Some(pins) = fields.get("pins") {
            let Some(pins) = pins.as_object() else {
                return Ok(json!({
                    "error": format!("pins update for {reference} must be an object"),
                    "code": "invalid_repair_update",
                    "draft_written": false,
                    "draft_changed": false,
                    "mode": "component_repair",
                }));
            };
            if pins.is_empty() || pins.values().any(|target| !target.is_string()) {
                return Ok(json!({
                    "error": format!("pins update for {reference} must map at least one pin to a net string or nc"),
                    "code": "invalid_repair_update",
                    "draft_written": false,
                    "draft_changed": false,
                    "mode": "component_repair",
                }));
            }
        }
    }
    let remove = match input.get("remove") {
        None => Vec::new(),
        Some(Value::Array(items)) => {
            let Some(refs) = items.iter().map(Value::as_str).collect::<Option<Vec<_>>>() else {
                return Ok(json!({
                    "error": "remove must contain only refdes strings",
                    "code": "invalid_repair_remove",
                    "draft_written": false,
                    "draft_changed": false,
                    "mode": "component_repair",
                }));
            };
            refs.into_iter().map(str::to_string).collect::<Vec<_>>()
        }
        Some(_) => {
            return Ok(json!({
                "error": "remove must be an array of refdes strings",
                "code": "invalid_repair_remove",
                "draft_written": false,
                "draft_changed": false,
                "mode": "component_repair",
            }));
        }
    };
    let remove_set = remove
        .iter()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    if remove_set.len() != remove.len() {
        return Ok(json!({
            "error": "remove contains a duplicate refdes",
            "code": "duplicate_repair_remove",
            "draft_written": false,
            "draft_changed": false,
            "mode": "component_repair",
        }));
    }
    if upsert
        .keys()
        .chain(update.keys())
        .any(|reference| remove_set.contains(reference))
        || upsert
            .keys()
            .any(|reference| update.contains_key(reference))
    {
        return Ok(json!({
            "error": "the same refdes cannot appear in more than one of components, update, or remove",
            "code": "conflicting_repair_operation",
            "draft_written": false,
            "draft_changed": false,
            "mode": "component_repair",
        }));
    }
    if upsert.is_empty() && update.is_empty() && remove.is_empty() {
        let available_authored_refs = available_authored_refs(ctx);
        return Ok(json!({
            "error": "repair requires at least one components, update, or remove refdes",
            "code": "empty_component_repair",
            "available_authored_refs": available_authored_refs,
            "example": {"components": {"C1": {"part": "Device:C", "value": "100nF", "pins": {"1": "+5V", "2": "GND"}}}},
            "draft_written": false,
            "draft_changed": false,
            "mode": "component_repair",
        }));
    }
    let replace_existing = input
        .get("replace_existing")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let Some(prior_yaml) = ctx.workspace().read_draft()? else {
        return Ok(json!({
            "error": "no durable draft exists; create a complete valid draft before component repair",
            "code": "repair_requires_draft",
            "draft_written": false,
            "draft_changed": false,
            "mode": "component_repair",
        }));
    };
    let prior_result = compile(&prior_yaml, ctx.provider());
    let mut prior_report = compile_authoring_report(&prior_result, ctx)?;
    let Some(prior_design) = prior_result.design.as_ref() else {
        prior_report["error"] = json!(
            "the current draft is invalid; component repair cannot safely preserve malformed content"
        );
        prior_report["code"] = json!("repair_requires_valid_draft");
        prior_report["draft_written"] = json!(false);
        prior_report["draft_changed"] = json!(false);
        prior_report["mode"] = json!("component_repair");
        return Ok(prior_report);
    };
    let fragment_yaml = format!(
        "version: 1\nblocks:\n  patch:\n    components: {}\n",
        serde_json::to_string(&upsert)?
    );
    let patch_result = compile(&fragment_yaml, ctx.provider());
    let Some(mut patch_design) = patch_result.design else {
        let mut report = compile_report(&patch_result.diagnostics);
        add_footprint_part_normalizations(&mut report, footprint_normalizations);
        add_footprint_alias_normalizations(&mut report, footprint_alias_normalizations);
        report["error"] = json!("component repair fragment is invalid");
        report["code"] = json!("invalid_component_repair");
        report["draft_written"] = json!(false);
        report["draft_changed"] = json!(false);
        report["mode"] = json!("component_repair");
        report["current_design_state"] = design_state_summary(prior_design);
        report["available_authored_refs"] = json!(available_authored_refs(ctx));
        report["expected_shape"] = json!({
            "components": {"NEW_REF": {"part": "Lib:Symbol", "pins": {"pin": "NET"}}},
            "update": {"EXISTING_REF": {"pins": {"pin": "NET"}}}
        });
        return Ok(report);
    };
    let patch_block = patch_design
        .blocks
        .shift_remove("patch")
        .expect("repair wrapper always creates patch block");
    let authored_patch_refs = patch_block
        .components
        .iter()
        .filter_map(|(reference, component)| {
            matches!(component.origin, Origin::Authored).then_some(reference.as_str())
        })
        .collect::<std::collections::BTreeSet<_>>();
    if authored_patch_refs.len() != upsert.len()
        || upsert
            .keys()
            .any(|reference| !authored_patch_refs.contains(reference.as_str()))
    {
        return Ok(json!({
            "error": "components must contain only explicit authored refdes entries",
            "code": "invalid_component_repair_refs",
            "draft_written": false,
            "draft_changed": false,
            "mode": "component_repair",
            "current_design_state": design_state_summary(prior_design),
        }));
    }

    let existing = prior_design
        .blocks
        .iter()
        .flat_map(|(block, contents)| {
            contents
                .components
                .iter()
                .map(move |(reference, component)| {
                    (reference.clone(), (block.clone(), component.origin.clone()))
                })
        })
        .collect::<std::collections::HashMap<_, _>>();
    for reference in &remove {
        let Some((_existing_block, origin)) = existing.get(reference) else {
            return Ok(json!({
                "error": format!("remove refdes {reference} does not exist"),
                "code": "unknown_repair_remove",
                "draft_written": false,
                "draft_changed": false,
                "mode": "component_repair",
                "current_design_state": design_state_summary(prior_design),
            }));
        };
        if !matches!(origin, Origin::Authored) {
            return Ok(json!({
                "error": format!("{reference} is synthesized; remove or replace its authored parent instead"),
                "code": "synthesized_component_repair_forbidden",
                "draft_written": false,
                "draft_changed": false,
                "mode": "component_repair",
                "current_design_state": design_state_summary(prior_design),
            }));
        }
    }

    let mut added = Vec::new();
    let mut replaced = Vec::new();
    for reference in upsert.keys() {
        match existing.get(reference) {
            None => added.push(reference.clone()),
            Some((existing_block, origin)) => {
                if !matches!(origin, Origin::Authored) {
                    return Ok(json!({
                        "error": format!("{reference} is synthesized; replace its authored parent instead"),
                        "code": "synthesized_component_repair_forbidden",
                        "draft_written": false,
                        "draft_changed": false,
                        "mode": "component_repair",
                        "current_design_state": design_state_summary(prior_design),
                    }));
                }
                let previous = &prior_design.blocks[existing_block].components[reference];
                let replacement = &patch_block.components[reference];
                if !replace_existing && previous.part != replacement.part {
                    return Ok(json!({
                        "error": format!("components changes {reference} from {} to {}; pass replace_existing=true to confirm the symbol change", previous.part, replacement.part),
                        "code": "component_replacement_requires_confirmation",
                        "draft_written": false,
                        "draft_changed": false,
                        "mode": "component_repair",
                        "current_design_state": design_state_summary(prior_design),
                    }));
                }
                replaced.push(reference.clone());
            }
        }
    }
    let mut updated = Vec::new();
    for reference in update.keys() {
        let Some((existing_block, origin)) = existing.get(reference) else {
            return Ok(json!({
                "error": format!("update refdes {reference} does not exist; use components with a complete component object to add it"),
                "code": "unknown_repair_update",
                "draft_written": false,
                "draft_changed": false,
                "mode": "component_repair",
                "current_design_state": design_state_summary(prior_design),
            }));
        };
        if !matches!(origin, Origin::Authored) {
            return Ok(json!({
                "error": format!("{reference} is synthesized; update its authored parent instead"),
                "code": "synthesized_component_repair_forbidden",
                "draft_written": false,
                "draft_changed": false,
                "mode": "component_repair",
                "current_design_state": design_state_summary(prior_design),
            }));
        }
        let component = &prior_design.blocks[existing_block].components[reference];
        if let Some(pins) = update[reference].get("pins").and_then(Value::as_object) {
            let unknown = pins
                .keys()
                .filter(|pin| !component.pins.contains_key(*pin))
                .cloned()
                .collect::<Vec<_>>();
            if !unknown.is_empty() {
                let mut valid = component.pins.keys().cloned().collect::<Vec<_>>();
                valid.sort();
                return Ok(json!({
                    "error": format!("update for {reference} uses pin key(s) not present in the current draft: {}", unknown.join(", ")),
                    "code": "unknown_repair_pin_key",
                    "reference": reference,
                    "unknown_pin_keys": unknown,
                    "valid_pin_keys": valid,
                    "draft_written": false,
                    "draft_changed": false,
                    "mode": "component_repair",
                    "current_design_state": design_state_summary(prior_design),
                }));
            }
        }
        updated.push(reference.clone());
    }

    let mut candidate = prior_design.clone();
    for reference in remove.iter().chain(replaced.iter()) {
        let owning_block = &existing
            .get(reference)
            .expect("removed and replaced refs were preflighted")
            .0;
        let target = candidate
            .blocks
            .get_mut(owning_block)
            .expect("owning block exists in candidate");
        target.components.shift_remove(reference);
        target.components.retain(|_, component| {
            !matches!(
                &component.origin,
                Origin::Synthesized { parent, .. } if parent == reference
            )
        });
    }
    for reference in &remove {
        let owning_block = &existing
            .get(reference)
            .expect("removed refs were preflighted")
            .0;
        if let Some(target) = candidate.blocks.get_mut(owning_block) {
            for row in &mut target.layout {
                for cell in row {
                    if cell.as_ref() == Some(reference) {
                        *cell = None;
                    }
                }
            }
        }
    }
    let mut synth_index = 0usize;
    for (reference, mut component) in patch_block.components {
        let parent_reference = match &component.origin {
            Origin::Authored => reference.as_str(),
            Origin::Synthesized { parent, .. } => parent.as_str(),
        };
        let destination_block = existing
            .get(parent_reference)
            .map(|(block, _)| block.as_str())
            .unwrap_or(block_name);
        match &component.origin {
            Origin::Authored => {
                if let Some(previous) = prior_design
                    .blocks
                    .get(destination_block)
                    .and_then(|block| block.components.get(&reference))
                {
                    let fields = upsert[&reference]
                        .as_object()
                        .expect("upsert values were validated as objects");
                    if !fields.contains_key("value") {
                        component.value.clone_from(&previous.value);
                    }
                    if !fields.contains_key("footprint") {
                        component.footprint.clone_from(&previous.footprint);
                    }
                    if !fields.contains_key("dnp") {
                        component.dnp = previous.dnp;
                    }
                    if !fields.contains_key("props") {
                        component.props.clone_from(&previous.props);
                    }
                    let supplies_topology = [
                        "pins", "units", "between", "positive", "negative", "decouple",
                    ]
                    .iter()
                    .any(|field| fields.contains_key(*field));
                    if !supplies_topology {
                        component.pins.clone_from(&previous.pins);
                        component.units.clone_from(&previous.units);
                    }
                }
                let target = candidate
                    .blocks
                    .entry(destination_block.to_string())
                    .or_insert_with(Block::default);
                target.components.insert(reference, component);
            }
            Origin::Synthesized { .. } => {
                let target = candidate
                    .blocks
                    .entry(destination_block.to_string())
                    .or_insert_with(Block::default);
                let key = loop {
                    let key = format!("__repair_synth_{synth_index}");
                    synth_index += 1;
                    if !target.components.contains_key(&key) {
                        break key;
                    }
                };
                target.components.insert(key, component);
            }
        }
    }
    for (reference, fields) in &update {
        let owning_block = &existing
            .get(reference)
            .expect("update refs were preflighted")
            .0;
        let component = candidate
            .blocks
            .get_mut(owning_block)
            .expect("owning block exists in candidate")
            .components
            .get_mut(reference)
            .expect("update refs were preflighted in the target block");
        let fields = fields
            .as_object()
            .expect("update values were validated as objects");
        if let Some(value) = fields.get("value").and_then(Value::as_str) {
            component.value = Some(value.to_string());
        }
        if let Some(footprint) = fields.get("footprint").and_then(Value::as_str) {
            component.footprint = Some(footprint.to_string());
        }
        if let Some(pins) = fields.get("pins").and_then(Value::as_object) {
            for (pin, target) in pins {
                let target = target
                    .as_str()
                    .expect("pin targets were validated as strings");
                let target = if target.eq_ignore_ascii_case("nc") {
                    PinTarget::NoConnect
                } else {
                    PinTarget::Net(target.to_string())
                };
                component.pins.insert(pin.clone(), target);
            }
        }
    }

    let candidate_yaml = circuit_lang::canon::to_canonical_yaml(&candidate);
    let candidate_result = compile(&candidate_yaml, ctx.provider());
    let mut report = compile_authoring_report(&candidate_result, ctx)?;
    add_footprint_part_normalizations(&mut report, footprint_normalizations);
    add_footprint_alias_normalizations(&mut report, footprint_alias_normalizations);
    let candidate_is_clean = report.get("ok").and_then(Value::as_bool) == Some(true);
    let prior_is_invalid = prior_report.get("ok").and_then(Value::as_bool) != Some(true);
    let candidate_strictly_improves = prior_is_invalid
        && authoring_report_quality(&report) < authoring_report_quality(&prior_report);
    if !candidate_is_clean && !candidate_strictly_improves {
        let current_validation = prior_report.clone();
        let candidate_validation = report.clone();
        report["error"] = json!(
            "component repair did not improve the complete draft's validation; the existing draft was preserved"
        );
        report["code"] = json!("invalid_component_repair_preserved_draft");
        report["current_validation"] = current_validation;
        report["candidate_validation"] = candidate_validation;
        report["draft_written"] = json!(false);
        report["draft_changed"] = json!(false);
        report["electrical_design_changed"] = json!(false);
        report["mode"] = json!("component_repair");
        report["current_design_state"] = design_state_summary(prior_design);
        return Ok(report);
    }

    ctx.workspace()
        .write_draft(&candidate_yaml, current_sch_text(ctx).as_deref())?;
    report["mode"] = json!("component_repair");
    report["added"] = json!(added);
    report["replaced"] = json!(replaced);
    report["updated"] = json!(updated);
    report["removed"] = json!(remove);
    report["draft_written"] = json!(true);
    report["draft_changed"] = json!(candidate_yaml != prior_yaml);
    report["electrical_design_changed"] = json!(electrical_yaml_changed(
        Some(&prior_yaml),
        &candidate_yaml,
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
        let (yaml, power_reference_normalizations) = normalize_invalid_power_references(yaml);
        let (yaml, normalizations) = normalize_misplaced_footprint_parts(&yaml, ctx)?;
        let (yaml, footprint_alias_normalizations) =
            normalize_common_footprint_aliases(&yaml, ctx)?;
        let (yaml, default_footprint_normalizations) =
            normalize_dense_default_footprints(&yaml, ctx)?;
        let result = compile(&yaml, ctx.provider());
        let mut report = compile_authoring_report(&result, ctx)?;
        add_power_reference_normalizations(&mut report, power_reference_normalizations);
        add_footprint_part_normalizations(&mut report, normalizations);
        add_footprint_alias_normalizations(&mut report, footprint_alias_normalizations);
        add_default_footprint_normalizations(&mut report, default_footprint_normalizations);
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
        if let Some(prior) = prior_result.as_ref()
            && invalid_compile_quality_regressed(&prior.diagnostics, &result.diagnostics)
        {
            let current_validation = compile_report(&prior.diagnostics);
            let candidate_validation = compile_report(&result.diagnostics);
            report["ok"] = json!(false);
            report["error"] = json!(
                "full replacement regressed an invalid draft; the better repair anchor was preserved. Top-level diagnostic examples describe the rejected candidate; preserved-draft diagnostics are in current_validation"
            );
            report["code"] = json!("invalid_replacement_regressed_draft");
            report["diagnostics_scope"] = json!("rejected_candidate");
            report["current_validation"] = current_validation;
            report["candidate_validation"] = candidate_validation;
            report["draft_written"] = json!(false);
            report["draft_changed"] = json!(false);
            report["electrical_design_changed"] = json!(false);
            report["mode"] = json!("full_replace");
            report["next_tool"] = json!("edit_design");
            report["next"] = json!(
                "fix the rejected candidate diagnostics and resend the complete yaml; replacements with fewer errors, or equal errors and no more warnings, remain accepted"
            );
            return Ok(report);
        }
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
            .write_draft(&yaml, current_sch_text(ctx).as_deref())?;
        report["draft_written"] = json!(true);
        report["draft_changed"] = json!(prior_draft.as_deref() != Some(yaml.as_str()));
        report["electrical_design_changed"] = json!(electrical_yaml_changed(
            prior_draft.as_deref(),
            &yaml,
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

    let (Some(old), Some(new)) = (
        input.get("old_string").and_then(Value::as_str),
        input.get("new_string").and_then(Value::as_str),
    ) else {
        return Ok(json!({
            "ok": false,
            "error": "edit_design requires one complete corrected YAML document in `yaml`; an incomplete old_string/new_string patch cannot be applied safely",
            "code": "edit_design_full_yaml_required",
            "draft_changed": false,
            "electrical_design_changed": false,
            "next_tool": "edit_design",
            "next": "resend the complete current draft with the correction applied as edit_design({yaml: ...})",
        }));
    };
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

#[cfg(test)]
mod tests {
    use crate::AgentRuntime;
    use serde_json::json;

    use super::{
        common_default_footprint, common_footprint_alias, compile, compile_report, create_design,
        edit_design, invalid_compile_quality_regressed, normalize_common_footprint_aliases,
        normalize_dense_default_footprints, normalize_invalid_power_references,
        normalize_misplaced_footprint_parts, repair_components, repair_components_tool,
        require_search_query, symbol_for_misplaced_footprint, tool_defs,
    };

    #[test]
    fn common_defaults_cover_only_canonical_package_families() {
        assert_eq!(
            common_default_footprint("Device:R").as_deref(),
            Some("Resistor_SMD:R_0603_1608Metric")
        );
        assert_eq!(
            common_default_footprint("Connector_Generic:Conn_02x05_Odd_Even").as_deref(),
            Some("Connector_PinHeader_2.54mm:PinHeader_2x05_P2.54mm_Vertical")
        );
        assert_eq!(
            common_default_footprint("Connector:Conn_01x03_Pin").as_deref(),
            Some("Connector_PinHeader_2.54mm:PinHeader_1x03_P2.54mm_Vertical")
        );
        assert_eq!(common_default_footprint("Device:C_Polarized"), None);
        assert_eq!(
            common_default_footprint("Transistor_FET:Q_NMOS_DGS"),
            None
        );
        assert_eq!(common_default_footprint("Amplifier_Operational:LM358"), None);
    }

    #[test]
    fn common_aliases_are_exact_and_never_guess_active_packages() {
        assert_eq!(
            common_footprint_alias("Resistor_SMD:R_0603"),
            Some("Resistor_SMD:R_0603_1608Metric")
        );
        assert_eq!(
            common_footprint_alias("Capacitor_SMD:C_0603"),
            Some("Capacitor_SMD:C_0603_1608Metric")
        );
        assert_eq!(
            common_footprint_alias("LED_SMD:LED_0603"),
            Some("LED_SMD:LED_0603_1608Metric")
        );
        assert_eq!(
            common_footprint_alias("Diode_SMD:SOD-123"),
            Some("Diode_SMD:D_SOD-123")
        );
        assert_eq!(
            common_footprint_alias("Connector:PinHeader_1x08_P2.54mm_Vertical"),
            Some("Connector_PinHeader_2.54mm:PinHeader_1x08_P2.54mm_Vertical")
        );
        assert_eq!(
            common_footprint_alias("MountingHole:MountingHole_M3"),
            Some("MountingHole:MountingHole_3.2mm_M3")
        );
        assert_eq!(
            common_footprint_alias("Capacitor_THT:CP_Radial_D8.0mm_P3.50mm_P7.5mm"),
            None
        );
        assert_eq!(common_footprint_alias("Package_TO_SOT_SMD:SOT23"), None);
        assert_eq!(common_footprint_alias("Custom:R_0603"), None);
    }

    #[test]
    fn footprint_alias_normalization_requires_a_catalog_verified_target() {
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../kicad-footprint/tests/fixtures/footprints/R_0603_1608Metric.kicad_mod");
        let footprints = tempfile::tempdir().unwrap();
        let pretty = footprints.path().join("Resistor_SMD.pretty");
        std::fs::create_dir_all(&pretty).unwrap();
        std::fs::copy(source, pretty.join("R_0603_1608Metric.kicad_mod")).unwrap();
        let runtime =
            AgentRuntime::with_footprint_dir_for_test(footprints.path().to_path_buf()).unwrap();
        let yaml = r#"
version: 1
blocks:
  main:
    components:
      R1: {part: Device:R, footprint: "Resistor_SMD:R_0603", between: [A, B]}
      C1: {part: Device:C, footprint: "Capacitor_SMD:C_0603", between: [A, B]}
      Q1: {part: Transistor_FET:Q_NMOS_GSD, footprint: "Package_TO_SOT_SMD:SOT23", pins: {G: A, S: B, D: C}}
"#;

        let (normalized, changes) = normalize_common_footprint_aliases(yaml, &runtime).unwrap();

        assert_eq!(changes.len(), 1);
        assert!(normalized.contains("Resistor_SMD:R_0603_1608Metric"));
        assert!(normalized.contains("Capacitor_SMD:C_0603\""));
        assert!(normalized.contains("Package_TO_SOT_SMD:SOT23"));
    }

    #[test]
    fn footprint_aliases_persist_canonically_across_authoring_paths() {
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../kicad-footprint/tests/fixtures/footprints/R_0603_1608Metric.kicad_mod");
        let footprints = tempfile::tempdir().unwrap();
        let pretty = footprints.path().join("Resistor_SMD.pretty");
        std::fs::create_dir_all(&pretty).unwrap();
        std::fs::copy(source, pretty.join("R_0603_1608Metric.kicad_mod")).unwrap();
        let runtime =
            AgentRuntime::with_footprint_dir_for_test(footprints.path().to_path_buf()).unwrap();
        let yaml = r#"
version: 1
blocks:
  main:
    components:
      R1: {part: Device:R, footprint: "Resistor_SMD:R_0603", between: [A, B]}
"#;

        let created = create_design(json!({"yaml": yaml}), &runtime).unwrap();
        assert_eq!(created["normalized_footprint_aliases"]["count"], 1);
        assert!(runtime
            .workspace()
            .read_draft()
            .unwrap()
            .unwrap()
            .contains("Resistor_SMD:R_0603_1608Metric"));

        let edited = edit_design(json!({"yaml": yaml}), &runtime).unwrap();
        assert_eq!(edited["normalized_footprint_aliases"]["count"], 1);

        let repaired = repair_components(
            json!({
                "update": {"R1": {"footprint": "Resistor_SMD:R_0603"}},
                "components": {
                    "R2": {
                        "part": "Device:R",
                        "footprint": "Resistor_SMD:R_0603",
                        "pins": {"1": "A", "2": "B"}
                    }
                }
            }),
            &runtime,
        )
        .unwrap();
        assert_eq!(repaired["normalized_footprint_aliases"]["count"], 2);
        assert_eq!(repaired["draft_written"], true);
        let draft = runtime.workspace().read_draft().unwrap().unwrap();
        assert_eq!(draft.matches("Resistor_SMD:R_0603_1608Metric").count(), 2);
        assert!(!draft.contains("footprint: Resistor_SMD:R_0603\n"));
    }

    #[test]
    fn dense_defaulting_fills_missing_footprints_but_preserves_explicit_choices() {
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../kicad-footprint/tests/fixtures/footprints/R_0603_1608Metric.kicad_mod");
        let footprints = tempfile::tempdir().unwrap();
        let pretty = footprints.path().join("Resistor_SMD.pretty");
        std::fs::create_dir_all(&pretty).unwrap();
        std::fs::copy(source, pretty.join("R_0603_1608Metric.kicad_mod")).unwrap();
        let runtime =
            AgentRuntime::with_footprint_dir_for_test(footprints.path().to_path_buf()).unwrap();

        let components = (1..=40)
            .map(|n| {
                if n == 1 {
                    format!(
                        "      R1: {{part: Device:R, footprint: Custom:R1, between: [N1, GND]}}"
                    )
                } else {
                    format!("      R{n}: {{part: Device:R, between: [N{n}, GND]}}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let yaml = format!("version: 1\nblocks:\n  main:\n    components:\n{components}\n");

        let (normalized, changes) =
            normalize_dense_default_footprints(&yaml, &runtime).unwrap();

        assert_eq!(changes.len(), 39);
        assert!(normalized.contains("R1: {part: Device:R, footprint: Custom:R1"));
        assert!(normalized.contains(
            "R40: {part: Device:R, between: [N40, GND], footprint: \"Resistor_SMD:R_0603_1608Metric\"}"
        ));

        let sparse = yaml.replace(
            "      R40: {part: Device:R, between: [N40, GND]}\n",
            "",
        );
        let (sparse, sparse_changes) =
            normalize_dense_default_footprints(&sparse, &runtime).unwrap();
        assert!(sparse_changes.is_empty());
        assert!(!sparse.contains("R_0603_1608Metric\"}"));
    }

    #[test]
    fn invalid_power_symbol_references_are_normalized_without_touching_physical_refs() {
        let yaml = r#"
version: 1
blocks:
  power:
    components:
      PWR1: {part: power:GND, pins: {1: GND}}
      V3V31: {part: power:+3V3, pins: {1: V3V3}}
      "V1V81": {part: power:+1V8, pins: {1: V1V8}}
      R_BAD1: {part: Device:R, between: [V3V3, V1V8]}
"#;

        let (normalized, changes) = normalize_invalid_power_references(yaml);

        assert!(normalized.contains("PWR2: {part: power:+3V3"), "{normalized}");
        assert!(normalized.contains("PWR3: {part: power:+1V8"), "{normalized}");
        assert!(normalized.contains("R_BAD1: {part: Device:R"), "{normalized}");
        assert_eq!(changes.len(), 2);
        let (_, diagnostics) = circuit_lang::parse::parse_str(&normalized);
        let bad = diagnostics
            .0
            .iter()
            .filter(|diagnostic| diagnostic.code == "bad-refdes")
            .collect::<Vec<_>>();
        assert_eq!(bad.len(), 1, "{diagnostics:?}");
        assert!(bad[0].message.contains("R_BAD1"));
    }

    #[test]
    fn malformed_legacy_edit_returns_a_structured_full_yaml_retry() {
        let footprints = tempfile::tempdir().unwrap();
        let runtime = AgentRuntime::with_footprint_dir_for_test(footprints.path().to_path_buf())
            .expect("test runtime");
        let draft = "version: 1\nblocks: {main: {components: {R1: {part: Device:R, between: [A, B]}}}}\n";
        runtime.workspace().write_draft(draft, None).unwrap();

        let report = edit_design(json!({"new_string": "4.7k"}), &runtime).unwrap();

        assert_eq!(report["code"], "edit_design_full_yaml_required");
        assert_eq!(report["draft_changed"], false);
        assert_eq!(report["next_tool"], "edit_design");
        assert_eq!(runtime.workspace().read_draft().unwrap().unwrap(), draft);
    }

    #[test]
    fn full_edit_persists_candidate_after_power_reference_recovery() {
        let footprints = tempfile::tempdir().unwrap();
        let runtime = AgentRuntime::with_footprint_dir_for_test(footprints.path().to_path_buf())
            .expect("test runtime");
        let prior = "version: 1\nblocks: {main: {components: {R1: {part: Device:R, between: [A, B]}}}}\n";
        runtime.workspace().write_draft(prior, None).unwrap();
        let candidate = r#"
version: 1
blocks:
  main:
    components:
      R1: {part: Device:R, between: [A, B]}
      V3V31: {part: power:+3V3, pins: {1: V3V3}}
"#;

        let report = edit_design(json!({"yaml": candidate}), &runtime).unwrap();

        assert_eq!(report["draft_written"], true, "{report}");
        assert_eq!(report["normalized_power_references"]["count"], 1);
        let draft = runtime.workspace().read_draft().unwrap().unwrap();
        assert!(draft.contains("PWR1: {part: power:+3V3"), "{draft}");
        assert!(!draft.contains("V3V31:"), "{draft}");
    }

    #[test]
    fn misplaced_footprint_inference_is_limited_to_safe_families() {
        for (footprint, pads, symbol) in [
            ("Resistor_SMD:R_0603", 2, "Device:R"),
            ("Capacitor_THT:C_Disc", 2, "Device:C"),
            ("Inductor_SMD:L_0603", 2, "Device:L"),
            ("LED_SMD:LED_0603", 2, "Device:LED"),
            ("Diode_SMD:D_SOD-123", 2, "Device:D"),
            ("MountingHole:MountingHole_3.2mm", 0, "Mechanical:MountingHole"),
            ("TestPoint:TestPoint_Pad", 1, "Connector:TestPoint"),
            (
                "Connector_PinHeader_2.54mm:PinHeader_1x08_Vertical",
                8,
                "Connector_Generic:Conn_01x08",
            ),
        ] {
            assert_eq!(
                symbol_for_misplaced_footprint(footprint, pads).as_deref(),
                Some(symbol),
                "{footprint}"
            );
        }
        assert_eq!(symbol_for_misplaced_footprint("Package_QFP:LQFP-48", 48), None);
        assert_eq!(symbol_for_misplaced_footprint("Package_TO_SOT_SMD:SOT-23", 3), None);
        assert_eq!(
            symbol_for_misplaced_footprint(
                "Connector_PinHeader_2.54mm:PinHeader_2x04_Vertical",
                8
            ),
            None
        );
    }

    #[test]
    fn create_and_edit_persist_misplaced_footprint_recovery() {
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../kicad-footprint/tests/fixtures/footprints/R_0603_1608Metric.kicad_mod");
        let footprints = tempfile::tempdir().unwrap();
        let pretty = footprints.path().join("Resistor_SMD.pretty");
        std::fs::create_dir_all(&pretty).unwrap();
        std::fs::copy(&source, pretty.join("R_0603_1608Metric.kicad_mod")).unwrap();
        let runtime =
            AgentRuntime::with_footprint_dir_for_test(footprints.path().to_path_buf()).unwrap();
        let misplaced = "Resistor_SMD:R_0603_1608Metric";
        let components = (1..=10)
            .map(|index| {
                format!(
                    "      R{index}:\n        part: {misplaced}\n        between: [A{index}, B{index}]"
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let multiline = format!(
            "version: 1\nblocks:\n  main:\n    components:\n{components}\n"
        );

        let created = create_design(json!({"yaml": multiline}), &runtime).unwrap();
        assert_eq!(created["normalized_misplaced_footprints"]["count"], 10);
        assert_eq!(created["normalized_misplaced_footprints"]["examples"].as_array().unwrap().len(), 8);
        assert_eq!(created["normalized_misplaced_footprints"]["examples"][0]["reference"], "R1");
        assert_eq!(created["normalized_misplaced_footprints"]["omitted"], 2);
        assert_eq!(created["draft_written"], true);
        let draft = runtime.workspace().read_draft().unwrap().unwrap();
        assert!(draft.contains("part: \"Device:R\""));
        assert!(draft.contains(&format!("footprint: \"{misplaced}\"")));

        let inline_components = (1..=10)
            .map(|index| {
                format!(
                    "R{index}: {{part: {misplaced}, between: [A{index}, C{index}]}}"
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let inline = format!(
            "version: 1\nblocks: {{main: {{components: {{{inline_components}}}}}}}\n"
        );
        let edited = edit_design(json!({"yaml": inline}), &runtime).unwrap();
        assert_eq!(edited["normalized_misplaced_footprints"]["count"], 10);
        assert_eq!(edited["normalized_misplaced_footprints"]["examples"].as_array().unwrap().len(), 8);
        assert_eq!(edited["normalized_misplaced_footprints"]["examples"][0]["reference"], "R1");
        assert_eq!(edited["normalized_misplaced_footprints"]["omitted"], 2);
        assert_eq!(edited["draft_written"], true);
        let draft = runtime.workspace().read_draft().unwrap().unwrap();
        assert!(draft.contains("part: \"Device:R\""));
        assert!(draft.contains(&format!("footprint: \"{misplaced}\"")));

        let repaired = repair_components(
            json!({
                "block": "main",
                "components": {
                    "R2": {"part": misplaced, "between": ["C", "D"]}
                }
            }),
            &runtime,
        )
        .unwrap();
        assert_eq!(repaired["normalized_misplaced_footprints"]["count"], 1);
        assert_eq!(repaired["draft_written"], true, "{repaired}");
        let draft = runtime.workspace().read_draft().unwrap().unwrap();
        assert!(draft.contains("R2:"));
        assert_eq!(
            draft.matches("part: Device:R").count()
                + draft.matches("part: \"Device:R\"").count(),
            10
        );
        assert_eq!(draft.matches(misplaced).count(), 10);
    }

    #[test]
    fn misplaced_footprints_normalize_safe_inline_and_multiline_families_only() {
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../kicad-footprint/tests/fixtures/footprints");
        let footprints = tempfile::tempdir().unwrap();
        for (library, file) in [
            ("Resistor_SMD", "R_0603_1608Metric.kicad_mod"),
            (
                "Connector_PinHeader_2.54mm",
                "PinHeader_1x02_P2.54mm_Vertical.kicad_mod",
            ),
            ("Package_TO_SOT_SMD", "SOT-23.kicad_mod"),
        ] {
            let pretty = footprints.path().join(format!("{library}.pretty"));
            std::fs::create_dir_all(&pretty).unwrap();
            std::fs::copy(source.join(file), pretty.join(file)).unwrap();
        }
        let runtime =
            AgentRuntime::with_footprint_dir_for_test(footprints.path().to_path_buf()).unwrap();
        let yaml = r#"version: 1
blocks:
  main:
    components:
      R1: {part: Resistor_SMD:R_0603_1608Metric, between: [A, B]}
      J1:
        part: Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical
        pins: {1: A, 2: B}
      U1: {part: Package_TO_SOT_SMD:SOT-23, pins: {1: A, 2: B, 3: C}}
"#;

        let (normalized, changes) =
            normalize_misplaced_footprint_parts(yaml, &runtime).unwrap();

        assert!(normalized.contains(
            "R1: {part: \"Device:R\", between: [A, B], footprint: \"Resistor_SMD:R_0603_1608Metric\"}"
        ));
        assert!(normalized.contains("        part: \"Connector_Generic:Conn_01x02\""));
        assert!(normalized.contains(
            "        footprint: \"Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical\""
        ));
        assert!(normalized.contains("U1: {part: Package_TO_SOT_SMD:SOT-23"));
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0]["reference"], "R1");
        assert_eq!(changes[1]["reference"], "J1");
    }

    fn invalid_refdes_draft(count: usize) -> String {
        let components = (1..=count)
            .map(|index| {
                format!("R_BAD{index}: {{part: Device:R, between: [A, B]}}")
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("version: 1\nblocks: {{main: {{components: {{{components}}}}}}}\n")
    }

    #[test]
    fn worse_invalid_full_replacement_preserves_the_better_draft() {
        let footprints = tempfile::tempdir().unwrap();
        let runtime = AgentRuntime::with_footprint_dir_for_test(footprints.path().to_path_buf())
            .expect("test runtime");
        let prior = invalid_refdes_draft(2);
        let worse = invalid_refdes_draft(3);
        runtime.workspace().write_draft(&prior, None).unwrap();

        let report = edit_design(json!({"yaml": worse}), &runtime).unwrap();

        assert_eq!(report["code"], "invalid_replacement_regressed_draft");
        assert_eq!(report["current_validation"]["errors"], 2);
        assert_eq!(report["candidate_validation"]["errors"], 3);
        assert_eq!(report["diagnostics_scope"], "rejected_candidate");
        assert!(report["error"].as_str().unwrap().contains("current_validation"));
        assert_eq!(report["draft_written"], false);
        assert_eq!(report["draft_changed"], false);
        assert_eq!(runtime.workspace().read_draft().unwrap().unwrap(), prior);
    }

    #[test]
    fn improved_invalid_full_replacement_is_written() {
        let footprints = tempfile::tempdir().unwrap();
        let runtime = AgentRuntime::with_footprint_dir_for_test(footprints.path().to_path_buf())
            .expect("test runtime");
        let prior = invalid_refdes_draft(3);
        let better = invalid_refdes_draft(2);
        runtime.workspace().write_draft(&prior, None).unwrap();

        let report = edit_design(
            json!({"yaml": better, "allow_component_removal": true}),
            &runtime,
        )
        .unwrap();

        assert_ne!(report.get("code"), Some(&json!("invalid_replacement_regressed_draft")));
        assert_eq!(report["errors"], 2);
        assert_eq!(report["draft_written"], true);
        assert_eq!(runtime.workspace().read_draft().unwrap().unwrap(), better);
    }

    #[test]
    fn improving_footprint_repair_of_parseable_invalid_draft_is_written() {
        let footprints = tempfile::tempdir().unwrap();
        let runtime = AgentRuntime::with_footprint_dir_for_test(footprints.path().to_path_buf())
            .expect("test runtime");
        let prior = "version: 1\nblocks: {main: {components: {R1: {part: Device:R, footprint: Missing:One, between: [A, B]}, R2: {part: Device:R, footprint: Missing:Two, between: [A, B]}}}}\n";
        runtime.workspace().write_draft(&prior, None).unwrap();

        let report = repair_components(json!({"remove": ["R1"]}), &runtime).unwrap();

        assert_eq!(report["errors"], 1, "{report}");
        assert_eq!(report["draft_written"], true, "{report}");
        let repaired = runtime.workspace().read_draft().unwrap().unwrap();
        assert!(!repaired.contains("R1:"), "{repaired}");
        assert!(repaired.contains("R2:"), "{repaired}");
    }

    #[test]
    fn repair_routes_existing_refs_across_blocks_in_one_batch() {
        let footprints = tempfile::tempdir().unwrap();
        let runtime = AgentRuntime::with_footprint_dir_for_test(footprints.path().to_path_buf())
            .expect("test runtime");
        let prior = r#"version: 1
blocks:
  bank_1:
    components:
      R1: {part: Device:R, value: old-1, between: [A, B]}
  bank_2:
    components:
      R2: {part: Device:R, value: old-2, between: [A, B]}
"#;
        runtime.workspace().write_draft(prior, None).unwrap();

        // A stale destination block must not reject or misroute existing refs.
        let report = repair_components(
            json!({
                "block": "main",
                "update": {
                    "R1": {"value": "new-1"},
                    "R2": {"value": "new-2"}
                }
            }),
            &runtime,
        )
        .unwrap();

        assert_eq!(report["draft_written"], true, "{report}");
        assert_eq!(report["updated"], json!(["R1", "R2"]), "{report}");
        let repaired = runtime.workspace().read_draft().unwrap().unwrap();
        let design = compile(&repaired, runtime.provider()).design.unwrap();
        assert_eq!(
            design.blocks["bank_1"].components["R1"].value.as_deref(),
            Some("new-1")
        );
        assert_eq!(
            design.blocks["bank_2"].components["R2"].value.as_deref(),
            Some("new-2")
        );
        assert!(!design.blocks.contains_key("main"));
    }

    #[test]
    fn non_improving_or_regressing_invalid_repair_preserves_draft() {
        let footprints = tempfile::tempdir().unwrap();
        let runtime = AgentRuntime::with_footprint_dir_for_test(footprints.path().to_path_buf())
            .expect("test runtime");
        let prior = "version: 1\nblocks: {main: {components: {R1: {part: Device:R, footprint: Missing:One, between: [A, B]}}}}\n";

        for input in [
            json!({"update": {"R1": {"value": "changed"}}}),
            json!({"components": {"R2": {"part": "Device:R", "footprint": "Missing:Two", "between": ["A", "B"]}}}),
        ] {
            runtime.workspace().write_draft(&prior, None).unwrap();

            let report = repair_components(input, &runtime).unwrap();

            assert_eq!(
                report["code"], "invalid_component_repair_preserved_draft",
                "{report}"
            );
            assert_eq!(report["current_validation"]["errors"], 1, "{report}");
            assert!(report["candidate_validation"]["errors"].is_number());
            assert_eq!(report["draft_written"], false, "{report}");
            assert_eq!(runtime.workspace().read_draft().unwrap().unwrap(), prior);
        }
    }

    #[test]
    fn repair_of_malformed_draft_without_design_remains_blocked() {
        let footprints = tempfile::tempdir().unwrap();
        let runtime = AgentRuntime::with_footprint_dir_for_test(footprints.path().to_path_buf())
            .expect("test runtime");
        let prior = "version: [\n";
        runtime.workspace().write_draft(prior, None).unwrap();

        let report = repair_components(json!({"remove": ["R1"]}), &runtime).unwrap();

        assert_eq!(report["code"], "repair_requires_valid_draft", "{report}");
        assert_eq!(report["draft_written"], false, "{report}");
        assert_eq!(runtime.workspace().read_draft().unwrap().unwrap(), prior);
    }

    #[test]
    fn invalid_compile_quality_uses_warnings_only_as_an_error_tiebreak() {
        use circuit_lang::{Diagnostic, Diagnostics};

        let diagnostics = |errors, warnings| {
            let mut diagnostics = Diagnostics::default();
            for _ in 0..errors {
                diagnostics.push(Diagnostic::error("error", "error"));
            }
            for _ in 0..warnings {
                diagnostics.push(Diagnostic::warning("warning", "warning"));
            }
            diagnostics
        };
        assert!(invalid_compile_quality_regressed(
            &diagnostics(2, 1),
            &diagnostics(2, 2)
        ));
        assert!(!invalid_compile_quality_regressed(
            &diagnostics(2, 1),
            &diagnostics(2, 1)
        ));
        assert!(!invalid_compile_quality_regressed(
            &diagnostics(2, 1),
            &diagnostics(1, 20)
        ));
    }

    #[test]
    fn compile_report_preserves_error_classes_under_repetitive_diagnostics() {
        use circuit_lang::{Diagnostic, Diagnostics};

        let mut diagnostics = Diagnostics::default();
        for index in 0..55 {
            diagnostics.push(Diagnostic::error(
                "bad-refdes",
                format!("R_SENSOR_{index} is not a valid refdes"),
            ));
        }
        diagnostics.push(Diagnostic::error(
            "unknown-pin",
            "pin ADC0 not found on U1",
        ));
        diagnostics.push(Diagnostic::error(
            "power-pin-unconnected",
            "U1 DVDD is not connected",
        ));
        for index in 0..9 {
            diagnostics.push(Diagnostic::warning(
                "single-pin-net",
                format!("net SIGNAL_{index} has only one pin"),
            ));
        }

        let report = compile_report(&diagnostics);
        assert_eq!(report["errors"], 57);
        assert_eq!(report["warnings"], 9);
        assert_eq!(report["diagnostics_omitted"], 52);
        assert_eq!(
            report["diagnostic_code_counts"]["bad-refdes"]["errors"],
            55
        );
        assert_eq!(
            report["diagnostic_code_counts"]["single-pin-net"]["warnings"],
            9
        );

        let rendered = report["diagnostics"].as_array().unwrap();
        assert_eq!(rendered.len(), 14);
        assert_eq!(
            rendered
                .iter()
                .filter(|d| d.as_str().unwrap().contains("bad-refdes"))
                .count(),
            6
        );
        for code in ["unknown-pin", "power-pin-unconnected", "single-pin-net"] {
            assert!(
                rendered
                    .iter()
                    .any(|d| d.as_str().unwrap().contains(code)),
                "missing representative for {code}: {report}"
            );
        }
    }

    #[test]
    fn natural_components_alias_is_advertised_as_a_nonempty_component_map() {
        let tool = repair_components_tool();
        assert!(
            tool.description
                .as_deref()
                .unwrap()
                .contains("NOT for an incomplete/missing circuit")
        );
        let schema = tool.schema.expect("repair schema");
        let components = &schema["properties"]["components"];
        assert_eq!(components["type"], "object");
        assert_eq!(components["minProperties"], 1);
        assert_eq!(
            components["additionalProperties"]["required"],
            serde_json::json!(["part"])
        );
        assert!(schema["anyOf"].as_array().is_some_and(|branches| {
            branches
                .iter()
                .any(|branch| branch["required"] == serde_json::json!(["components"]))
        }));
        assert!(
            components["description"]
                .as_str()
                .unwrap()
                .contains("DIRECT")
        );
        assert!(
            schema["properties"]["update"]["description"]
                .as_str()
                .unwrap()
                .contains("Use this—not components")
        );
        assert!(
            schema["properties"]["block"]["description"]
                .as_str()
                .unwrap()
                .contains("one update/remove/components batch may span blocks")
        );
    }

    #[test]
    fn create_design_demands_the_requested_complete_document() {
        let tool = tool_defs()
            .into_iter()
            .find(|tool| tool.name.as_str() == "create_design")
            .expect("create_design tool");
        assert!(
            tool.description
                .as_deref()
                .unwrap()
                .contains("complete requested circuit")
        );
        assert!(
            tool.schema.unwrap()["properties"]["yaml"]["description"]
                .as_str()
                .unwrap()
                .contains("Complete top-level circuit-YAML")
        );
    }

    #[test]
    fn discovery_schemas_reject_empty_queries() {
        for name in ["search_symbols", "search_footprints"] {
            let tool = tool_defs()
                .into_iter()
                .find(|tool| tool.name.as_str() == name)
                .unwrap();
            let schema = tool.schema.unwrap();
            assert_eq!(schema["properties"]["query"]["minLength"], 1, "{name}");
            assert_eq!(
                schema["properties"]["queries"]["items"]["properties"]["query"]
                    ["minLength"],
                1,
                "{name}"
            );
        }
        for query in ["", "   ", "\t\n"] {
            let error = require_search_query(&json!({"query": query})).unwrap_err();
            assert!(error.to_string().contains("non-whitespace"), "{error:#}");
        }
        assert_eq!(
            require_search_query(&json!({"query": "  USB-C  "})).unwrap(),
            "  USB-C  "
        );
    }
}
