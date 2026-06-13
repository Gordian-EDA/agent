//! PCB-side tools and the persisted board draft (slice 5).
//!
//! `tools.rs` stays the schematic file; the PCB tools live here and are merged
//! into [`crate::tools::Tools::defs`]/`run`. They follow the same house pattern:
//! [`crate::llm::ToolDef`] JSON schemas, free `fn(input, ctx) -> Result<Value>`
//! handlers, `require_str`-style arg handling, and recoverable failures returned
//! as `{"error": …, "suggestions": …}` values rather than `Err`.
//!
//! ## The board draft
//!
//! Board state follows the DRAFT pattern (the mirror of the schematic
//! `draft.circuit.yaml`): a [`BoardDraft`] persisted as `.autopcb/board.json`.
//! It carries the parts (reference, footprint lib_id, per-pad nets, optional
//! locked position), board bounds, design rules, keepouts, placement hints, and
//! the last placement. Tools mutate the draft; `place_board`/`route_board` (Task
//! 2) read it. The LLM never emits trace coordinates — placement positions enter
//! only via `move_part`, snapped/legalized by the engine on the next place.
//!
//! The serde shape reuses pcb-engine types directly ([`PlacementHints`],
//! [`Placement`], [`LockedAt`], [`Rect`], [`LayerRef`]) so a draft round-trips
//! straight into a `PlaceProblem` in Task 2 without a translation layer.

use std::collections::BTreeMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use kicad_bridge::placefp::part_from_footprint;
use pcb_engine::placement::{LockedAt, Placement, PlacementHints};
use pcb_engine::problem::{Bounds, LayerRef};

use crate::tools::{ToolCtx, require_str};

/// Default number of footprint-search hits returned when `limit` is omitted.
/// Mirrors `tools::DEFAULT_SEARCH_LIMIT` for the symbol side.
const DEFAULT_SEARCH_LIMIT: usize = 8;

// ── board draft model ────────────────────────────────────────────────────────

/// The persisted board draft (`.autopcb/board.json`) — the PCB analog of the
/// schematic `draft.circuit.yaml`. Tools mutate it; place/route read it.
///
/// Unknown JSON fields are rejected (`deny_unknown_fields`) so a schema drift
/// fails loudly, matching the engine's `PlaceProblem`/solution types.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BoardDraft {
    /// Board outline (mm, y-down) — the placement/routing extent.
    pub bounds: Bounds,
    /// Board-level design rules (clearance, trace width, via geometry).
    #[serde(default)]
    pub rules: DraftRules,
    /// The parts on the board (footprint + per-pad nets + optional lock).
    pub parts: Vec<DraftPart>,
    /// Rectangular keepouts (routing obstacles; v1 honored by route_board).
    #[serde(default)]
    pub keepouts: Vec<Keepout>,
    /// LLM-authored placement hints (reused verbatim from pcb-engine).
    #[serde(default)]
    pub hints: PlacementHints,
    /// The last placement produced by `place_board`, if any (set in Task 2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_placement: Option<Vec<Placement>>,
}

/// Board-level design rules. Defaults are the engine's own
/// (`PlaceProblem`/`RouteProblem` defaults): 0.2 mm clearance & trace width,
/// 0.6/0.3 mm via diameter/drill — so an omitted `rules` matches what the
/// router and oracle already expect.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DraftRules {
    /// Copper-to-copper clearance (mm); also floors the courtyard margin.
    pub clearance: f64,
    /// Minimum trace width (mm).
    pub min_trace_width: f64,
    /// Via copper diameter (mm).
    pub via_diameter: f64,
    /// Via drill diameter (mm).
    pub via_drill: f64,
}

impl Default for DraftRules {
    fn default() -> Self {
        DraftRules {
            clearance: 0.2,
            min_trace_width: 0.2,
            via_diameter: 0.6,
            via_drill: 0.3,
        }
    }
}

/// One part on the board: a reference, the footprint `Lib:Name` lib_id, the
/// per-pad net assignment (pad number → net name), and an optional locked
/// position (the triage lever, set via `move_part` in Task 2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DraftPart {
    /// Schematic reference designator ("R1", "U2", "J1"). Unique per board.
    pub reference: String,
    /// Fully-qualified footprint id ("Resistor_SMD:R_0603_1608Metric").
    pub footprint: String,
    /// Pad number → net name. A pad absent from the map is left unconnected.
    #[serde(default)]
    pub pad_nets: BTreeMap<String, String>,
    /// If present, the part is pinned here and the engine never moves it
    /// (reuses pcb-engine's [`LockedAt`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locked: Option<LockedAt>,
}

/// A rectangular keepout on a set of copper layers. v1 affects ROUTING only —
/// keepouts become BLOCKED obstacles in the draft→RouteProblem path (Task 2),
/// not placement no-go regions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Keepout {
    /// The keepout rectangle (mm), reusing pcb-engine's [`Rect`].
    pub rect: pcb_engine::placement::Rect,
    /// Copper layers the keepout blocks ("top", "bottom", …).
    pub layers: Vec<LayerRef>,
}

impl BoardDraft {
    /// Load the persisted board draft, if one exists and parses.
    pub fn load(ctx: &ToolCtx) -> Option<BoardDraft> {
        let raw = ctx.workspace().read_board()?;
        serde_json::from_str(&raw).ok()
    }

    /// Persist this draft to `.autopcb/board.json` (pretty-printed for the
    /// human reader, mirroring how the schematic draft stays inspectable).
    pub fn save(&self, ctx: &ToolCtx) -> Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        ctx.workspace().write_board(&json)?;
        Ok(())
    }
}

// ── search_footprints ────────────────────────────────────────────────────────

pub fn search_footprints(input: Value, ctx: &ToolCtx) -> Result<Value> {
    let query = require_str(&input, "query")?;
    let limit = input
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(DEFAULT_SEARCH_LIMIT);

    let hits: Vec<Value> = ctx
        .footprint_index()?
        .search(&query, limit)
        .into_iter()
        .map(|h| json!({ "lib_id": h.lib_id, "pad_count": h.pad_count }))
        .collect();

    Ok(json!({ "hits": hits }))
}

// ── get_footprint_info ───────────────────────────────────────────────────────

pub fn get_footprint_info(input: Value, ctx: &ToolCtx) -> Result<Value> {
    let lib_id = require_str(&input, "lib_id")?;
    let index = ctx.footprint_index()?;

    match index.footprint(&lib_id) {
        Some(fp) => {
            let pads: Vec<Value> = fp
                .pads
                .iter()
                .map(|p| {
                    json!({
                        "number": p.number,
                        "offset": p.at,
                        "size": p.size,
                        "technology": technology_str(p.technology),
                        "layers": p.layers,
                    })
                })
                .collect();
            Ok(json!({
                "lib_id": lib_id,
                "name": fp.name,
                "descr": fp.descr,
                "pads": pads,
                "courtyard": bbox_json(&fp.courtyard),
                "courtyard_source": courtyard_source_str(fp.courtyard_source),
                "bbox": bbox_json(&fp.bbox),
            }))
        }
        None => Ok(json!({
            "error": format!("unknown footprint `{lib_id}`"),
            "suggestions": index.suggest(&lib_id),
        })),
    }
}

/// Render a [`kicad_bridge::footlib::PadTechnology`] as a stable lowercase
/// string for the LLM.
fn technology_str(t: kicad_bridge::footlib::PadTechnology) -> &'static str {
    use kicad_bridge::footlib::PadTechnology::*;
    match t {
        Smd => "smd",
        ThruHole => "thru_hole",
        NpThruHole => "np_thru_hole",
        Other => "other",
    }
}

fn courtyard_source_str(s: kicad_bridge::footlib::CourtyardSource) -> &'static str {
    use kicad_bridge::footlib::CourtyardSource::*;
    match s {
        Crtyd => "crtyd",
        PadSilkFallback => "pad_silk_fallback",
    }
}

fn bbox_json(b: &kicad_bridge::footlib::BBox) -> Value {
    json!({
        "min_x": b.min_x,
        "min_y": b.min_y,
        "max_x": b.max_x,
        "max_y": b.max_y,
        "width": b.width(),
        "height": b.height(),
    })
}

// ── create_board ─────────────────────────────────────────────────────────────

/// Read one required `f64` field from a JSON object, returning a model-readable
/// error message string on failure.
fn req_num(obj: &Value, key: &str, ctx: &str) -> std::result::Result<f64, String> {
    obj.get(key)
        .and_then(Value::as_f64)
        .ok_or_else(|| format!("{ctx}: missing or non-numeric `{key}`"))
}

/// Parse the board `bounds` from snake_case model input into the engine's
/// [`Bounds`] (whose serde is camelCase, so we read fields explicitly rather than
/// deserializing directly — the tool API stays snake_case like the others).
fn parse_bounds(v: Option<&Value>) -> std::result::Result<Bounds, String> {
    let Some(obj) = v else {
        return Err("missing required `bounds` ({min_x, max_x, min_y, max_y} in mm)".into());
    };
    Ok(Bounds {
        min_x: req_num(obj, "min_x", "bounds")?,
        max_x: req_num(obj, "max_x", "bounds")?,
        min_y: req_num(obj, "min_y", "bounds")?,
        max_y: req_num(obj, "max_y", "bounds")?,
    })
}

/// Parse optional `rules` from snake_case model input; an absent/null `rules`
/// yields the engine defaults ([`DraftRules::default`]).
fn parse_rules(v: Option<&Value>) -> std::result::Result<DraftRules, String> {
    let obj = match v {
        None | Some(Value::Null) => return Ok(DraftRules::default()),
        Some(obj) => obj,
    };
    Ok(DraftRules {
        clearance: req_num(obj, "clearance", "rules")?,
        min_trace_width: req_num(obj, "min_trace_width", "rules")?,
        via_diameter: req_num(obj, "via_diameter", "rules")?,
        via_drill: req_num(obj, "via_drill", "rules")?,
    })
}

pub fn create_board(input: Value, ctx: &ToolCtx) -> Result<Value> {
    let overwrite = input.get("overwrite").and_then(Value::as_bool).unwrap_or(false);
    if BoardDraft::load(ctx).is_some() && !overwrite {
        return Ok(json!({
            "error": "a board draft already exists — pass overwrite=true to \
                      replace it (a future move_part/set_constraints tool edits \
                      it in place)",
        }));
    }

    let bounds = match parse_bounds(input.get("bounds")) {
        Ok(b) => b,
        Err(msg) => return Ok(json!({ "error": msg })),
    };

    let rules = match parse_rules(input.get("rules")) {
        Ok(r) => r,
        Err(msg) => return Ok(json!({ "error": msg })),
    };

    let Some(parts_json) = input.get("parts").and_then(Value::as_array) else {
        return Ok(json!({
            "error": "missing required `parts` array (each {reference, footprint, pad_nets})",
        }));
    };

    let index = ctx.footprint_index()?;
    let mut parts: Vec<DraftPart> = Vec::with_capacity(parts_json.len());

    // Resolve every footprint up front; a single unknown lib_id is a recoverable
    // error with suggestions (mirrors get_footprint_info), so the model can fix
    // exactly that part rather than re-sending the whole board.
    for (i, pj) in parts_json.iter().enumerate() {
        let reference = match pj.get("reference").and_then(Value::as_str) {
            Some(r) => r.to_string(),
            None => {
                return Ok(json!({
                    "error": format!("parts[{i}] missing string `reference`"),
                }));
            }
        };
        let footprint = match pj.get("footprint").and_then(Value::as_str) {
            Some(f) => f.to_string(),
            None => {
                return Ok(json!({
                    "error": format!("part {reference}: missing string `footprint` lib_id"),
                }));
            }
        };
        // Resolve the footprint so a typo'd lib_id fails now, with suggestions.
        if index.footprint(&footprint).is_none() {
            return Ok(json!({
                "error": format!(
                    "part {reference}: unknown footprint `{footprint}` — \
                     search_footprints for the real lib_id, never guess it"
                ),
                "suggestions": index.suggest(&footprint),
            }));
        }
        let pad_nets: BTreeMap<String, String> = match pj.get("pad_nets") {
            None | Some(Value::Null) => BTreeMap::new(),
            Some(v) => match serde_json::from_value(v.clone()) {
                Ok(m) => m,
                Err(e) => {
                    return Ok(json!({
                        "error": format!(
                            "part {reference}: pad_nets must map pad number → net name: {e}"
                        ),
                    }));
                }
            },
        };
        parts.push(DraftPart { reference, footprint, pad_nets, locked: None });
    }

    // Validate references are unique (the engine sorts/dedups by reference).
    let mut seen = std::collections::BTreeSet::new();
    for p in &parts {
        if !seen.insert(p.reference.clone()) {
            return Ok(json!({
                "error": format!("duplicate reference `{}` — references must be unique", p.reference),
            }));
        }
    }

    // Build the placement Parts via placefp (reuse, don't fork) so the
    // enclosing-courtyard rule and layer mapping are applied once. We don't keep
    // the Parts here — Task 2 rebuilds them at place time — but building them now
    // surfaces any footprint we can't turn into a part, and lets us derive the
    // net pin counts for the ≥2-pin warning from the SAME geometry place/route
    // will see.
    let net_pins = net_pin_counts(&parts, ctx);

    // Nets with < 2 pins have nothing to connect — surface as warnings (not
    // errors): a board may legitimately carry a test point or a no-connect.
    let warnings: Vec<String> = net_pins
        .iter()
        .filter(|&(_, &count)| count < 2)
        .map(|(net, count)| {
            format!("net `{net}` has only {count} pin — nothing to route (single-pin net)")
        })
        .collect();

    let draft = BoardDraft {
        bounds,
        rules,
        parts,
        keepouts: Vec::new(),
        hints: PlacementHints::default(),
        last_placement: None,
    };
    draft.save(ctx)?;

    Ok(json!({
        "ok": true,
        "board_written": true,
        "part_count": draft.parts.len(),
        "net_count": net_pins.len(),
        "warnings": warnings,
    }))
}

/// Pin count per net across the draft, derived from the resolved footprints via
/// `placefp::part_from_footprint` (the SAME geometry place/route consumes). A
/// pad whose number is absent from `pad_nets` contributes no pin.
fn net_pin_counts(parts: &[DraftPart], ctx: &ToolCtx) -> BTreeMap<String, usize> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let Ok(index) = ctx.footprint_index() else {
        return counts;
    };
    for p in parts {
        let Some(fp) = index.footprint(&p.footprint) else {
            continue;
        };
        let part = part_from_footprint(&fp, &p.reference, &p.pad_nets);
        for pad in &part.pads {
            if let Some(net) = &pad.net {
                *counts.entry(net.clone()).or_default() += 1;
            }
        }
    }
    counts
}

// ── get_board ────────────────────────────────────────────────────────────────

pub fn get_board(ctx: &ToolCtx) -> Result<Value> {
    let Some(draft) = BoardDraft::load(ctx) else {
        return Ok(json!({
            "error": "no board draft yet — call create_board first",
        }));
    };

    let net_pins = net_pin_counts(&draft.parts, ctx);
    let nets: Vec<Value> = net_pins
        .iter()
        .map(|(name, &pins)| json!({ "name": name, "pins": pins }))
        .collect();

    let placed = draft.last_placement.is_some();
    // Routed state lives in a separate workspace file (route.json, Task 2);
    // for now report routed=false unless that file exists.
    let routed = ctx.workspace().read_route().is_some();

    Ok(json!({
        "draft": draft,
        "summary": {
            "part_count": draft.parts.len(),
            "net_count": net_pins.len(),
            "nets": nets,
            "keepout_count": draft.keepouts.len(),
            "placed": placed,
            "routed": routed,
        },
    }))
}
