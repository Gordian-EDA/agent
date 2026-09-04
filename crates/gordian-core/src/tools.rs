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
//! discovery (`search_symbols` / `get_symbol_info` / `search_footprints`),
//! `project_info` and `render_schematic`; `pcb-workflow` covers footprint info,
//! `sync_board`, and the place/route/export/interactive flow.
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

use gordian_runtime::tool::{require_search_query, require_str};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::{AgentRuntime, Tool};

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

/// The `intent` object both board-building tools take: what the layout should
/// be, never where a part goes. `zones` is `sync_board`'s half (it seeds the
/// pours); the rest is `place_board`'s.
fn intent_schema() -> Value {
    json!({
        "type": "object",
        "description": "What the layout should be, not where parts go. Coordinates belong only in move_parts{to}.",
        "properties": {
            "edge": {
                "type": "object",
                "description": "Reference -> board side its courtyard should touch; compass aliases are normalized in the response.",
                "additionalProperties": { "type": "string", "enum": ["left", "right", "top", "bottom", "north", "south", "east", "west", "N", "S", "E", "W", "n", "s", "e", "w"] }
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
                 best hit for each query comes back with its full pin list, alternate functions, \
                 and default compatible footprint inline. Those pin numbers, names, or alternates \
                 are valid place_parts pin keys, case-insensitively."
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
            description: "Return symbol ratings, datasheet, footprint, pins, and alternate \
                 pin functions. Pass `lib_ids` to look up several symbols in one call."
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
            name: "reserve_refs".into(),
            description: "Claim a block of reference designators (R7…R12) and record the claim \
                 in the project. Use it before adding parts while another agent works on the \
                 same design, then name those exact refs in place_parts/add_parts — the claim \
                 is recorded and skipped by later reserve_refs calls, so two agents that both \
                 reserve never collide."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "prefix": { "type": "string", "minLength": 1, "description": "Letters only: R, C, U, TP." },
                    "count": { "type": "integer", "minimum": 1, "maximum": gordian_runtime::refdes::MAX_RESERVATION }
                },
                "required": ["prefix", "count"],
                "additionalProperties": false
            }),
        },
        // ── PCB tools (slice 5) ─────────────────────────────────────────
        Def {
            name: "search_footprints".into(),
            description: "Find footprint `Lib:Name` IDs by fuzzy name query, optionally ranked for an electrical symbol. With `symbol`, compatible pad-number matches rank before query text; without it, names from an explicit `Lib:` prefix rank first. Batch up to 4 searches."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "minLength": 1, "description": "KiCAD symbol Lib:Name whose electrical pins the footprint must fit." },
                    "query": { "type": "string", "minLength": 1, "description": "Optional physical package or footprint-name preference." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 25 },
                    "queries": {
                        "type": "array", "minItems": 1, "maxItems": 4,
                        "items": {
                            "type": "object",
                            "properties": {
                                "symbol": { "type": "string", "minLength": 1 },
                                "query": { "type": "string", "minLength": 1 },
                                "limit": { "type": "integer", "minimum": 1, "maximum": 25 }
                            },
                            "anyOf": [{ "required": ["symbol"] }, { "required": ["query"] }],
                            "additionalProperties": false
                        }
                    }
                },
                "anyOf": [
                    { "required": ["symbol"] },
                    { "required": ["query"] },
                    { "required": ["queries"] }
                ],
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
            name: "move_parts".into(),
            description: "Move footprints by to/at, by, near, or edge, with rotation/rot and offsets. Accepts ref/reference, defaults an omitted near/edge gap, and nudges an occupied target through 5, 10, and 20 mm searches before choosing the nearest legal pose anywhere inside the outline. Reports nudged_to and nudge_distance_mm; refuses only when the outline has no legal room.".into(),
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
                                "ref": { "type": "string" },
                                "to": {
                                    "type": "array",
                                    "items": { "type": "number" },
                                    "minItems": 2,
                                    "maxItems": 2
                                },
                                "at": {
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
                                "rot": { "type": "number" },
                                "horizontal_offset": { "type": "number" },
                                "vertical_offset": { "type": "number" }
                            },
                            "anyOf": [{ "required": ["reference"] }, { "required": ["ref"] }]
                        }
                    }
                },
                "required": ["moves"]
            }),
        },
        Def {
            name: "route_track".into(),
            description: "Route one connection around obstacles, with layers and optional vias. Widths below the board minimum are raised with a note; same-layer vias are dropped; a blocked search returns blocker geometry, a waypoint and a layer alternative without writing copper."
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
            description: "Delete track/via copper by click, all:true, nets, or bbox. Reports deleted counts by kind and any now_open nets; removing copper never refuses for a newly attributed clearance finding."
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
                    "nets": {
                        "type": "array",
                        "items": { "type": "string" },
                        "minItems": 1,
                        "description": "Delete all copper on these nets."
                    },
                    "layer": { "type": "string" },
                    "all": { "type": "boolean", "description": "Set true to delete all track/via copper; may be combined only with kinds or layer." }
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
            description: "Edit Edge.Cuts by bounds, polygon, or fitted geometry. A fit request on a routed board is a successful no-op reported as outline_refit: skipped (routed board).".into(),
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
                        "description": "Fit around parts before routing; reports a successful skipped no-op when copper already exists."
                    },
                    "margin": { "type": "number", "description": "Margin mm; default 2." }
                }
            }),
        },
        Def {
            name: "lock_parts".into(),
            description: "Lock footprints where they sit. A locked part is never moved by \
                 place_board or any other helper, and move_parts refuses it. Lock the parts \
                 whose position is decided — connectors, mounting holes, a pose worth keeping."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "refs": { "type": "array", "minItems": 1, "items": { "type": "string" } },
                    "reason": {
                        "type": "string",
                        "enum": ["mechanical", "agent", "user"],
                        "description": "Default agent. `mechanical` for a position the physical world fixes."
                    }
                },
                "required": ["refs"],
                "additionalProperties": false
            }),
        },
        Def {
            name: "unlock_parts".into(),
            description: "Release locks so placement may move these parts again.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "refs": { "type": "array", "minItems": 1, "items": { "type": "string" } }
                },
                "required": ["refs"],
                "additionalProperties": false
            }),
        },
        Def {
            name: "sync_board".into(),
            description: "Sync the PCB to the schematic: creates the board when absent, \
                 else applies only the delta and keeps placement and copper. Parts without a \
                 usable footprint remain staged and are named instead of blocking the sync. \
                 ERC errors are reported in schematic_erc but do not block. On an existing board, \
                 intent applies the delta then places staged/new parts. Geometry findings remain \
                 written as honest partial work in guard_findings. Omit bounds for a managed auto outline \
                 that place_board grows/refits around placed parts while ignoring the staging row. \
                 clearance/min_trace_width are lowered to what those footprints permit \
                 (reported in design_rules). Subminimum clearance, width, via diameter, drill, \
                 and annular-ring requests are raised to standard-fab floors and reported in \
                 rule_adjustments instead of refusing the sync. When a schematic edit re-nets \
                 routed pads, sync retracts only the old copper components that would short the \
                 new nets and returns copper_retracted plus their now_open ratsnest entries."
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
                                "description": "One net or a list of nets/objects. Omitted layer defaults to B.Cu on 2-layer boards and an inner plane on 4+ layers; sync_board reports canonical objects.",
                                "oneOf": [
                                    { "type": "string" },
                                    {
                                        "type": "object",
                                        "properties": {
                                            "net": { "type": "string" },
                                            "layer": { "type": "string" },
                                            "connect": { "type": "string", "enum": ["thermal", "solid"] }
                                        },
                                        "required": ["net"]
                                    },
                                    {
                                        "type": "array",
                                        "items": {
                                            "oneOf": [
                                                { "type": "string" },
                                                {
                                                    "type": "object",
                                                    "properties": {
                                                        "net": { "type": "string" },
                                                        "layer": { "type": "string" },
                                                        "connect": { "type": "string", "enum": ["thermal", "solid"] }
                                                    },
                                                    "required": ["net"]
                                                }
                                            ]
                                        }
                                    }
                                ]
                            }
                        }
                    }
                }
            }),
        },
        Def {
            name: "get_board".into(),
            description: "Inspect the board: its parts split into `staged` (with staged_reason), \
                 `placed` and `locked`, its outline bounds, part extents, rules and nets. `net` adds that net's pads, \
                 copper, endpoint touches and its `ratsnest` entry — status, blocker, escapes."
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
                 else stays where it sits and its copper becomes a keep-out. With no arguments \
                 it places exactly the parts that are still staged, leaving every laid-out \
                 pose alone; when there is nothing staged it reports that and moves nothing. \
                 Copper on the parts it moves is retracted (see nets_to_reroute), and a local \
                 call reports what is still_staged. Locked parts are never moved and come \
                 back as skipped_locked; pass replace:true to re-place a finished board and \
                 lose its layout. A managed auto outline grows/refits to the accepted placement; \
                 fixed bounds accept fitting parts and report the rest with extents and suggested bounds. Unknown refs are classified as staged, absent from the schematic, absent pending sync, or present under another spelling while valid refs proceed."
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
            description: "Auto-route the board, committing every net whose copper is \
                 DRC-clean. A partially placed board routes what it can: nets that reach a \
                 STAGED part are left alone and reported open with the part to place. Returns \
                 routed N/M and a `ratsnest` entry per net — the two pads, status \
                 open/routed/blocked, the blocker, and the escapes. LOCAL by default: pass \
                 `nets` to re-route only those nets after a move_parts, or `bbox` to rip and \
                 re-route only the nets that reach into a board window. Every other net's \
                 copper is kept exactly as it is and treated as fixed obstacle. Plane nets keep clean connected fanout and report unreached pads; stale schematic nets request sync_board first."
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
            description: "Progress and DRC for the board as it stands: `routed N/M`, `blocked` \
                 (the same ratsnest entries route_board returns), `staged` (the parts still in \
                 the staging row, with reason and extent), outline bounds, grouped top_violations, \
                 and representative live DRC findings with an explicit truncation flag. `ok` considers \
                 every blocking finding. A staged part is never a violation."
                .into(),
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        Def {
            name: "refill_zones".into(),
            description: "Refill every copper zone in KiCad and persist the filled board before checking connectivity."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
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
        return result.map(pcb_workflow::enrich_positional_pin_refusal);
    }
    match name {
        "search_symbols" => search_symbols(input, ctx),
        "get_symbol_info" => get_symbol_info(input, ctx),
        "project_info" => project_info(ctx),
        "reserve_refs" => reserve_refs(input, ctx),
        "get_footprint_info" => pcb_workflow::get_footprint_info(input, ctx),
        "sync_board" => match bench_refusal(ctx, "sync_board")? {
            Some(refusal) => Ok(refusal),
            None => pcb_workflow::sync_board(input, ctx),
        },
        "get_board" => pcb_workflow::get_board(input, ctx),
        "place_board" => pcb_workflow::place_board(input, ctx),
        "route_board" => pcb_workflow::route_board(input, ctx),
        "check_board" => pcb_workflow::check_board(input, ctx),
        "refill_zones" => pcb_workflow::refill_zones(input, ctx),
        "export_fab" => match bench_refusal(ctx, "export_fab")? {
            Some(refusal) => Ok(refusal),
            None => pcb_workflow::export_fab(input, ctx),
        },
        "move_parts" => pcb_workflow::move_parts(input, ctx),
        "lock_parts" => pcb_workflow::lock_parts(input, ctx),
        "unlock_parts" => pcb_workflow::unlock_parts(input, ctx),
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
            if !p.alternates.is_empty() {
                pin["alternates"] = json!(p.alternates);
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
    let (mut hits, note) = if let Some((lib_id, pin_count)) = common_connector_symbol_alias(query) {
        (
            vec![json!({ "lib_id": lib_id, "pin_count": pin_count })],
            Some(
                "Connector symbols and physical footprints are separate choices; the inline footprint is electrically compatible.",
            ),
        )
    } else if let Some((lib_id, pin_count)) = builtin_symbol_alias(query) {
        (
            vec![json!({ "lib_id": lib_id, "pin_count": pin_count })],
            Some("built-in alias/canonical symbol; use it directly and do not repeat this search"),
        )
    } else {
        (
            ctx.index()?
                .search(query, limit)
                .into_iter()
                .map(|h| json!({ "lib_id": h.lib_id, "pin_count": h.pin_count }))
                .collect(),
            None,
        )
    };

    // The best hit carries its pins and compatible footprint, so the common case —
    // "find this part, then write its pin map" — is one request instead of two.
    if let Some(top) = hits.first_mut()
        && let Some(lib_id) = top["lib_id"].as_str().map(str::to_string)
        && let Some(meta) = ctx
            .provider()
            .symbol(&lib_id)
            .or_else(|| ctx.index().ok()?.symbol(&lib_id))
    {
        top["pins"] = json!(pin_digest(&meta));
        top["footprint"] = json!(validated_symbol_footprint(ctx, &lib_id, &meta)?);
        top["description"] = json!(meta.description);
    }

    let mut result = json!({ "hits": hits });
    if let Some(note) = note {
        result["note"] = json!(note);
    }
    Ok(result)
}

fn validated_symbol_footprint(
    ctx: &AgentRuntime,
    symbol_id: &str,
    meta: &sch_check::SymbolMeta,
) -> Result<Option<String>> {
    if symbol_id.starts_with("power:") {
        return Ok(None);
    }
    if let Some(footprint) = meta.footprint.as_deref()
        && gordian_runtime::footprint_compat::footprint_compatibility(ctx, symbol_id, footprint)
            .is_ok_and(|verdict| verdict.compatible)
    {
        return Ok(Some(footprint.to_owned()));
    }
    gordian_runtime::footprint_compat::best_compatible_footprint(
        ctx,
        symbol_id,
        meta.footprint.as_deref(),
    )
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
            let footprint = validated_symbol_footprint(ctx, lib_id, &meta)?;
            Ok(json!({
                "lib_id": lib_id,
                "description": meta.description,
                "keywords": meta.keywords,
                "datasheet": meta.datasheet,
                "footprint": footprint,
                "pins": pins,
            }))
        }
        None => {
            if sch_check::authored::looks_like_footprint(lib_id) {
                let (message, suggestions) =
                    sch_check::authored::unknown_part_details(lib_id, ctx.provider());
                return Ok(json!({
                    "error": message,
                    "suggestions": suggestions,
                }));
            }
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

/// Claim a block of reference designators so two callers never mint the same
/// `R12`. The reservation is recorded in the project, not in this process.
fn reserve_refs(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let Some(prefix) = input.get("prefix").and_then(Value::as_str) else {
        return Ok(json!({ "error": "reserve_refs needs a `prefix` such as \"R\"" }));
    };
    let count = input.get("count").and_then(Value::as_u64).unwrap_or(1) as u32;
    let taken = designators_in_use(ctx);
    match ctx.reservations().reserve(prefix, count, &taken) {
        Ok(reservation) => Ok(json!({
            "ok": true,
            "prefix": reservation.prefix,
            "start": reservation.start,
            "count": reservation.count,
            "refs": reservation.refs,
            "note": "These references are recorded as yours: no later reserve_refs will hand                      them out, and no part in the design carries them today. Use exactly these                      refs when you add the parts.",
        })),
        Err(error) => Ok(json!({ "error": error.to_string() })),
    }
}

/// Every reference the project's schematic and board already carry, so a
/// reservation never collides with a part that exists but was never reserved.
fn designators_in_use(ctx: &AgentRuntime) -> std::collections::BTreeSet<String> {
    let mut taken = std::collections::BTreeSet::new();
    taken.extend(pcb_workflow::board_references(ctx));
    if let Ok(doc) = sch_doc::SchDoc::read(ctx.sch_path()) {
        taken.extend(doc.symbols().map(|symbol| symbol.refdes().to_owned()));
    }
    taken
}

/// The one refusal a partial state earns: a symbol on the bench is on its nets but
/// has no layout, so a board built from it would silently omit real circuitry.
///
/// Everything else about an unfinished design is reported as progress; this is the
/// line, and it names exactly which references have to be arranged first.
fn bench_refusal(ctx: &AgentRuntime, tool: &str) -> Result<Option<Value>> {
    if !ctx.sch_path().is_file() {
        return Ok(None);
    }
    let doc = sch_doc::SchDoc::read(ctx.sch_path()).context("reading schematic for the bench")?;
    let bench = sch_floorplan::bench::benched(&doc);
    if bench.is_empty() {
        return Ok(None);
    }
    Ok(Some(json!({
        "ok": false,
        "code": "bench_not_empty",
        "bench": bench.len(),
        "bench_refs": bench.clone(),
        "error": format!(
            "{tool} needs a finished schematic: {} symbol(s) are still on the bench — placed and \
             wired by name, but not laid out ({}). Call arrange({{refs}}) or arrange({{block}}) \
             on them first.",
            bench.len(),
            bench.join(", ")
        ),
    })))
}

