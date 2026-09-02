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
//! search/info, `sync_board`, and the place/route/export/interactive flow.
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

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::{AgentRuntime, Tool};

/// The JSON-Schema definitions for every tool, in a stable order. The
/// The `intent` object both board-building tools take: what the layout should
/// be, never where a part goes. `zones` is `sync_board`'s half (it seeds the
/// pours); the rest is `place_board`'s.
/// The board window `place_board` and `route_board` both accept. A box is a
/// selector: it names what to work on, and everything outside it is left alone.
fn bbox_schema(what: &str) -> Value {
    json!({
        "type": "object",
        "description": format!("Board window in millimetres. {what}"),
        "properties": {
            "min_x": { "type": "number" },
            "min_y": { "type": "number" },
            "max_x": { "type": "number" },
            "max_y": { "type": "number" }
        },
        "required": ["min_x", "min_y", "max_x", "max_y"],
        "additionalProperties": false
    })
}

fn intent_schema() -> Value {
    json!({
        "type": "object",
        "description": "What the layout should be, not where parts go. Coordinates belong only in move_parts{to}.",
        "properties": {
            "edge": {
                "type": "object",
                "description": "Reference -> board side its courtyard should touch.",
                "additionalProperties": { "type": "string", "enum": ["left", "right", "top", "bottom"] }
            },
            "keep_near": {
                "type": "array",
                "description": "Pairs that must end up close, e.g. [[\"C3\",\"U1\"]].",
                "items": { "type": "array", "items": { "type": "string" }, "minItems": 2, "maxItems": 2 }
            },
            "group": {
                "type": "array",
                "description": "Parts that belong together, e.g. [[\"U1\",\"C3\",\"C4\"]].",
                "items": { "type": "array", "items": { "type": "string" }, "minItems": 2 }
            },
            "zones": {
                "type": "array",
                "description": "Nets to pour as a copper zone; sync_board applies these when it creates the board.",
                "items": { "type": "string" }
            }
        },
        "additionalProperties": false
    })
}

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
            description: "Find symbol `Lib:Name`; batch up to 10 queries in one call. The \
                 best hit for each query comes back with its full pin list and default \
                 footprint inline, so get_symbol_info is only needed for a hit further down."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "minLength": 1 },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 25 },
                    "queries": {
                        "type": "array", "minItems": 1, "maxItems": 10,
                        "items": {
                            "type": "object",
                            "properties": {
                                "query": { "type": "string", "minLength": 1 },
                                "limit": { "type": "integer", "minimum": 1, "maximum": 25 }
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
            description: "Return symbol ratings, datasheet, footprint, and pins. Pass \
                 `lib_ids` to look up several symbols in one call."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "lib_id": { "type": "string", "description": "E.g. Device:R." },
                    "lib_ids": {
                        "type": "array", "minItems": 1, "maxItems": 12,
                        "items": { "type": "string", "minLength": 1 },
                        "description": "Look up this whole list in one call."
                    }
                },
                "anyOf": [{ "required": ["lib_id"] }, { "required": ["lib_ids"] }]
            }),
        },
        Def {
            name: "project_info".into(),
            description: "Return project paths/state.".into(),
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        Def {
            name: "undo".into(),
            description: "Restore every project file captured by a revision; defaults to the latest revision. Returns a new revision for the state being replaced.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "revision": { "type": "integer", "minimum": 1 }
                },
                "additionalProperties": false
            }),
        },
        Def {
            name: "history".into(),
            description: "List project revisions as plain text, newest first.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "minimum": 1, "maximum": 50 }
                },
                "additionalProperties": false
            }),
        },
        Def {
            name: "render_schematic".into(),
            description: "Render the schematic to a PNG with mm axes to check the visual result; use it whenever you want to see what an edit did. Not a substitute for `check_schematic`.".into(),
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
                    "limit": { "type": "integer", "minimum": 1, "maximum": 25 },
                    "queries": {
                        "type": "array", "minItems": 1, "maxItems": 10,
                        "items": {
                            "type": "object",
                            "properties": {
                                "query": { "type": "string", "minLength": 1 },
                                "limit": { "type": "integer", "minimum": 1, "maximum": 25 }
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
                    "from": { "oneOf": [
                        { "type": "string", "description": "Pad reference, e.g. \"U1.3\"." },
                        { "type": "array", "items": {"type":"number"}, "minItems": 2, "maxItems": 2 }
                    ] },
                    "to": { "oneOf": [
                        { "type": "string", "description": "Pad reference, e.g. \"R1.1\"." },
                        { "type": "array", "items": {"type":"number"}, "minItems": 2, "maxItems": 2 }
                    ] },
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
            description: "Delete track/via copper by click, or remove a net globally or inside a bbox."
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
                    "bbox": {
                        "type": "object",
                        "properties": {
                            "min_x": { "type": "number" },
                            "min_y": { "type": "number" },
                            "max_x": { "type": "number" },
                            "max_y": { "type": "number" }
                        },
                        "required": ["min_x", "min_y", "max_x", "max_y"],
                        "description": "Delete matching net copper intersecting this board-space box."
                    },
                    "kinds": {
                        "type": "array",
                        "items": { "type": "string", "enum": ["track", "via"] },
                        "description": "Default both."
                    },
                    "net": { "type": "string" },
                    "layer": { "type": "string" },
                    "all": { "type": "boolean", "description": "All matches; default nearest." }
                }
            }),
        },
        Def {
            name: "set_net_width".into(),
            description:
                "Set one existing board net's net-class width; prefer sync rules pre-route."
                    .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "net": { "type": "string" },
                    "name": { "type": "string", "description": "Optional net-class name." },
                    "width": { "type": "number", "description": "Track width in mm." },
                    "clearance": { "type": "number", "description": "mm; default 0.2." },
                },
                "required": ["net", "width"]
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
            name: "sync_board".into(),
            description: "Sync the PCB to the schematic: creates the board when absent, \
                 else applies only the delta and keeps placement and copper. bounds/rules \
                 apply on creation only; omit bounds to size the outline from the footprints \
                 (the result reports required_bounds and recommended_bounds). \
                 clearance/min_trace_width are lowered to what those footprints permit \
                 (reported in design_rules)."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "intent": intent_schema(),
                    "bounds": {
                        "description": "Omit (or \"auto\") to size the board from its parts. Bounds smaller than required_bounds are refused before anything is written.",
                        "oneOf": [
                            { "type": "string", "enum": ["auto"] },
                            {
                                "type": "object",
                                "properties": {
                                    "min_x": { "type": "number" }, "max_x": { "type": "number" },
                                    "min_y": { "type": "number" }, "max_y": { "type": "number" }
                                }
                            }
                        ]
                    },
                    "rules": {
                        "type": "object",
                        "properties": {
                            "layer_count": { "type": "integer", "enum": [2, 4, 6, 8], "description": "Default 2. Ask for 4+ only for a dense/high-speed board; route_board reports layers_used." },
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
            description: "Inspect the board; net returns its pads, tracks, vias, coordinates, and endpoint touches."
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
            description: "Place a PCB from stated intent: `intent` gives edges, \
                 proximities and groups (never coordinates); `groups` steers regions, grids \
                 and surrounds. LOCAL by default: `refs` names the parts to move, or `bbox` \
                 selects every footprint whose centre is inside a board window — everything \
                 else stays locked where it sits and its copper becomes a keep-out. With no \
                 arguments it places exactly the parts that are still unplaced, leaving every \
                 laid-out pose alone. Copper on the parts it moves is retracted (see \
                 nets_to_reroute), and a local call reports what is still_unplaced. It refuses \
                 only when nothing is unplaced — pass replace:true to re-place a finished \
                 board and lose its layout."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "refs": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Place only these footprints, with every other part locked where it sits and the existing copper as keep-outs. Omit to place the whole board."
                    },
                    "bbox": bbox_schema(
                        "Every footprint whose courtyard centre is inside the window is placed, \
                         and the window is where they should end up."
                    ),
                    "intent": intent_schema(),
                    "replace": {
                        "type": "boolean",
                        "description": "Re-place an already-placed board, losing its layout."
                    },
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
            description: "Auto-route the PLACED board, committing every net whose copper is \
                 DRC-clean; refuses while any part is unplaced. \
                 Reports routed N/M and, per unrouted net, the two pads, the obstacle in the \
                 way and the repair. LOCAL by default: pass `nets` to re-route only those \
                 nets after a move_parts, or `bbox` to rip and re-route only the nets that \
                 reach into a board window. Every other net's copper is kept exactly as it is \
                 and treated as fixed obstacle."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "nets": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Route only these nets, keeping all other copper. Omit for the whole board."
                    },
                    "bbox": bbox_schema(
                        "Every net with a pad in the window, or copper entering it, is ripped \
                         and re-routed; every other net's copper is fixed."
                    )
                }
            }),
        },
        Def {
            name: "render_board".into(),
            description: "Render board PNG.".into(),
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        Def {
            name: "check_board".into(),
            description: "Run PCB DRC and classify violations and unrouted pairs against the \
                 turn-start board. `ok` considers introduced blocking findings only: fix those \
                 and leave inherited findings alone unless asked. On failure lists every introduced \
                 unconnected item as the pad pair it is, and `unplaced` — the footprints still in \
                 the seed row, which place_board({refs}) lays out."
                .into(),
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        Def {
            name: "refill_zones".into(),
            description: "Refill every copper zone in KiCad and persist the filled board before checking connectivity."
                .into(),
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
            let description = if matches!(
                d.name.as_str(),
                "sync_board"
                    | "place_board"
                    | "route_board"
                    | "move_parts"
                    | "route_track"
                    | "delete_copper"
                    | "refill_zones"
                    | "set_net_width"
                    | "update_board_outline"
            ) {
                format!(
                    "{} Success returns the pre-write `revision`.",
                    d.description
                )
            } else {
                d.description
            };
            Tool::new(d.name)
                .with_description(description)
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
        "undo" => undo(input, ctx),
        "history" => history(input, ctx),
        "render_schematic" => render_schematic(ctx),
        "search_footprints" => pcb_workflow::search_footprints(input, ctx),
        "get_footprint_info" => pcb_workflow::get_footprint_info(input, ctx),
        "sync_board" => pcb_workflow::sync_board(input, ctx),
        "get_board" => pcb_workflow::get_board(input, ctx),
        "place_board" => pcb_workflow::place_board(input, ctx),
        "route_board" => pcb_workflow::route_board(input, ctx),
        "check_board" => pcb_workflow::check_board(input, ctx),
        "refill_zones" => pcb_workflow::refill_zones(input, ctx),
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

/// Searches one `search_symbols` call may batch. A 50-part design needs pin
/// names for a dozen distinct symbols; at four per call that alone cost three
/// provider requests before any part could be placed.
const MAX_BATCHED_QUERIES: usize = 10;

/// Symbols one `get_symbol_info` call may look up.
const MAX_BATCHED_SYMBOLS: usize = 12;

/// A symbol's pins, one compact entry per pin. `unit` is carried only for
/// multi-unit symbols, where it is the only way to tell the units apart.
fn pin_digest(meta: &sch_check::SymbolMeta) -> Vec<Value> {
    let multi_unit = meta.pins.iter().any(|p| p.unit > 1);
    meta.pins
        .iter()
        .map(|p| {
            let mut pin = json!({
                "number": p.number,
                "name": p.name,
                "type": pin_type_str(p.etype),
            });
            if multi_unit {
                pin["unit"] = json!(p.unit);
            }
            pin
        })
        .collect()
}

fn search_symbols(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    if let Some(queries) = input.get("queries") {
        let queries = queries
            .as_array()
            .ok_or_else(|| anyhow!("`queries` must be an array"))?;
        if queries.is_empty() || queries.len() > MAX_BATCHED_QUERIES {
            bail!("`queries` must contain 1 to {MAX_BATCHED_QUERIES} searches");
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

    let mut hits: Vec<Value> = ctx
        .index()?
        .search(query, limit)
        .into_iter()
        .map(|h| json!({ "lib_id": h.lib_id, "pin_count": h.pin_count }))
        .collect();

    // The best hit carries its pins and default footprint, so the common case —
    // "find this part, then write its pin map" — is one request instead of two.
    if let Some(top) = hits.first_mut()
        && let Some(lib_id) = top["lib_id"].as_str().map(str::to_string)
        && let Some(meta) = ctx
            .provider()
            .symbol(&lib_id)
            .or_else(|| ctx.index().ok()?.symbol(&lib_id))
    {
        top["pins"] = json!(pin_digest(&meta));
        top["default_footprint"] = json!(meta.footprint);
        top["description"] = json!(meta.description);
    }

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
    if let Some(ids) = input.get("lib_ids") {
        let ids = ids
            .as_array()
            .ok_or_else(|| anyhow!("`lib_ids` must be an array"))?;
        if ids.is_empty() || ids.len() > MAX_BATCHED_SYMBOLS {
            bail!("`lib_ids` must contain 1 to {MAX_BATCHED_SYMBOLS} symbols");
        }
        let symbols: Result<Vec<Value>> = ids
            .iter()
            .map(|id| {
                let id = id
                    .as_str()
                    .ok_or_else(|| anyhow!("each `lib_ids` entry must be a string"))?;
                get_symbol_info_one(id, ctx)
            })
            .collect();
        return Ok(json!({ "symbols": symbols? }));
    }
    let lib_id = require_str(&input, "lib_id")?;
    get_symbol_info_one(&lib_id, ctx)
}

fn get_symbol_info_one(lib_id: &str, ctx: &AgentRuntime) -> Result<Value> {
    match ctx
        .provider()
        .symbol(lib_id)
        .or_else(|| ctx.index().ok()?.symbol(lib_id))
    {
        Some(meta) => {
            let pins = pin_digest(&meta);
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
            let suggestions = ctx.provider().suggest(lib_id);
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
        NoConnect => "no_connect",
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

fn undo(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let id = match input.get("revision") {
        None | Some(Value::Null) => None,
        Some(value) => match value.as_u64() {
            Some(id) if id > 0 => Some(gordian_runtime::revisions::RevisionId::new(id)),
            _ => return Ok(json!({ "error": "revision must be a positive integer" })),
        },
    };
    let target = match ctx.revisions().manifest(id) {
        Ok(manifest) => manifest,
        Err(error) => return Ok(json!({ "error": error.to_string() })),
    };
    let files: Vec<_> = target
        .files
        .iter()
        .map(|file| ctx.project_dir().join(&file.path))
        .collect();
    let revision = match ctx.revisions().capture(
        "undo",
        &format!("Restore revision {}", target.id),
        &files,
    ) {
        Ok(revision) => revision,
        Err(error) => {
            return Ok(
                json!({ "error": format!("could not capture the current project before undo: {error}") }),
            );
        }
    };
    let restored = match ctx.revisions().restore(Some(target.id)) {
        Ok(restored) => restored,
        Err(error) => {
            return Ok(
                json!({ "error": format!("could not restore revision {}: {error}", target.id), "revision": revision }),
            );
        }
    };
    Ok(json!({
        "changed": format!("restored project revision {}", restored.id),
        "restored_revision": restored.id,
        "files": restored.files,
        "revision": revision,
    }))
}

fn history(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let limit = input
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(20)
        .clamp(1, 50) as usize;
    let manifests = match ctx.revisions().history(limit) {
        Ok(manifests) => manifests,
        Err(error) => return Ok(json!({ "error": error.to_string() })),
    };
    if manifests.is_empty() {
        return Ok(Value::String("No project revisions.".to_owned()));
    }
    let mut out = String::new();
    for manifest in manifests {
        let files = manifest
            .files
            .iter()
            .map(|file| file.path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        use std::fmt::Write as _;
        writeln!(
            out,
            "{}  {}  {}  {}  [{}]",
            manifest.id, manifest.created_at, manifest.tool, manifest.summary, files
        )
        .expect("writing to a string cannot fail");
    }
    Ok(Value::String(out))
}

/// Result key carrying a PNG path for the agent loop to attach as an image
/// block (and strip from the JSON the model sees as text).
fn render_schematic(ctx: &AgentRuntime) -> Result<Value> {
    if !ctx.sch_path().exists() {
        return Ok(json!({
            "error": "no schematic yet — create one with place_parts first",
        }));
    }
    let doc = sch_doc::SchDoc::read(ctx.sch_path()).context("reading schematic visual facts")?;
    let visual = sch_floorplan::visual::measure(&doc);
    let baseline = ctx.revisions().turn_baseline(ctx.sch_path())?;
    let baseline_visual = baseline
        .as_ref()
        .and_then(|baseline| baseline.path.as_deref())
        .map(|path| {
            sch_doc::SchDoc::read(path)
                .map(|doc| sch_floorplan::visual::measure(&doc))
                .context("measuring turn-start schematic visual facts")
        })
        .transpose()?;
    let mut visual_json = visual_with_introduced(&visual, baseline_visual.as_ref())?;
    visual_json["baseline_revision"] = json!(baseline.as_ref().map(|baseline| baseline.revision));
    let content_bounds = render_bounds(visual.sheet_extent);
    let overview_bounds = padded_bounds(content_bounds, 2.54);
    let part_count = doc
        .symbols()
        .filter(|symbol| !symbol.refdes().is_empty() && !symbol.refdes().starts_with('#'))
        .count();
    let plan = gordian_runtime::render::render_plan(
        part_count,
        content_bounds,
        ctx.config().tools.render_max_px,
    );
    let source_svg = gordian_runtime::render::schematic_svg(ctx.env(), ctx.sch_path())?;
    let overview_svg = schematic_overlay(&source_svg, overview_bounds);
    let png = gordian_runtime::render::svg_to_png(&overview_svg, plan.overview_px)?;
    let path = ctx.workspace().write_render(&png)?;
    let mut detail_paths = Vec::new();
    if let Some(detail_px) = plan.detail_px {
        for region in detail_regions(content_bounds) {
            let detail_svg = schematic_overlay(&source_svg, region);
            let detail_png = gordian_runtime::render::svg_to_png(&detail_svg, detail_px)?;
            let detail_path = ctx.workspace().write_render(&detail_png)?;
            detail_paths.push(json!({
                "region": [region.min_x, region.min_y, region.max_x, region.max_y],
                "png_path": detail_path.display().to_string(),
            }));
        }
    }
    let mut obj = json!({
        "ok": true,
        "png_path": path.display().to_string(),
        "overview_px": plan.overview_px,
        "detail_paths": detail_paths,
        "visual": visual_json,
        "note": format!(
            "Schematic rendered from the saved .kicad_sch using KiCad's schematic SVG export and attached. \
             Symbols, fields, labels, and wires are drawn on a light background; X/Y axes and ticks \
             are sheet millimetres, matching read_schematic @x,y positions. PNG saved to {}. \
             visual lists the deterministic measured problems; dense/large sheets also return \
             detail_paths whose region boxes can be passed to read_schematic.",
            path.display(),
        ),
    });
    obj[IMAGE_PATH_KEY] = json!(path.display().to_string());
    Ok(obj)
}

fn visual_with_introduced(
    visual: &sch_floorplan::visual::VisualFacts,
    baseline: Option<&sch_floorplan::visual::VisualFacts>,
) -> Result<Value> {
    let mut current = serde_json::to_value(visual)?;
    let baseline = baseline
        .map(serde_json::to_value)
        .transpose()?
        .unwrap_or_else(|| json!({}));
    for name in [
        "body_overlaps",
        "wires_through_bodies",
        "text_collisions",
        "off_grid_pins",
        "dangling_wire_ends",
    ] {
        let mut available = baseline
            .get(name)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let introduced = current
            .get(name)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|item| {
                available
                    .iter()
                    .position(|baseline| baseline == *item)
                    .is_none_or(|position| {
                        available.remove(position);
                        false
                    })
            })
            .cloned()
            .collect::<Vec<_>>();
        current[format!("{name}_introduced")] = json!(introduced);
    }
    Ok(current)
}

fn render_bounds(extent: [f64; 4]) -> gordian_runtime::render::RenderBounds {
    let [mut min_x, mut min_y, mut max_x, mut max_y] = extent;
    if max_x - min_x < 1.0 {
        min_x -= 10.0;
        max_x += 10.0;
    }
    if max_y - min_y < 1.0 {
        min_y -= 10.0;
        max_y += 10.0;
    }
    gordian_runtime::render::RenderBounds::new(min_x, min_y, max_x, max_y)
}

fn padded_bounds(
    bounds: gordian_runtime::render::RenderBounds,
    padding: f64,
) -> gordian_runtime::render::RenderBounds {
    gordian_runtime::render::RenderBounds::new(
        bounds.min_x - padding,
        bounds.min_y - padding,
        bounds.max_x + padding,
        bounds.max_y + padding,
    )
}

fn schematic_overlay(svg: &str, bounds: gordian_runtime::render::RenderBounds) -> String {
    let cropped = gordian_runtime::render::crop_svg(svg, bounds);
    gordian_runtime::render::add_coordinate_overlay(
        &cropped,
        bounds,
        "mm",
        gordian_runtime::render::CoordinateOverlayStyle {
            background: "#fffdf7",
            axis: "#1f2937",
            grid: "#94a3b8",
            x_axis: "#be123c",
            y_axis: "#1d4ed8",
        },
    )
}

fn detail_regions(
    bounds: gordian_runtime::render::RenderBounds,
) -> [gordian_runtime::render::RenderBounds; 4] {
    let mid_x = (bounds.min_x + bounds.max_x) / 2.0;
    let mid_y = (bounds.min_y + bounds.max_y) / 2.0;
    let overlap = 1.27;
    [
        gordian_runtime::render::RenderBounds::new(
            bounds.min_x,
            bounds.min_y,
            (mid_x + overlap).min(bounds.max_x),
            (mid_y + overlap).min(bounds.max_y),
        ),
        gordian_runtime::render::RenderBounds::new(
            (mid_x - overlap).max(bounds.min_x),
            bounds.min_y,
            bounds.max_x,
            (mid_y + overlap).min(bounds.max_y),
        ),
        gordian_runtime::render::RenderBounds::new(
            bounds.min_x,
            (mid_y - overlap).max(bounds.min_y),
            (mid_x + overlap).min(bounds.max_x),
            bounds.max_y,
        ),
        gordian_runtime::render::RenderBounds::new(
            (mid_x - overlap).max(bounds.min_x),
            (mid_y - overlap).max(bounds.min_y),
            bounds.max_x,
            bounds.max_y,
        ),
    ]
}
