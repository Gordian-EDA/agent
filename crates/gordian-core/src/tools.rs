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
use kicad_cli::KicadCli;
use sch_io::read::lift;

use crate::{AgentRuntime, SchematicPlacementEngine, Tool};

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
                description: "Find KiCAD symbol `Lib:Name` ids. Skip stable built-ins like Device:R/C/LED, power:GND/+3V3, Connector:Conn_01x02_Pin; reuse hits."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Part name/fragment." },
                        "limit": { "type": "integer", "description": "Max hits, default 8.", "minimum": 1 }
                    },
                    "required": ["query"]
                }),
            },
            Def {
                name: "get_symbol_info".into(),
                description: "Return pin number/name/type/unit for a symbol `Lib:Name`."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "lib_id": { "type": "string", "description": "Symbol id, e.g. Device:R." }
                    },
                    "required": ["lib_id"]
                }),
            },
            Def {
                name: "validate_design".into(),
                description: "Compile circuit-YAML without writing; returns ok/errors/warnings."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "yaml": { "type": "string", "description": "circuit-YAML." }
                    },
                    "required": ["yaml"]
                }),
            },
            Def {
                name: "apply_design".into(),
                description: "Compile/render schematic, preview the diff, and submit it through the approval gate. Omit yaml to use draft."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "yaml": { "type": "string", "description": "Optional circuit-YAML; default draft." }
                    }
                }),
            },
            Def {
                name: "review_design".into(),
                description: "Independent electrical review of the current draft. Costly: call once when the draft is complete, fix high-confidence defects, then continue."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "intent": { "type": "string", "description": "Design goal with rails/key parts/interfaces." }
                    }
                }),
            },
            Def {
                name: "run_erc".into(),
                description: "Run KiCAD ERC on the current schematic; returns counts and violations."
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
                description: "Read circuit-YAML as plain text. source='draft' reads/seeds the project draft; otherwise source is a .kicad_sch path and does not change the draft."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "source": { "type": "string", "description": "'draft' (default) or a .kicad_sch path." }
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
                description: "Create a new circuit-YAML draft. Fails if one exists unless overwrite=true."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "yaml": { "type": "string", "description": "Full draft." },
                        "overwrite": { "type": "boolean", "description": "Replace existing draft." }
                    },
                    "required": ["yaml"]
                }),
            },
            Def {
                name: "edit_design".into(),
                description: "Edit draft. For multiple changes use one full `yaml` replacement; use old_string/new_string only for one exact copied snippet. Returns diagnostics."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "yaml": { "type": "string", "description": "Full replacement draft." },
                        "old_string": { "type": "string", "description": "Exact current snippet." },
                        "new_string": { "type": "string", "description": "Replacement snippet." },
                        "replace_all": { "type": "boolean", "description": "Replace all matches." }
                    }
                }),
            },
            // ── PCB tools (slice 5) ─────────────────────────────────────────
            Def {
                name: "search_footprints".into(),
                description: "Find real KiCAD footprint `Lib:Name` ids. Use during schematic drafting before apply_design; reuse hits."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Footprint name/fragment." },
                        "limit": { "type": "integer", "description": "Max hits, default 8.", "minimum": 1 }
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
                        "lib_id": { "type": "string", "description": "Footprint id." }
                    },
                    "required": ["lib_id"]
                }),
            },
            Def {
                name: "assign_footprint".into(),
                description: "Set one component's footprint field in the circuit-YAML draft. After assignments, apply_design before regenerate_board."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "reference": { "type": "string", "description": "Refdes." },
                        "footprint": { "type": "string", "description": "Footprint id." }
                    },
                    "required": ["reference", "footprint"]
                }),
            },
            Def {
                name: "open_board".into(),
                description: "Open the project .kicad_pcb in headless KiCAD for live IPC edits; returns board_state."
                    .into(),
                input_schema: json!({ "type": "object", "properties": {} }),
            },
            Def {
                name: "board_state".into(),
                description: "Read live board refs/positions, track count, and nets. Requires open_board."
                    .into(),
                input_schema: json!({ "type": "object", "properties": {} }),
            },
            Def {
                name: "move_part".into(),
                description: "Move a live-board part to x/y mm, optional rotation. Requires open_board."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "reference": { "type": "string", "description": "Refdes." },
                        "x": { "type": "number", "description": "mm." },
                        "y": { "type": "number", "description": "mm." },
                        "rotation": { "type": "number", "description": "Degrees." }
                    },
                    "required": ["reference", "x", "y"]
                }),
            },
            Def {
                name: "route_track".into(),
                description: "Add one straight live-board track: start/end [x,y] mm, width, layer, optional net. Requires open_board."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "start": { "type": "array", "items": {"type":"number"}, "description": "[x, y] mm." },
                        "end": { "type": "array", "items": {"type":"number"}, "description": "[x, y] mm." },
                        "width": { "type": "number", "description": "mm, default 0.2." },
                        "layer": { "type": "string", "description": "F.Cu/B.Cu/etc." },
                        "net": { "type": "string", "description": "Net name." }
                    },
                    "required": ["start", "end"]
                }),
            },
            Def {
                name: "set_net_width".into(),
                description: "Set live-board net class width/clearance for nets. Prefer regenerate_board.rules.net_widths before routing."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "Class name." },
                        "width": { "type": "number", "description": "mm, default 0.5." },
                        "clearance": { "type": "number", "description": "mm, default 0.2." },
                        "nets": { "type": "array", "items": {"type":"string"}, "description": "Net names." }
                    },
                    "required": ["name", "nets"]
                }),
            },
            Def {
                name: "regenerate_board".into(),
                description: "Destructively regenerate/seed the PCB from the committed schematic; not KiCAD F8 sync. May replace existing placement/routing. If footprints are missing/unapplied, fix YAML and apply_design first."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "bounds": {
                            "type": "object",
                            "description": "Board outline rect, mm.",
                            "properties": {
                                "min_x": { "type": "number" }, "max_x": { "type": "number" },
                                "min_y": { "type": "number" }, "max_y": { "type": "number" }
                            }
                        },
                        "rules": {
                            "type": "object",
                            "description": "{layers, net_widths, clearance, min_trace_width, via_diameter, via_drill, pours}. net_widths is {GND: 0.6, V3V3: 0.5} in mm. pours is [{net:'GND', layer:'bottom'}] on top/bottom/innerN signal layers; on 6+ layers omitted pours default to GND/V3V3 power pours when those nets exist.",
                            "properties": {
                                "layers": { "type": "integer", "enum": [2, 4, 6, 8] },
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
                                            "layer": { "type": "string", "description": "top, bottom, or innerN signal layer" }
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
                description: "Return live board parts/summary/state."
                    .into(),
                input_schema: json!({ "type": "object", "properties": {} }),
            },
            Def {
                name: "place_board".into(),
                description: "Auto-place the regenerated board and write placement. Run after regenerate_board."
                    .into(),
                input_schema: json!({ "type": "object", "properties": {} }),
            },
            Def {
                name: "route_board".into(),
                description: "Auto-route the placed board and write copper. Returns failed nets and metrics."
                    .into(),
                input_schema: json!({ "type": "object", "properties": {} }),
            },
            Def {
                name: "autoroute".into(),
                description: "Disabled; use route_board."
                    .into(),
                input_schema: json!({ "type": "object", "properties": {} }),
            },
            Def {
                name: "render_board".into(),
                description: "Render board PNG; view placed/routed. Use when visual inspection is needed."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "view": {
                            "type": "string",
                            "enum": ["placed", "routed"],
                            "description": "placed/routed; omit for auto."
                        }
                    }
                }),
            },
            Def {
                name: "check_board".into(),
                description: "Save live board and run KiCAD PCB DRC; returns violations/unconnected counts."
                    .into(),
                input_schema: json!({ "type": "object", "properties": {} }),
            },
            Def {
                name: "export_fab".into(),
                description: "Export Gerbers/drill/position/BOM fab bundle. Call last after check_board passes."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Input .kicad_pcb; default project board." },
                        "out_dir": { "type": "string", "description": "Output dir; default fab/." }
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
        "assign_footprint" => crate::tools_pcb::assign_footprint(input, ctx),
        "get_board" => crate::tools_pcb::get_board(ctx),
        "place_board" => crate::tools_pcb::place_board(input, ctx),
        "route_board" => crate::tools_pcb::route_board(input, ctx),
        "autoroute" => crate::tools_pcb::autoroute(input, ctx),
        "check_board" => crate::tools_pcb::check_board(input, ctx),
        "export_fab" => crate::tools_pcb::export_fab(input, ctx),
        "open_board" => crate::tools_pcb::open_board(input, ctx),
        "board_state" => crate::tools_pcb::board_state(ctx),
        "move_part" => crate::tools_pcb::move_part(input, ctx),
        "route_track" => crate::tools_pcb::route_track(input, ctx),
        "set_net_width" => crate::tools_pcb::set_net_width(input, ctx),
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
    if let Some((lib_id, pin_count)) = builtin_symbol_alias(&query) {
        return Ok(json!({
            "hits": [{ "lib_id": lib_id, "pin_count": pin_count }],
            "note": "built-in alias/canonical symbol; use it directly and do not repeat this search",
        }));
    }
    if looks_like_pinheader_2pin_symbol_query(&query) {
        let hits: Vec<Value> = ctx
            .index()?
            .search("Conn_01x02", limit)
            .into_iter()
            .map(|h| json!({ "lib_id": h.lib_id, "pin_count": h.pin_count }))
            .collect();
        return Ok(json!({
            "hits": hits,
            "note": "PinHeader_* names are footprints. Use Connector_Generic:Conn_01x02 as the schematic symbol when suitable; use search_footprints for the physical header footprint.",
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

fn looks_like_pinheader_2pin_symbol_query(query: &str) -> bool {
    let q = query.to_ascii_lowercase();
    q.contains("pinheader")
        && (q.contains("2pin")
            || q.contains("2 pin")
            || q.contains("1x02")
            || q.contains("01x02")
            || q.contains("2x1"))
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
            Ok(json!({ "lib_id": lib_id, "pins": pins }))
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
    if let Some(draft) = ctx.workspace().read_draft() {
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
    let yaml = require_str(&input, "yaml")?;
    let result = compile(&yaml, &ctx.provider());
    Ok(compile_report(&result.diagnostics))
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
        None => match ctx.workspace().read_draft() {
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
    let result = compile(&yaml, &ctx.provider());
    let Some(design) = result.design else {
        let mut report = compile_report(&result.diagnostics);
        // `ok` is already false here (errors > 0), but be explicit for the LLM.
        report["ok"] = json!(false);
        return Ok(report);
    };

    // Prior design (for the diff), lifted from the existing schematic.
    let prior_design = if ctx.sch_path().exists() {
        let prior_yaml = lift(ctx.env(), ctx.sch_path())
            .with_context(|| format!("lifting prior {}", ctx.sch_path().display()))?;
        compile(&prior_yaml, &ctx.provider()).design
    } else {
        None
    };

    let diff = design_diff(prior_design.as_ref(), &design);

    let n_blocks = design
        .blocks
        .values()
        .filter(|b| !b.components.is_empty())
        .count();
    let composed_layout = n_blocks >= 2 || needs_fast_schematic_placer(&design);

    // Multi-block and complex single-block commits use the composed-sheet path
    // below, where each refined group is laid out independently. Running those
    // designs through the single-sheet placer first is wasted work and can time
    // out on realistic MCU boards. For preview, compile + diff are enough to
    // gate approval; commit does the actual composed layout.
    let single_sheet_emit = if composed_layout {
        None
    } else {
        let ir = ctx.layout_for(&design);
        Some(
            sch_floorplan::floorplan::emit_strategy(
                ctx.env(),
                &design,
                &ir,
                schematic_placement_engine_for_design(
                    ctx.config().engines.schematic_placer,
                    &design,
                ),
            )
            .context("rendering schematic")?,
        )
    };

    let rendered_len = single_sheet_emit
        .as_ref()
        .map(|emitted| emitted.sch.len())
        .unwrap_or(0);
    let layout_warnings = single_sheet_emit
        .as_ref()
        .map(|emitted| emitted.layout_warnings.clone())
        .unwrap_or_default();
    // Idioms the engine recognized + co-placed (crystal, decoupling, …), surfaced so
    // the LLM can confirm the layout matched its intent — detection is automatic from
    // the netlist, no new authoring syntax.
    let detected_idioms = single_sheet_emit
        .as_ref()
        .and_then(|emitted| serde_json::to_value(&emitted.detected_idioms).ok())
        .unwrap_or(json!([]));

    if !commit {
        return Ok(json!({
            "ok": true,
            "would_write": true,
            "stale_draft_warning": stale,
            "diff": diff,
            "layout_mode": if composed_layout { "composed_blocks" } else { "single_sheet" },
            "rendered_len": rendered_len,
            "layout_warnings": layout_warnings,
            "wire_through_body": single_sheet_emit
                .as_ref()
                .map(|emitted| emitted.crossings.body + emitted.crossings.ic)
                .unwrap_or(0),
            "detected_idioms": detected_idioms,
        }));
    }

    // Commit path: write, then ERC.
    // ANY multi-block design ships as ONE COMPOSED sheet: each functional block is laid out
    // independently (8-9 each), then the block regions are tiled onto a single enlarged page
    // as labeled bounding boxes — per-block independent layout is the RULE, not a dense-only
    // special case. `multisheet::refine_blocks` first normalizes the blocks (split
    // over-crammed, merge tiny) so even a 2-block design lays out per-block with no
    // cross-border global SA; cross-block nets join via matching global labels on the one
    // sheet. Only a single-block design takes the plain single-sheet emit. The composed
    // .kicad_sch is written at ctx.sch_path(); downstream render/ERC operate on it.
    if composed_layout {
        let dir = ctx
            .sch_path()
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."));
        let root = crate::multisheet::compose_single_sheet(
            ctx.env(),
            &design,
            dir,
            ctx.config().engines.schematic_placer,
        )
        .context("composing single-sheet schematic")?;
        if root != ctx.sch_path() {
            std::fs::rename(&root, ctx.sch_path()).with_context(|| {
                format!("placing composed sheet at {}", ctx.sch_path().display())
            })?;
        }
    } else {
        let rendered = single_sheet_emit
            .as_ref()
            .expect("single-sheet commit should have emitted schematic")
            .sch
            .as_str();
        std::fs::write(ctx.sch_path(), rendered)
            .with_context(|| format!("writing {}", ctx.sch_path().display()))?;
    }

    let erc = KicadCli::new(ctx.env())
        .erc(ctx.sch_path())
        .with_context(|| format!("running ERC on {}", ctx.sch_path().display()))?;

    // Record the hash of the just-written schematic (current_sch_text reads the
    // file we wrote above) so the applied draft is no longer flagged stale.
    // No-op when no draft exists (an explicit-yaml apply must not create one).
    if ctx.workspace().read_draft().is_some() {
        ctx.workspace()
            .write_draft(&yaml, current_sch_text(ctx).as_deref())?;
    }

    Ok(json!({
        "ok": true,
        "written": true,
        "path": ctx.sch_path().display().to_string(),
        "stale_draft_warning": stale,
        "diff": diff,
        "layout_mode": if composed_layout { "composed_blocks" } else { "single_sheet" },
        "erc": { "errors": erc.error_count(), "warnings": erc.warning_count() },
        "layout_warnings": layout_warnings,
        "wire_through_body": single_sheet_emit
            .as_ref()
            .map(|emitted| emitted.crossings.body + emitted.crossings.ic)
            .unwrap_or(0),
        "detected_idioms": detected_idioms,
    }))
}

pub(crate) fn schematic_placement_engine(
    engine: SchematicPlacementEngine,
) -> Box<dyn sch_floorplan::contract::PlacementEngine> {
    match engine {
        SchematicPlacementEngine::Anneal => Box::new(anneal_place::Anneal),
        SchematicPlacementEngine::Greedy => Box::new(greedy_place::Greedy),
    }
}

pub(crate) fn schematic_placement_engine_for_design(
    engine: SchematicPlacementEngine,
    design: &Design,
) -> Box<dyn sch_floorplan::contract::PlacementEngine> {
    if engine == SchematicPlacementEngine::Anneal && needs_fast_schematic_placer(design) {
        return Box::new(greedy_place::Greedy);
    }
    schematic_placement_engine(engine)
}

fn needs_fast_schematic_placer(design: &Design) -> bool {
    let (components, pins) = design_complexity(design);
    components >= 10 || pins >= 60
}

fn design_complexity(design: &Design) -> (usize, usize) {
    let components = design
        .blocks
        .values()
        .map(|block| block.components.len())
        .sum();
    let pins = design
        .blocks
        .values()
        .flat_map(|block| block.components.values())
        .map(component_pin_count)
        .sum();
    (components, pins)
}

fn component_pin_count(component: &Component) -> usize {
    component.pins.len()
        + component
            .units
            .values()
            .map(indexmap::IndexMap::len)
            .sum::<usize>()
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

    let violations: Vec<Value> = report
        .violations
        .iter()
        .map(|v| {
            json!({
                "severity": v.severity,
                "type": v.kind,
                "description": v.description,
            })
        })
        .collect();

    Ok(json!({
        "errors": report.error_count(),
        "warnings": report.warning_count(),
        "violations": violations,
    }))
}

// ── 9. create_design / edit_design ────────────────────────────────────────

fn create_design(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let yaml = require_str(&input, "yaml")?;
    let overwrite = input
        .get("overwrite")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if ctx.workspace().read_draft().is_some() && !overwrite {
        return Ok(json!({
            "error": "a draft already exists — pass overwrite=true to replace it, \
                      or use edit_design to modify it",
        }));
    }
    ctx.workspace()
        .write_draft(&yaml, current_sch_text(ctx).as_deref())?;
    let mut report = compile_report(&compile(&yaml, &ctx.provider()).diagnostics);
    report["draft_written"] = json!(true);
    Ok(report)
}

fn edit_design(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let full_yaml = input.get("yaml").and_then(Value::as_str);
    let Some(draft) = ctx.workspace().read_draft() else {
        return Ok(json!({
            "error": "no draft exists — call read_schematic({source:\"draft\"}) (seeds a draft from the \
                      current schematic) or create_design first",
        }));
    };
    if let Some(yaml) = full_yaml {
        ctx.workspace()
            .write_draft(yaml, current_sch_text(ctx).as_deref())?;
        let mut report = compile_report(&compile(yaml, &ctx.provider()).diagnostics);
        report["draft_written"] = json!(true);
        report["mode"] = json!("full_replace");
        return Ok(report);
    }

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
    ctx.workspace()
        .write_draft(&edited, current_sch_text(ctx).as_deref())?;

    let mut report = compile_report(&compile(&edited, &ctx.provider()).diagnostics);
    report["replacements"] = json!(if replace_all { count } else { 1 });
    Ok(report)
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
    let path = ctx.workspace().next_render_path()?;
    std::fs::write(&path, &png).with_context(|| format!("writing {}", path.display()))?;
    let mut obj = json!({
        "ok": true,
        "png_path": path.display().to_string(),
        "note": "image attached; also saved to png_path for the user to open",
    });
    obj[IMAGE_PATH_KEY] = json!(path.display().to_string());
    Ok(obj)
}
