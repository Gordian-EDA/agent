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

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use kicad_bridge::cli::{KicadCli, Violation};
use kicad_bridge::pcb::{read_problem, write_solution};
use kicad_bridge::placefp::{part_from_footprint, part_from_footprint_layers};
use kicad_bridge::synth::{
    plane_fill_rects, synthesize_board_full, synthesize_board_layers, SynthPart, ZoneSpec,
};
use pcb_engine::connectivity::Violation as ConnViolation;
use pcb_engine::lint::{DrcViolation, lint};
use pcb_engine::pathing::global_route;
use pcb_engine::pipeline::{RouterKind, metrics, route_auto};
use pcb_engine::placement::{
    GroupHint, LockedAt, Part, PlaceProblem, PlaceReport, PlaceResult, Placement, PlacementHints,
    Rect, place_best, to_route_problem,
};
use pcb_engine::problem::{
    Bounds, FailedNet, LayerRef, Obstacle, Point2, RouteProblem, RouteSolution, Via,
};

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
    /// Whether the last placement was geometrically ILLEGAL (courtyard overlap or
    /// out-of-bounds — the board is too tight for the parts). Export refuses an
    /// illegal placement so the engine never ships a board that fails DRC.
    #[serde(default)]
    pub last_place_illegal: bool,
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
    /// Copper layer count (2 or 4). 4 lets dense / fine-pitch parts (BGAs) fan
    /// out their inner pins onto inner layers; 2 is the default for simple boards.
    #[serde(default = "default_layers")]
    pub layer_count: u32,
}

fn default_layers() -> u32 {
    2
}

impl Default for DraftRules {
    fn default() -> Self {
        DraftRules {
            clearance: 0.2,
            min_trace_width: 0.2,
            via_diameter: 0.6,
            via_drill: 0.3,
            layer_count: 2,
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
    let d = DraftRules::default();
    let obj = match v {
        None | Some(Value::Null) => return Ok(d),
        Some(obj) => obj,
    };
    // Partial rules are allowed: any omitted field falls back to the engine
    // default, so a caller can pass just `{ "layers": 4 }` or `{ "clearance": 0.15 }`.
    let num = |k: &str, fallback: f64| obj.get(k).and_then(Value::as_f64).unwrap_or(fallback);
    let layer_count = obj
        .get("layers")
        .or_else(|| obj.get("layer_count"))
        .and_then(Value::as_u64)
        .map(|n| n as u32)
        .unwrap_or(d.layer_count);
    if !matches!(layer_count, 2 | 4) {
        return Err(format!("rules.layers must be 2 or 4, got {layer_count}"));
    }
    let via_diameter = num("via_diameter", d.via_diameter);
    let via_drill = num("via_drill", d.via_drill);
    // Vias must be fabricable to KiCAD's built-in standard-fab minimums (verified
    // against kicad-cli DRC): via ≥ 0.5 mm, drill ≥ 0.3 mm, annular ring ≥ 0.1 mm
    // (i.e. via ≥ drill + 0.2). Below these the board would route but fail KiCAD
    // DRC (via_diameter / drill_out_of_range / annular_width) — reject up front
    // with the floor, rather than silently emit copper that lies about fab.
    if via_diameter < KICAD_MIN_VIA_DIAMETER {
        return Err(format!(
            "rules.via_diameter {via_diameter} is below KiCAD's standard-fab minimum \
             {KICAD_MIN_VIA_DIAMETER}mm — raise it (microvias need custom board rules / a finer fab class)"
        ));
    }
    if via_drill < KICAD_MIN_VIA_DRILL {
        return Err(format!(
            "rules.via_drill {via_drill} is below KiCAD's standard-fab minimum {KICAD_MIN_VIA_DRILL}mm — raise it"
        ));
    }
    if via_diameter - via_drill < 2.0 * KICAD_MIN_ANNULAR {
        return Err(format!(
            "rules via annular ring {:.3}mm (= (via_diameter {via_diameter} − via_drill {via_drill})/2) is below \
             KiCAD's {KICAD_MIN_ANNULAR}mm minimum — widen the via or shrink the drill",
            (via_diameter - via_drill) / 2.0
        ));
    }
    Ok(DraftRules {
        clearance: num("clearance", d.clearance),
        min_trace_width: num("min_trace_width", d.min_trace_width),
        via_diameter,
        via_drill,
        layer_count,
    })
}

/// KiCAD 9 built-in (standard-fab) minimums, verified against `kicad-cli pcb drc`:
/// a via below these trips `via_diameter` / `drill_out_of_range` / `annular_width`.
const KICAD_MIN_VIA_DIAMETER: f64 = 0.5;
const KICAD_MIN_VIA_DRILL: f64 = 0.3;
const KICAD_MIN_ANNULAR: f64 = 0.1;

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
        let Some(resolved_fp) = index.footprint(&footprint) else {
            return Ok(json!({
                "error": format!(
                    "part {reference}: unknown footprint `{footprint}` — \
                     search_footprints for the real lib_id, never guess it"
                ),
                "suggestions": index.suggest(&footprint),
            }));
        };
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
        // Reject a footprint whose own different-net pads sit closer than the
        // board clearance — an inherent clearance DRC fault no routing can fix.
        let viol = kicad_bridge::placefp::pad_clearance_violations(
            &resolved_fp,
            &pad_nets,
            rules.clearance,
        );
        if let Some((a, b, gap)) = viol.first() {
            return Ok(json!({
                "error": format!(
                    "part {reference}: footprint `{footprint}` pads {a} and {b} are only \
                     {gap:.3}mm apart (< the {:.3}mm rules.clearance) — they are on \
                     different nets, so this is a built-in clearance violation. Lower \
                     rules.clearance (e.g. to {:.2}) or use a coarser-pitch footprint.",
                    rules.clearance,
                    (gap - 0.01_f64).max(0.05),
                ),
            }));
        }
        // Optional lock: pin a part at a position/rotation (a mechanically-fixed
        // connector, a rotated part). Accepts {x, y, rotation?} or {at:{x,y}, rotation?}.
        let locked = match pj.get("locked") {
            None | Some(Value::Null) => None,
            Some(l) => {
                let at = l.get("at").unwrap_or(l);
                match (at.get("x").and_then(Value::as_f64), at.get("y").and_then(Value::as_f64)) {
                    (Some(x), Some(y)) => {
                        let raw = l.get("rotation").and_then(Value::as_i64).unwrap_or(0) as i32;
                        let rotation = match axis_aligned_rotation(raw) {
                            Ok(r) => r,
                            Err(e) => {
                                return Ok(json!({ "error": format!("part {reference}: {e}") }));
                            }
                        };
                        Some(LockedAt { at: Point2 { x, y }, rotation })
                    }
                    _ => {
                        return Ok(json!({
                            "error": format!("part {reference}: `locked` needs numeric x and y"),
                        }));
                    }
                }
            }
        };
        parts.push(DraftPart { reference, footprint, pad_nets, locked });
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
        last_place_illegal: false,
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

// ── draft → engine problem ───────────────────────────────────────────────────

/// Build the [`PlaceProblem`] the placement/routing engine consumes from a
/// board draft. Every part's footprint is resolved through
/// [`part_from_footprint`] (the SAME geometry `create_board` validated and the
/// net-pin counts derive from), then the draft's `locked` position is applied so
/// the placer pins it. Design rules ride along as the problem's clearance /
/// trace width / layer count.
///
/// Keepouts are deliberately NOT carried here: in v1 they affect ROUTING only
/// (they become BLOCKED obstacles in [`route_board`]), never placement no-go
/// regions. An unresolvable footprint (e.g. the index lost a lib between
/// `create_board` and now) is returned as an `Err(lib_id)` so the caller can
/// surface a recoverable error naming the part.
fn place_problem_from_draft(
    draft: &BoardDraft,
    ctx: &ToolCtx,
) -> std::result::Result<PlaceProblem, String> {
    let index = ctx
        .footprint_index()
        .map_err(|e| format!("footprint index unavailable: {e}"))?;
    let mut parts: Vec<Part> = Vec::with_capacity(draft.parts.len());
    for dp in &draft.parts {
        let Some(fp) = index.footprint(&dp.footprint) else {
            return Err(format!(
                "part {}: footprint `{}` is no longer resolvable — re-create the board",
                dp.reference, dp.footprint
            ));
        };
        let mut part =
            part_from_footprint_layers(&fp, &dp.reference, &dp.pad_nets, draft.rules.layer_count);
        // The triage lever: a `move_part`-set lock pins the part for the placer.
        part.locked = dp.locked.clone();
        parts.push(part);
    }
    // Keep-outs that block a SIGNAL layer (top/bottom) are placement obstacles too:
    // a part dropped inside one has its pads trapped (no track can leave). Inner-only
    // (plane) keep-outs don't constrain placement, so they're excluded here.
    let keepouts: Vec<pcb_engine::placement::Rect> = draft
        .keepouts
        .iter()
        .filter(|k| {
            k.layers
                .iter()
                .any(|l| *l == LayerRef::top() || *l == LayerRef::bottom())
        })
        .map(|k| k.rect.clone())
        .collect();
    Ok(PlaceProblem {
        bounds: draft.bounds.clone(),
        clearance: draft.rules.clearance,
        layer_count: draft.rules.layer_count,
        min_trace_width: draft.rules.min_trace_width,
        parts,
        keepouts,
    })
}

/// JSON shape for one placed part, returned by `place_board` (and reused as the
/// model's view of `last_placement`).
fn placement_json(p: &Placement) -> Value {
    json!({
        "reference": p.reference,
        "x": p.at.x,
        "y": p.at.y,
        "rotation": p.rotation,
    })
}

// ── place_board ──────────────────────────────────────────────────────────────

/// Whether a part is a board-edge part (connector / header / terminal block /
/// mounting hole) that should hug the perimeter. Detected from the footprint
/// library id, with the conventional `J` reference prefix as a fallback.
fn is_connector(footprint: &str, reference: &str) -> bool {
    let fp = footprint.to_ascii_lowercase();
    fp.contains("connector")
        || fp.contains("pinheader")
        || fp.contains("pinsocket")
        || fp.contains("terminalblock")
        || fp.contains("screwterminal")
        || fp.contains("mountinghole")
        || fp.contains("usb")
        || fp.contains("barreljack")
        || reference.starts_with('J')
}

/// A mounting hole / mechanical fixing: pulled to a board CORNER (not just an
/// edge), where a screw clears the components. Checked before [`is_connector`]
/// (which also matches mounting holes) so these corner-seek rather than edge-seek.
fn is_mounting_hole(footprint: &str) -> bool {
    footprint.to_ascii_lowercase().contains("mountinghole")
}

/// Validate a part rotation is axis-aligned (0/90/180/270), normalizing to
/// `[0,360)`. The placer + synth support only these; rejecting others HERE (at the
/// agent surface) fails fast with a clear message, instead of routing to wrong pad
/// positions (`rotate_offset` is identity for non-axis angles) and failing late at
/// export.
fn axis_aligned_rotation(rot: i32) -> std::result::Result<i32, String> {
    let r = rot.rem_euclid(360);
    if matches!(r, 0 | 90 | 180 | 270) {
        Ok(r)
    } else {
        Err(format!(
            "rotation {rot}° is not supported — use 0, 90, 180, or 270 \
             (the engine places axis-aligned parts only)"
        ))
    }
}

pub fn place_board(_input: Value, ctx: &ToolCtx) -> Result<Value> {
    let Some(mut draft) = BoardDraft::load(ctx) else {
        return Ok(json!({
            "error": "no board draft yet — call create_board first",
        }));
    };

    let mut problem = match place_problem_from_draft(&draft, ctx) {
        Ok(p) => p,
        Err(msg) => return Ok(json!({ "error": msg })),
    };

    // Auto edge-affinity: pull connectors/headers to their nearest board edge so
    // they land at the perimeter (where a cable or the enclosure reaches them),
    // not stranded in the interior with copper wrapping around them. Skip any
    // part the model already steered with an explicit group `edge` hint.
    let mut hints = draft.hints.clone();
    let explicitly_edged: std::collections::BTreeSet<&str> = hints
        .groups
        .iter()
        .filter(|g| g.edge.is_some())
        .flat_map(|g| g.members.iter().map(String::as_str))
        .collect();
    for p in &draft.parts {
        if explicitly_edged.contains(p.reference.as_str()) {
            continue;
        }
        // Mounting holes corner-seek (mechanical fixings at the board corners) AND
        // edge-seek (so any hole the corner post-pass can't seat — a corner taken
        // or blocked by a part — falls back to the perimeter, not the interior).
        // Other connectors/headers edge-seek (a cable/enclosure reaches the edge).
        if is_mounting_hole(&p.footprint) {
            if !hints.corner_seek.contains(&p.reference) {
                hints.corner_seek.push(p.reference.clone());
            }
            if !hints.edge_seek.contains(&p.reference) {
                hints.edge_seek.push(p.reference.clone());
            }
        } else if is_connector(&p.footprint, &p.reference)
            && !hints.edge_seek.contains(&p.reference)
        {
            hints.edge_seek.push(p.reference.clone());
        }
    }

    // Tile any `grid` group (repetitive array) by locking its members at grid cells
    // before the annealer runs, so it lays out the rest around the tidy array.
    pcb_engine::placement::apply_grid_hints(&mut problem, &hints);

    let result = place_best(&problem, &hints);

    // Persist the placement into the draft so route_board / render_board / a
    // later get_board can read it without re-running the placer.
    draft.last_placement = Some(result.placements.clone());
    draft.last_place_illegal = !result.legal;
    draft.save(ctx)?;

    let positions: Vec<Value> = result.placements.iter().map(placement_json).collect();

    // Discoverability: if a legal placement has a decoupling-heavy IC whose caps the
    // annealer scattered (>=4 bypass caps, none locked/pinned), suggest the `surround`
    // hint so the agent can ring them into a tidy decoupling cluster.
    let mut hint_suggestions: Vec<Value> = Vec::new();
    if result.legal {
        let pairs = pcb_engine::placement::decoupling_pairs(&problem);
        let mut by_ic: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for (cap, ic) in pairs {
            by_ic.entry(ic).or_default().push(cap);
        }
        for (ic, caps) in by_ic {
            if caps.len() >= 4 && caps.iter().all(|&c| problem.parts[c].locked.is_none()) {
                let ic_ref = problem.parts[ic].reference.clone();
                let cap_refs: Vec<String> =
                    caps.iter().map(|&c| problem.parts[c].reference.clone()).collect();
                hint_suggestions.push(json!({
                    "type": "surround",
                    "target": ic_ref,
                    "members": cap_refs,
                    "note": format!(
                        "{ic_ref} has {} decoupling caps the placer scattered. For a tidy \
                         ring: move_part to lock {ic_ref} at a position, then set_placement_hints \
                         with a group {{\"members\":[<the caps>],\"surround\":\"{ic_ref}\"}}, then \
                         place_board again.",
                        caps.len()
                    ),
                }));
            }
        }
    }

    // On a failed placement, give the agent a CONCRETE minimum board size so it can
    // retry deterministically instead of guessing. Estimate from the parts' total
    // courtyard area (with packing + routing overhead) and the largest single part.
    let mut extra = json!({});
    if !result.legal {
        let total_area: f64 = problem
            .parts
            .iter()
            .map(|p| p.courtyard_w * p.courtyard_h)
            .sum();
        let max_w = problem.parts.iter().map(|p| p.courtyard_w).fold(0.0, f64::max);
        let max_h = problem.parts.iter().map(|p| p.courtyard_h).fold(0.0, f64::max);
        let cw = (problem.bounds.max_x - problem.bounds.min_x).max(0.1);
        let ch = (problem.bounds.max_y - problem.bounds.min_y).max(0.1);
        // ~2x the courtyard area leaves room for spacing, refdes gaps, and routing;
        // but ALSO grow 1.3x past the current bounds, so if the caller already gave
        // generous-but-still-failing bounds (large parts the legalizer can't
        // separate) each retry with the suggestion converges instead of looping.
        // Keep the caller's aspect ratio; never below the biggest part + a margin.
        let min_area = (total_area * 2.0).max(cw * ch * 1.3);
        let aspect = cw / ch;
        let mut sh = (min_area / aspect).sqrt();
        let mut sw = aspect * sh;
        sw = sw.max(max_w + 2.0);
        sh = sh.max(max_h + 2.0);
        extra = json!({
            "parts_courtyard_area_mm2": (total_area * 10.0).round() / 10.0,
            "current_bounds_mm": { "w": (cw * 10.0).round() / 10.0, "h": (ch * 10.0).round() / 10.0 },
            "suggested_min_bounds_mm": { "w": sw.ceil(), "h": sh.ceil() },
        });
    }

    let mut out = json!({
        "legal": result.legal,
        "hpwl": result.report.hpwl,
        "overlaps_resolved": result.report.overlaps_resolved,
        "out_of_bounds_clamps": result.report.out_of_bounds_clamps,
        "positions": positions,
        "note": if result.legal {
            "placement is legal (no courtyard overlap, all parts in bounds). \
             Call route_board next, or render_board to see it."
        } else {
            "placement is NOT legal — the board is too tight for these parts. \
             See suggested_min_bounds_mm: re-run create_board with at least that \
             bounds (it keeps your aspect ratio), or relax rules / move-unlock parts."
        },
    });
    if let (Value::Object(o), Value::Object(e)) = (&mut out, extra) {
        o.extend(e);
    }
    if !hint_suggestions.is_empty() {
        out["hint_suggestions"] = Value::Array(hint_suggestions);
    }
    Ok(out)
}

// ── set_placement_hints ──────────────────────────────────────────────────────

/// Parse one group hint from snake_case model input, validating its members
/// against the draft's known references. Returns the engine [`GroupHint`] on
/// success or a model-readable error string.
fn parse_group_hint(v: &Value, known_refs: &[&str]) -> std::result::Result<GroupHint, String> {
    let name = v
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| "each group needs a string `name`".to_string())?
        .to_string();
    let members_json = v
        .get("members")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("group `{name}`: needs a `members` array of references"))?;
    let mut members = Vec::with_capacity(members_json.len());
    for m in members_json {
        let r = m
            .as_str()
            .ok_or_else(|| format!("group `{name}`: members must be reference strings"))?;
        if !known_refs.contains(&r) {
            return Err(format!(
                "group `{name}`: member `{r}` is not a part on this board — known references: {}",
                known_refs.join(", ")
            ));
        }
        members.push(r.to_string());
    }
    // region / edge are optional; reuse the engine serde so the vocabulary stays
    // single-sourced (these are camelCase-or-simple shapes the LLM can author).
    let region = match v.get("region") {
        None | Some(Value::Null) => None,
        Some(r) => Some(parse_rect(r).map_err(|e| format!("group `{name}`: region {e}"))?),
    };
    let edge = match v.get("edge") {
        None | Some(Value::Null) => None,
        Some(e) => Some(parse_edge(e).map_err(|err| format!("group `{name}`: {err}"))?),
    };
    let grid = v.get("grid").and_then(Value::as_bool).unwrap_or(false);
    if grid && region.is_none() {
        return Err(format!("group `{name}`: `grid` requires a `region` to tile into"));
    }
    let surround = match v.get("surround") {
        None | Some(Value::Null) => None,
        Some(s) => {
            let r = s
                .as_str()
                .ok_or_else(|| format!("group `{name}`: `surround` must be a part-reference string"))?;
            if !known_refs.contains(&r) {
                return Err(format!(
                    "group `{name}`: surround target `{r}` is not a part on this board"
                ));
            }
            Some(r.to_string())
        }
    };
    Ok(GroupHint {
        name,
        members,
        region,
        edge,
        grid,
        surround,
    })
}

/// Parse a `{min_x,max_x,min_y,max_y}` rect from snake_case model input.
fn parse_rect(v: &Value) -> std::result::Result<Rect, String> {
    Ok(Rect {
        min_x: req_num(v, "min_x", "rect")?,
        max_x: req_num(v, "max_x", "rect")?,
        min_y: req_num(v, "min_y", "rect")?,
        max_y: req_num(v, "max_y", "rect")?,
    })
}

/// Parse an edge hint ("n"/"s"/"e"/"w", case-insensitive).
fn parse_edge(v: &Value) -> std::result::Result<pcb_engine::placement::Edge, String> {
    use pcb_engine::placement::Edge;
    let s = v
        .as_str()
        .ok_or_else(|| "edge must be a string \"n\"/\"s\"/\"e\"/\"w\"".to_string())?;
    match s.to_ascii_lowercase().as_str() {
        "n" => Ok(Edge::N),
        "s" => Ok(Edge::S),
        "e" => Ok(Edge::E),
        "w" => Ok(Edge::W),
        other => Err(format!("edge must be \"n\"/\"s\"/\"e\"/\"w\", got {other:?}")),
    }
}

pub fn set_placement_hints(input: Value, ctx: &ToolCtx) -> Result<Value> {
    let Some(mut draft) = BoardDraft::load(ctx) else {
        return Ok(json!({
            "error": "no board draft yet — call create_board first",
        }));
    };

    let Some(groups_json) = input.get("groups").and_then(Value::as_array) else {
        return Ok(json!({
            "error": "missing required `groups` array (each {name, members, region?, edge?})",
        }));
    };

    let known_refs: Vec<&str> = draft.parts.iter().map(|p| p.reference.as_str()).collect();
    let mut groups = Vec::with_capacity(groups_json.len());
    for g in groups_json {
        match parse_group_hint(g, &known_refs) {
            Ok(gh) => groups.push(gh),
            Err(msg) => return Ok(json!({ "error": msg })),
        }
    }

    let summary: Vec<Value> = groups
        .iter()
        .map(|g| {
            json!({
                "name": g.name,
                "members": g.members,
                "region": g.region.is_some(),
                "edge": g.edge.is_some(),
            })
        })
        .collect();

    draft.hints = PlacementHints { groups, ..Default::default() };
    draft.save(ctx)?;

    Ok(json!({
        "ok": true,
        "group_count": summary.len(),
        "groups": summary,
        "note": "hints stored; they steer the NEXT place_board (they improve, never gate).",
    }))
}

// ── set_constraints ──────────────────────────────────────────────────────────

/// Validate a keepout rectangle lies within the board bounds and lists only
/// known copper layers, then return the engine [`Keepout`].
fn parse_keepout(
    v: &Value,
    bounds: &Bounds,
    layer_count: u32,
    idx: usize,
) -> std::result::Result<Keepout, String> {
    let ctxstr = format!("keepouts[{idx}]");
    let rect_v = v
        .get("rect")
        .ok_or_else(|| format!("{ctxstr}: missing `rect` {{min_x,max_x,min_y,max_y}}"))?;
    let rect = parse_rect(rect_v).map_err(|e| format!("{ctxstr}: rect {e}"))?;
    if rect.min_x >= rect.max_x || rect.min_y >= rect.max_y {
        return Err(format!("{ctxstr}: rect is degenerate (min must be < max)"));
    }
    // Within bounds (a keepout outside the board is almost certainly a mistake).
    if rect.min_x < bounds.min_x - 1e-9
        || rect.max_x > bounds.max_x + 1e-9
        || rect.min_y < bounds.min_y - 1e-9
        || rect.max_y > bounds.max_y + 1e-9
    {
        return Err(format!(
            "{ctxstr}: rect [{},{}]x[{},{}] is outside the board bounds [{},{}]x[{},{}]",
            rect.min_x, rect.max_x, rect.min_y, rect.max_y,
            bounds.min_x, bounds.max_x, bounds.min_y, bounds.max_y
        ));
    }
    let layers_json = v
        .get("layers")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{ctxstr}: missing `layers` array (e.g. [\"top\",\"bottom\"])"))?;
    if layers_json.is_empty() {
        return Err(format!("{ctxstr}: `layers` must name at least one copper layer"));
    }
    let mut layers = Vec::with_capacity(layers_json.len());
    for l in layers_json {
        let name = l
            .as_str()
            .ok_or_else(|| format!("{ctxstr}: layer names must be strings"))?;
        let layer = LayerRef(name.to_string());
        // Resolve against THIS board's stackup: "top"/"bottom" always, plus
        // "inner1".."inner{layer_count-2}" on a multilayer board.
        if layer.index(layer_count).is_none() {
            let inners = if layer_count >= 4 {
                format!(
                    ", \"inner1\"..\"inner{}\"",
                    layer_count - 2
                )
            } else {
                String::new()
            };
            return Err(format!(
                "{ctxstr}: unknown layer `{name}` — this board has \"top\", \"bottom\"{inners}"
            ));
        }
        layers.push(layer);
    }
    Ok(Keepout { rect, layers })
}

pub fn set_constraints(input: Value, ctx: &ToolCtx) -> Result<Value> {
    let Some(mut draft) = BoardDraft::load(ctx) else {
        return Ok(json!({
            "error": "no board draft yet — call create_board first",
        }));
    };

    // Net classes are vocabulary-reserved but honestly rejected: the router does
    // not honor them yet, so accepting them silently would lie about the present.
    if input.get("net_classes").is_some_and(|v| !v.is_null()) {
        return Ok(json!({
            "error": "net classes are reserved but not yet supported by the router \
                      — use rules/keepouts",
        }));
    }

    // Merge rules: a partial `rules` object updates only the fields it names, so
    // the model can tweak clearance alone without re-sending the whole block.
    let mut rules_changed = false;
    if let Some(r) = input.get("rules").filter(|v| !v.is_null()) {
        if let Some(n) = r.get("clearance").and_then(Value::as_f64) {
            draft.rules.clearance = n;
            rules_changed = true;
        }
        if let Some(n) = r.get("min_trace_width").and_then(Value::as_f64) {
            draft.rules.min_trace_width = n;
            rules_changed = true;
        }
        if let Some(n) = r.get("via_diameter").and_then(Value::as_f64) {
            draft.rules.via_diameter = n;
            rules_changed = true;
        }
        if let Some(n) = r.get("via_drill").and_then(Value::as_f64) {
            draft.rules.via_drill = n;
            rules_changed = true;
        }
    }

    // Replace keepouts wholesale when present (validated against bounds/layers).
    let mut keepouts_changed = false;
    if let Some(ko) = input.get("keepouts").filter(|v| !v.is_null()) {
        let Some(arr) = ko.as_array() else {
            return Ok(json!({
                "error": "keepouts must be an array of {rect, layers}",
            }));
        };
        let mut keepouts = Vec::with_capacity(arr.len());
        for (i, k) in arr.iter().enumerate() {
            match parse_keepout(k, &draft.bounds, draft.rules.layer_count, i) {
                Ok(keepout) => keepouts.push(keepout),
                Err(msg) => return Ok(json!({ "error": msg })),
            }
        }
        draft.keepouts = keepouts;
        keepouts_changed = true;
    }

    if !rules_changed && !keepouts_changed {
        return Ok(json!({
            "error": "nothing to set — pass `rules` (partial updates allowed) \
                      and/or `keepouts`",
        }));
    }

    // Rules/keepouts do not MOVE parts, so the placement stays valid — but the
    // copper does not: clearance/keepout changes invalidate any routed solution.
    // Drop the stored route.json so a stale route can't be exported.
    let route_cleared = clear_route(ctx)?;

    draft.save(ctx)?;

    Ok(json!({
        "ok": true,
        "rules": {
            "clearance": draft.rules.clearance,
            "min_trace_width": draft.rules.min_trace_width,
            "via_diameter": draft.rules.via_diameter,
            "via_drill": draft.rules.via_drill,
        },
        "keepout_count": draft.keepouts.len(),
        "placement_kept": draft.last_placement.is_some(),
        "route_cleared": route_cleared,
        "note": "rules/keepouts don't move parts so the placement stands, but the \
                 routing was invalidated — run route_board again before export.",
    }))
}

/// Remove the stored route solution (if any) so a state change can't leave a
/// stale `route.json` behind. Returns whether a route was actually cleared.
fn clear_route(ctx: &ToolCtx) -> Result<bool> {
    if ctx.workspace().read_route().is_none() {
        return Ok(false);
    }
    std::fs::remove_file(ctx.workspace().route_path())?;
    Ok(true)
}

// ── move_part / unlock_part ──────────────────────────────────────────────────

pub fn move_part(input: Value, ctx: &ToolCtx) -> Result<Value> {
    let Some(mut draft) = BoardDraft::load(ctx) else {
        return Ok(json!({
            "error": "no board draft yet — call create_board first",
        }));
    };
    let reference = require_str(&input, "reference")?;
    let x = match input.get("x").and_then(Value::as_f64) {
        Some(v) => v,
        None => return Ok(json!({ "error": "missing or non-numeric `x`" })),
    };
    let y = match input.get("y").and_then(Value::as_f64) {
        Some(v) => v,
        None => return Ok(json!({ "error": "missing or non-numeric `y`" })),
    };
    let rotation = match axis_aligned_rotation(
        input.get("rotation").and_then(Value::as_i64).map(|r| r as i32).unwrap_or(0),
    ) {
        Ok(r) => r,
        Err(e) => return Ok(json!({ "error": e })),
    };

    // A part origin must sit within the board (an out-of-bounds nudge is a model
    // mistake; the engine would clamp it silently, hiding the error). We check
    // the ORIGIN against bounds — the placer legalizes the courtyard on the next
    // place_board, but the origin itself must be on the board.
    let b = &draft.bounds;
    if x < b.min_x || x > b.max_x || y < b.min_y || y > b.max_y {
        return Ok(json!({
            "error": format!(
                "({x}, {y}) is outside the board bounds [{},{}]x[{},{}] — pick a point on the board",
                b.min_x, b.max_x, b.min_y, b.max_y
            ),
        }));
    }

    let Some(part) = draft.parts.iter_mut().find(|p| p.reference == reference) else {
        let known: Vec<&str> = draft.parts.iter().map(|p| p.reference.as_str()).collect();
        return Ok(json!({
            "error": format!(
                "unknown reference `{reference}` — known references: {}",
                known.join(", ")
            ),
        }));
    };
    part.locked = Some(LockedAt {
        at: Point2 { x, y },
        rotation,
    });

    // State changed: the stored route is stale, and the placement no longer
    // reflects the lock until place_board re-runs.
    let route_cleared = clear_route(ctx)?;
    draft.last_placement = None;
    draft.save(ctx)?;

    Ok(json!({
        "ok": true,
        "reference": reference,
        "locked_at": { "x": x, "y": y, "rotation": rotation },
        "route_cleared": route_cleared,
        "note": "part pinned here; run place_board (it legalizes around the lock) \
                 then route_board.",
    }))
}

pub fn unlock_part(input: Value, ctx: &ToolCtx) -> Result<Value> {
    let Some(mut draft) = BoardDraft::load(ctx) else {
        return Ok(json!({
            "error": "no board draft yet — call create_board first",
        }));
    };
    let reference = require_str(&input, "reference")?;

    let Some(part) = draft.parts.iter_mut().find(|p| p.reference == reference) else {
        let known: Vec<&str> = draft.parts.iter().map(|p| p.reference.as_str()).collect();
        return Ok(json!({
            "error": format!(
                "unknown reference `{reference}` — known references: {}",
                known.join(", ")
            ),
        }));
    };
    let was_locked = part.locked.is_some();
    part.locked = None;

    let route_cleared = clear_route(ctx)?;
    draft.last_placement = None;
    draft.save(ctx)?;

    Ok(json!({
        "ok": true,
        "reference": reference,
        "was_locked": was_locked,
        "route_cleared": route_cleared,
        "note": "part released; run place_board to re-place it freely.",
    }))
}

// ── route_board ──────────────────────────────────────────────────────────────

/// Inject the draft's keepouts into a [`RouteProblem`] as BLOCKED_ALL obstacles.
///
/// We extend the engine's `to_route_problem` output at the agent layer (rather
/// than forking `to_route_problem`): each keepout becomes an [`Obstacle`] with an
/// EMPTY `connected_to`, which the lint/router treat as unowned copper — blocking
/// EVERY net on the listed layers. A keepout rect maps to a centered rect
/// obstacle per the rect's geometry.
fn inject_keepouts(rp: &mut RouteProblem, keepouts: &[Keepout]) {
    for ko in keepouts {
        let center = Point2 {
            x: (ko.rect.min_x + ko.rect.max_x) / 2.0,
            y: (ko.rect.min_y + ko.rect.max_y) / 2.0,
        };
        rp.obstacles.push(Obstacle {
            kind: "rect".to_owned(),
            layers: ko.layers.clone(),
            center,
            width: ko.rect.max_x - ko.rect.min_x,
            height: ko.rect.max_y - ko.rect.min_y,
            // No owner ⇒ blocks all nets (a true keepout).
            connected_to: Vec::new(),
        });
    }
}

/// A `lint_summary` for a routed solution, split into two buckets.
///
/// The connectivity oracle flags an `Unconnected` violation for EVERY net the
/// router honestly dropped — but a `route_auto` failure is not an engine bug, it
/// is the board being too tight, exactly what the model triages. So we classify:
///
/// - **expected gaps** — `Connectivity::Unconnected` whose net is in the
///   already-reported `failed` set. These are the honest finisher/global drops;
///   they are NOT engine bugs (the model already sees them in `failed`).
/// - **real violations** — geometry defects (clearance, width, via, bounds,
///   invalid layer), any `CrossNetMerge` (a short — always a bug), and any
///   `Unconnected` on a net the router claimed to ROUTE. A non-zero real count
///   means a violation escaped the router's own oracles: an engine bug.
///
/// Returns `(by_kind_of_real_violations, real_count, expected_gap_count)`.
struct LintSplit {
    /// Counts of REAL violations by serde `kind` tag (empty ⇒ clean copper).
    by_kind: Value,
    /// Number of real violations (the engine-bug signal; should be 0).
    real: usize,
    /// Number of expected connectivity gaps from already-failed nets.
    expected_gaps: usize,
}

fn lint_summary(
    rp: &RouteProblem,
    solution: &pcb_engine::problem::RouteSolution,
    failed: &[FailedNet],
) -> LintSplit {
    let failed_nets: std::collections::BTreeSet<&str> =
        failed.iter().map(|f| f.connection.as_str()).collect();

    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut real = 0usize;
    let mut expected_gaps = 0usize;

    for v in lint(rp, solution) {
        // An Unconnected on a net the router already reported as failed is the
        // expected gap, not a bug.
        if let DrcViolation::Connectivity {
            violation: ConnViolation::Unconnected { connection, .. },
        } = &v
            && failed_nets.contains(connection.as_str())
        {
            expected_gaps += 1;
            continue;
        }
        // Everything else is a real violation the router should have prevented.
        let kind = serde_json::to_value(&v)
            .ok()
            .and_then(|j| j.get("kind").and_then(Value::as_str).map(str::to_owned))
            .unwrap_or_else(|| "unknown".to_owned());
        *counts.entry(kind).or_default() += 1;
        real += 1;
    }

    let by_kind: Value = counts
        .into_iter()
        .map(|(k, c)| (k, json!(c)))
        .collect::<serde_json::Map<_, _>>()
        .into();
    LintSplit {
        by_kind,
        real,
        expected_gaps,
    }
}

/// Build the congestion-hotspot enrichment for a FAILED route. `route_auto` does
/// not expose the global stage's congestion report, so we re-run the global
/// router here ONLY on failure to recover the hotspots/iterations for the model's
/// triage. NOTE (duplication cost): this repeats the global-routing pass that
/// `route_auto` already ran internally; it is paid only on the failure path, on
/// boards small enough that one extra global route is cheap. If `route_auto`
/// later surfaces the congestion report directly, drop this.
fn congestion_json(rp: &RouteProblem) -> Value {
    let g = global_route(rp);
    let hotspots: Vec<Value> = g
        .report
        .edge_hotspots
        .iter()
        .map(|h| {
            json!({
                "edge": h.edge,
                "leaves": [h.a, h.b],
                "layer": h.layer,
                "usage": h.usage,
                "capacity": h.capacity,
                "load": h.load,
            })
        })
        .collect();
    json!({
        "iterations": g.report.iterations,
        "final_overflow": g.report.final_overflow,
        "hotspots": hotspots,
    })
}

pub fn route_board(_input: Value, ctx: &ToolCtx) -> Result<Value> {
    let Some(draft) = BoardDraft::load(ctx) else {
        return Ok(json!({
            "error": "no board draft yet — call create_board first",
        }));
    };
    let Some(placements) = draft.last_placement.clone() else {
        return Ok(json!({
            "error": "the board is not placed — run place_board first, then route_board",
        }));
    };

    let problem = match place_problem_from_draft(&draft, ctx) {
        Ok(p) => p,
        Err(msg) => return Ok(json!({ "error": msg })),
    };

    // Placement → RouteProblem, then inject the keepouts as BLOCKED_ALL obstacles
    // (the one place v1 keepouts bite: routing, not placement).
    let mut rp = to_route_problem(&problem, &placements);
    inject_keepouts(&mut rp, &draft.keepouts);

    // On a multilayer board the highest-fanout power/ground nets become copper
    // PLANES (emitted as zones at export) instead of point-to-point traces — the
    // only way a dense part's many power pins connect. Signals route on the outer
    // pair; each plane pad is stitched up to its plane with a through-via.
    let planes = assign_planes(&draft);
    let result = if planes.is_empty() {
        route_auto(&rp)
    } else {
        let plane_names: std::collections::BTreeSet<String> =
            planes.iter().map(|(n, _)| n.clone()).collect();
        route_with_planes(rp.clone(), &plane_names, &draft.rules)
    };

    // Persist the full solution + failures + router for export (Task 4) and the
    // routed flag (get_board). The solution lives in the workspace; only a small
    // summary goes back to the model.
    let stored = json!({
        "solution": result.solution,
        "failed": result.failed,
        "router": result.router,
        "planes": planes,
    });
    ctx.workspace().write_route(&serde_json::to_string_pretty(&stored)?)?;

    let failed: Vec<Value> = result
        .failed
        .iter()
        .map(|f| json!({ "connection": f.connection, "reason": f.reason }))
        .collect();

    let m = metrics(&result.solution);
    let split = lint_summary(&rp, &result.solution, &result.failed);

    let router = match result.router {
        RouterKind::Naive => "naive",
        RouterKind::Detailed => "detailed",
    };

    let mut out = json!({
        "router": router,
        "failed": failed,
        "metrics": {
            "wirelength": m.wirelength,
            "vias": m.via_count,
            "traces": m.trace_count,
        },
        // lint_summary counts only REAL violations (geometry/short/unexpected
        // gaps). Expected connectivity gaps from already-failed nets are NOT here
        // — those are the honest failures the model triages, listed in `failed`.
        "lint_summary": split.by_kind,
    });

    // A non-zero REAL lint on the routed solution means a violation slipped past
    // the router's own oracles — surface it LOUDLY (it is an engine bug, not a
    // board problem the model can fix). An expected gap from a failed net is NOT
    // an engine bug; it flows to the triage branch below.
    if split.real > 0 {
        let real = split.real;
        out["engine_bug"] = json!(true);
        out["note"] = json!(format!(
            "ENGINE BUG: the routed copper has {real} DRC violation(s) that the \
             router's oracles should have caught (over and above the {} expected \
             gap(s) from failed nets) — this is not a board you can fix by triage; \
             report it. Counts by kind are in lint_summary.",
            split.expected_gaps
        ));
    } else if result.failed.is_empty() {
        out["note"] = json!("routed cleanly: zero failed nets, lint clean. Ready to export.");
    } else {
        // Honest failures: enrich with congestion hotspots so the model can triage
        // (move a part off a hot edge, relax a rule, or drop a keepout).
        out["congestion"] = congestion_json(&rp);
        out["note"] = json!(
            "some nets did not route — read each failure `reason` (global:/assign:/cell:/\
             finisher: provenance) and the congestion hotspots, then triage: move_part to \
             relieve a hot region, relax rules via set_constraints, or remove a blocking \
             keepout. Re-place and re-route after each change."
        );
    }

    Ok(out)
}

// ── render_board ─────────────────────────────────────────────────────────────

/// Stored route: the full `route.json` shape — solution + failures + router
/// tag. This mirrors the JSON `route_board` persists so we can recover the
/// rendered state without re-routing.
#[derive(Debug, Deserialize)]
struct StoredRoute {
    solution: RouteSolution,
    failed: Vec<FailedNet>,
    // router field is present in the JSON but we only need it for the key;
    // its value is a RouterKind enum that serde handles fine.
    #[allow(dead_code)]
    router: serde_json::Value,
    /// Power-plane assignment chosen at route time: (net name, copper layer index).
    /// Empty for ordinary (2-layer / no-plane) boards. Export emits these as zones.
    #[serde(default)]
    planes: Vec<(String, u32)>,
}

/// Minimum pin count for a net to become a copper plane (a power/ground rail) on
/// a multilayer board, rather than being routed as point-to-point traces.
const PLANE_MIN_PINS: usize = 5;

/// Choose copper-plane nets for a multilayer board: the highest-fanout nets
/// (>= [`PLANE_MIN_PINS`] pins) get the inner copper layers (In1, In2, …), so a
/// dense part's many power/ground pins connect through a plane instead of traces
/// the grid router cannot fan out. Returns `(net, layer_index)`; empty unless the
/// board has >= 4 copper layers (2-layer boards keep the all-traces flow).
fn assign_planes(draft: &BoardDraft) -> Vec<(String, u32)> {
    if draft.rules.layer_count < 4 {
        return Vec::new();
    }
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for p in &draft.parts {
        for net in p.pad_nets.values() {
            if !net.is_empty() {
                *counts.entry(net.clone()).or_default() += 1;
            }
        }
    }
    let mut nets: Vec<(String, usize)> =
        counts.into_iter().filter(|(_, c)| *c >= PLANE_MIN_PINS).collect();
    // Highest fanout first; ties by name for determinism.
    nets.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let inner = (draft.rules.layer_count - 2) as usize;
    nets.into_iter()
        .take(inner)
        .enumerate()
        .map(|(i, (net, _))| (net, (i + 1) as u32))
        .collect()
}

/// Route a plane board's SIGNAL nets only. Power/ground go to inner-layer copper
/// planes (emitted as zones at export), so here we: route everything else on the
/// OUTER pair (`layer_count = 2`, so the router's `top`/`bottom` map to F.Cu/B.Cu
/// on the real board and the inner layers stay clear for planes); reserve each
/// plane pad's through-via column (block it on both outer layers); and stitch
/// every plane pad up to its plane with a through-via on the plane net.
/// Distance from point `p` to segment `a`–`b` (mm) — used to keep a plane
/// stitching via clear of routed signal tracks.
fn seg_point_dist(a: &Point2, b: &Point2, p: &Point2) -> f64 {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let len2 = dx * dx + dy * dy;
    if len2 < 1e-12 {
        return ((p.x - a.x).powi(2) + (p.y - a.y).powi(2)).sqrt();
    }
    let t = (((p.x - a.x) * dx + (p.y - a.y) * dy) / len2).clamp(0.0, 1.0);
    let (cx, cy) = (a.x + t * dx, a.y + t * dy);
    ((p.x - cx).powi(2) + (p.y - cy).powi(2)).sqrt()
}

fn route_with_planes(
    mut rp: RouteProblem,
    plane_names: &std::collections::BTreeSet<String>,
    rules: &DraftRules,
) -> pcb_engine::pipeline::RouteResult {
    rp.layer_count = 2;
    // Stitch points: one through-via per SMD plane pad, dropping it to its inner
    // plane. Collected BEFORE retagging layers so we can still tell a THROUGH-HOLE
    // plane pad — which already spans the inner planes and needs NO via (adding one
    // drills a hole co-located with the pad's own barrel: a KiCAD holes_co_located
    // defect) — from an SMD pad on one face, which does need the via. A pad belongs
    // to exactly one plane net.
    let stitches: Vec<(String, Point2, bool)> = rp
        .obstacles
        .iter()
        .filter_map(|ob| {
            ob.connected_to
                .iter()
                .find(|n| plane_names.contains(*n))
                .map(|n| {
                    let thru = ob.layers.contains(&LayerRef::top())
                        && ob.layers.contains(&LayerRef::bottom());
                    (n.clone(), ob.center.clone(), thru)
                })
        })
        .collect();
    // Retag plane-net pads onto both signal faces so signals route around them.
    for ob in &mut rp.obstacles {
        if ob.connected_to.iter().any(|n| plane_names.contains(n)) {
            ob.layers = vec![LayerRef::top(), LayerRef::bottom()];
        }
    }
    rp.connections.retain(|c| !plane_names.contains(&c.name));

    let mut result = route_auto(&rp);
    // Add a through-via per plane pad, but ONLY where it clears its foreign
    // neighbours — a fine-pitch part (0.5mm BGA) has no room for a standard
    // through-via between balls, and an overhanging via would short adjacent nets.
    // A via that doesn't fit is SKIPPED (that pad's power stays honestly unrouted,
    // surfaced as a KiCAD unconnected), never shipped as a DRC fault: the plane
    // path must be as connectivity-honest as the router. (Fine-pitch parts need
    // via-in-pad / microvias, a documented v1 limitation.)
    let via_r = rules.via_diameter / 2.0;
    let min_via2 = (2.0 * via_r + rules.clearance).powi(2);
    let dist2 = |a: &Point2, b: &Point2| (a.x - b.x).powi(2) + (a.y - b.y).powi(2);
    // Accurate point-to-RECT clearance: a long pad (a QFP lead) reaches far on its
    // long axis but is narrow across — measuring to the rect, not a max-dimension
    // circle, avoids over-skipping vias that actually clear a neighbour's lead.
    let clears_rect = |at: &Point2, ob: &Obstacle| -> bool {
        let dx = (at.x - ob.center.x).abs() - ob.width / 2.0;
        let dy = (at.y - ob.center.y).abs() - ob.height / 2.0;
        let d2 = dx.max(0.0).powi(2) + dy.max(0.0).powi(2);
        d2 >= (via_r + rules.clearance).powi(2)
    };
    let mut skipped = 0usize;
    for (net, at, thru) in stitches {
        // A through-hole plane pad already connects to its inner plane (its barrel
        // spans every copper layer); a stitching via here would only co-locate a
        // second drill with the pad's own hole.
        if thru {
            continue;
        }
        let clears_pads = rp
            .obstacles
            .iter()
            .all(|ob| ob.connected_to.contains(&net) || clears_rect(&at, ob));
        let clears_vias = result
            .solution
            .vias
            .iter()
            .all(|v| v.connection == net || dist2(&at, &v.at) >= min_via2);
        // The stitching via is added AFTER the signals route, so it must also clear
        // the routed TRACKS of other nets — otherwise on a congested board a power
        // via lands within clearance of a signal trace (a KiCAD clearance fault).
        // A via that can't clear is skipped (honest unrouted), never shipped.
        let clears_tracks = result.solution.traces.iter().all(|t| {
            t.connection == net || {
                let need = via_r + t.width / 2.0 + rules.clearance;
                !t.path
                    .windows(2)
                    .any(|w| seg_point_dist(&w[0], &w[1], &at) < need)
            }
        });
        if clears_pads && clears_vias && clears_tracks {
            result.solution.vias.push(Via {
                connection: net,
                at,
                diameter: rules.via_diameter,
                drill: rules.via_drill,
            });
        } else {
            skipped += 1;
        }
    }
    if skipped > 0 {
        result.failed.push(FailedNet {
            connection: format!("<{skipped} plane stitching vias>"),
            reason: "no room for a through-via between fine-pitch pads — that power \
                     pin stays on the plane unrouted (needs via-in-pad / microvias)"
                .to_owned(),
        });
    }
    result
}

/// Build the copper-plane zones for export: one pour per plane net, filling the
/// board minus an anti-pad keep-out around every FOREIGN copper item that reaches
/// that inner layer — every via on another net (which passes through the plane)
/// and every foreign through-hole pad (whose barrel sits on the inner layer).
/// Same-net vias/pads are NOT carved, so the plane's own stitching vias connect.
fn plane_zones(
    planes: &[(String, u32)],
    board: &RouteProblem,
    solution: &RouteSolution,
    bounds: &Bounds,
    rules: &DraftRules,
    user_keepouts: &[Keepout],
) -> Vec<ZoneSpec> {
    let layer_count = rules.layer_count;
    let via_half = rules.via_diameter / 2.0 + rules.clearance + PLANE_ANTIPAD_MARGIN_MM;
    planes
        .iter()
        .map(|(net, layer_idx)| {
            let mut keepouts: Vec<(Point2, f64, f64)> = Vec::new();
            for v in &solution.vias {
                if &v.connection != net {
                    keepouts.push((v.at.clone(), via_half, via_half));
                }
            }
            for ob in &board.obstacles {
                let on_layer = ob.layers.iter().any(|lr| lr.index(layer_count) == Some(*layer_idx));
                let foreign = !ob.connected_to.iter().any(|n| n == net);
                if on_layer && foreign {
                    let half =
                        ob.width.max(ob.height) / 2.0 + rules.clearance + PLANE_ANTIPAD_MARGIN_MM;
                    keepouts.push((ob.center.clone(), half, half));
                }
            }
            // A user keepout on this plane's layer is a NO-COPPER region — the
            // plane must carve it out (it was previously filled over: planes don't
            // route, so the routing-only keepout never reached the pour).
            for ko in user_keepouts {
                let on_layer = ko.layers.iter().any(|lr| lr.index(layer_count) == Some(*layer_idx));
                if on_layer {
                    let cx = (ko.rect.min_x + ko.rect.max_x) / 2.0;
                    let cy = (ko.rect.min_y + ko.rect.max_y) / 2.0;
                    let hx = (ko.rect.max_x - ko.rect.min_x) / 2.0 + rules.clearance;
                    let hy = (ko.rect.max_y - ko.rect.min_y) / 2.0 + rules.clearance;
                    keepouts.push((Point2 { x: cx, y: cy }, hx, hy));
                }
            }
            let fill = plane_fill_rects(bounds, BOARD_EDGE_MARGIN_MM, &keepouts);
            ZoneSpec {
                net_name: net.clone(),
                layer_name: format!("In{layer_idx}.Cu"),
                fill_rects: fill,
            }
        })
        .collect()
}

/// Safety margin added to a plane anti-pad beyond bare (radius + clearance) — a
/// bare keep-out lands exactly on the clearance limit and KiCAD flags it.
const PLANE_ANTIPAD_MARGIN_MM: f64 = 0.15;

/// Render the board to a PNG using the placement or routed SVG, save under
/// `.autopcb/renders/`, and attach via `IMAGE_PATH_KEY`.
///
/// `view` may be `"placed"` or `"routed"`. When omitted the default is
/// `"routed"` when `route.json` exists, `"placed"` otherwise.
pub fn render_board(input: Value, ctx: &ToolCtx) -> Result<Value> {
    // ── load draft ───────────────────────────────────────────────────────────
    let Some(draft) = crate::tools_pcb::BoardDraft::load(ctx) else {
        return Ok(json!({
            "error": "no board draft yet — call create_board first",
        }));
    };

    // ── resolve view ─────────────────────────────────────────────────────────
    let has_route = ctx.workspace().read_route().is_some();
    let view_str = input.get("view").and_then(Value::as_str);
    let view = match view_str {
        Some("placed") => "placed",
        Some("routed") => "routed",
        None => {
            if has_route { "routed" } else { "placed" }
        }
        Some(other) => {
            return Ok(json!({
                "error": format!(
                    "unknown view `{other}` — pass \"placed\" or \"routed\", or omit for auto"
                ),
            }));
        }
    };

    // ── generate SVG ─────────────────────────────────────────────────────────
    let svg = match view {
        "placed" => {
            // Need a last_placement in the draft.
            let Some(placements) = draft.last_placement.clone() else {
                return Ok(json!({
                    "error": "board has not been placed yet — run place_board first, \
                              then render_board",
                }));
            };
            let problem = match place_problem_from_draft(&draft, ctx) {
                Ok(p) => p,
                Err(msg) => return Ok(json!({ "error": msg })),
            };
            // Reconstruct a minimal PlaceResult from the stored placements.
            // render_placement uses .placements to look up part positions, and
            // problem.parts for courtyard/pad geometry. legal/report are not
            // used by the renderer — any zero-default values are fine.
            let result = PlaceResult {
                placements,
                legal: true,
                report: PlaceReport {
                    overlaps_resolved: 0,
                    out_of_bounds_clamps: 0,
                    hpwl: 0.0,
                    layout_cost: 0.0,
                },
            };
            pcb_engine::svg::render_placement(&problem, &draft.hints, &result)
        }
        "routed" => {
            // Need route.json.
            let Some(raw) = ctx.workspace().read_route() else {
                return Ok(json!({
                    "error": "board has not been routed yet — run route_board first, \
                              then render_board",
                }));
            };
            // Need placements too (to rebuild the RouteProblem).
            let Some(placements) = draft.last_placement.clone() else {
                return Ok(json!({
                    "error": "board has no placement in the draft — run place_board \
                              then route_board before rendering the routed view",
                }));
            };
            let stored: StoredRoute = match serde_json::from_str(&raw) {
                Ok(s) => s,
                Err(e) => {
                    return Ok(json!({
                        "error": format!("route.json is corrupt or schema-mismatch: {e}"),
                    }));
                }
            };
            let problem = match place_problem_from_draft(&draft, ctx) {
                Ok(p) => p,
                Err(msg) => return Ok(json!({ "error": msg })),
            };
            let mut rp = to_route_problem(&problem, &placements);
            inject_keepouts(&mut rp, &draft.keepouts);
            pcb_engine::svg::render_svg(&rp, &stored.solution, &stored.failed)
        }
        // The match above is exhaustive over {"placed","routed"}; the `other`
        // arm returned early, so this branch is unreachable.
        _ => unreachable!(),
    };

    // ── rasterize + persist ──────────────────────────────────────────────────
    let png = crate::render::svg_to_png(&svg, crate::tools::RENDER_MAX_PX)?;
    let path = ctx.workspace().next_render_path()?;
    std::fs::write(&path, &png)
        .with_context(|| format!("writing render to {}", path.display()))?;

    let mut obj = json!({
        "ok": true,
        "view": view,
        "png_path": path.display().to_string(),
        "note": format!(
            "Board ({view} view) rendered and attached. \
             Top-layer copper = red, bottom = blue, failed nets = orange crosses. \
             In the placed view, keepout rectangles appear as dark-grey unowned \
             obstacles and part courtyards as grey outlines. \
             PNG also saved to png_path for the user to open."
        ),
    });
    obj[crate::tools::IMAGE_PATH_KEY] = json!(path.display().to_string());
    Ok(obj)
}

// ── export_board ─────────────────────────────────────────────────────────────

/// DRC findings KiCAD raises that are independent of the routed copper: the
/// inline footprint bodies do not byte-match the installed library copies. This
/// is the same inert library-bookkeeping warning the e2e tests carve out (see
/// `placed_board_e2e.rs`); it never reflects a copper or connectivity fault.
const NON_COPPER_WARNINGS: &[&str] = &[
    // Inert library-bookkeeping diff between the board footprint and its library.
    "lib_footprint_mismatch",
    "lib_footprint_issues",
    // Silkscreen warnings — these are silk-layer aesthetics, never a copper or
    // connectivity fault. Keeping silkscreen on a dense board (so it reads like a
    // real PCB) inevitably produces some silk-over-copper / silk overlap; that is
    // a fab note, not a copper violation, so it must not gate the copper count.
    "silk_over_copper",
    "silk_overlap",
    "silk_edge_clearance",
    "silk_over_silk",
];

fn is_non_copper(v: &Violation) -> bool {
    v.severity == "warning" && NON_COPPER_WARNINGS.contains(&v.kind.as_str())
}

/// The KiCAD major version, or 0 if unparseable / no real install.
fn kicad_major(ctx: &ToolCtx) -> u32 {
    ctx.env()
        .cli_version
        .split('.')
        .next()
        .and_then(|m| m.parse().ok())
        .unwrap_or(0)
}

/// Build the [`SynthPart`]s for the draft from the resolved footprint sources and
/// the stored placement. Returns a model-readable error string if a footprint's
/// source text can no longer be loaded, or a part has no placement.
fn synth_parts_from_draft(
    draft: &BoardDraft,
    placements: &[Placement],
    ctx: &ToolCtx,
) -> std::result::Result<Vec<SynthPart>, String> {
    let index = ctx
        .footprint_index()
        .map_err(|e| format!("footprint index unavailable: {e}"))?;
    let by_ref: BTreeMap<&str, &Placement> =
        placements.iter().map(|p| (p.reference.as_str(), p)).collect();

    let mut parts = Vec::with_capacity(draft.parts.len());
    for dp in &draft.parts {
        let source = index.footprint_source(&dp.footprint).ok_or_else(|| {
            format!(
                "part {}: footprint `{}` source is no longer readable — re-create the board",
                dp.reference, dp.footprint
            )
        })?;
        let placement = by_ref.get(dp.reference.as_str()).ok_or_else(|| {
            format!(
                "part {} has no placement — re-run place_board before export",
                dp.reference
            )
        })?;
        parts.push(SynthPart {
            reference: dp.reference.clone(),
            lib_id: dp.footprint.clone(),
            source,
            pad_nets: dp.pad_nets.clone(),
            placement: (*placement).clone(),
        });
    }
    Ok(parts)
}

/// Edge-of-board margin (mm) left around the copper when the export tightens the
/// board outline to the parts. Comfortably clears the default copper-to-edge
/// clearance and leaves a clean visual border.
const BOARD_EDGE_MARGIN_MM: f64 = 1.0;

/// A board outline that snugly fits all copper (pads, traces, vias) plus
/// `margin` of edge clearance on every side. Returns `budget` unchanged when
/// there is no copper to bound. The outline is `content ± margin` *without*
/// clamping to `budget`: every piece of copper is inside by exactly `margin`, so
/// the board can never clip copper or trip a copper-to-edge clearance rule. The
/// requested `budget` is only a routing area; the finished outline may extend up
/// to `margin` beyond it so a part routed to the budget edge still clears it.
fn content_bounds(
    problem: &RouteProblem,
    solution: &RouteSolution,
    budget: &Bounds,
    margin: f64,
) -> Bounds {
    let (mut min_x, mut max_x) = (f64::INFINITY, f64::NEG_INFINITY);
    let (mut min_y, mut max_y) = (f64::INFINITY, f64::NEG_INFINITY);
    let mut acc = |x0: f64, y0: f64, x1: f64, y1: f64| {
        min_x = min_x.min(x0);
        max_x = max_x.max(x1);
        min_y = min_y.min(y0);
        max_y = max_y.max(y1);
    };
    for o in &problem.obstacles {
        acc(
            o.center.x - o.width / 2.0,
            o.center.y - o.height / 2.0,
            o.center.x + o.width / 2.0,
            o.center.y + o.height / 2.0,
        );
    }
    for t in &solution.traces {
        let hw = t.width / 2.0;
        for p in &t.path {
            acc(p.x - hw, p.y - hw, p.x + hw, p.y + hw);
        }
    }
    for v in &solution.vias {
        let r = v.diameter / 2.0;
        acc(v.at.x - r, v.at.y - r, v.at.x + r, v.at.y + r);
    }
    if !min_x.is_finite() {
        return budget.clone();
    }
    Bounds {
        min_x: min_x - margin,
        max_x: max_x + margin,
        min_y: min_y - margin,
        max_y: max_y + margin,
    }
}

/// Synthesize the routed board into a `.kicad_pcb`: requires a placed + routed
/// draft, writes the board (footprints from the engine placement + the stored
/// copper) and, when a recent enough KiCAD is available, runs `kicad-cli pcb
/// drc` and reports the counts.
pub fn export_board(input: Value, ctx: &ToolCtx) -> Result<Value> {
    let Some(draft) = BoardDraft::load(ctx) else {
        return Ok(json!({
            "error": "no board draft yet — call create_board first",
        }));
    };
    let Some(placements) = draft.last_placement.clone() else {
        return Ok(json!({
            "error": "the board is not placed — run place_board (then route_board) before export",
        }));
    };
    // Never export an ILLEGAL placement — it would ship a board with overlapping
    // courtyards / out-of-bounds parts that fails DRC. The engine's contract is to
    // emit only DRC-clean copper or fail honestly: refuse with an actionable fix.
    if draft.last_place_illegal {
        return Ok(json!({
            "error": "the last placement is NOT legal (courtyard overlap / out of bounds) — \
                      the board is too tight for these parts. Enlarge `bounds` or remove parts, \
                      then re-run place_board and route_board before export.",
        }));
    }
    let Some(raw_route) = ctx.workspace().read_route() else {
        return Ok(json!({
            "error": "the board is not routed — run route_board before export \
                      (export writes the routed copper)",
        }));
    };
    let stored: StoredRoute = match serde_json::from_str(&raw_route) {
        Ok(s) => s,
        Err(e) => {
            return Ok(json!({
                "error": format!("route.json is corrupt or schema-mismatch: {e}"),
            }));
        }
    };

    // Build the synthesis inputs and assemble the board text.
    let parts = match synth_parts_from_draft(&draft, &placements, ctx) {
        Ok(p) => p,
        Err(msg) => return Ok(json!({ "error": msg })),
    };
    let board_text = match synthesize_board_layers(&parts, &draft.bounds, draft.rules.layer_count) {
        Ok(t) => t,
        Err(e) => {
            return Ok(json!({
                "error": format!("board synthesis failed: {e}"),
            }));
        }
    };

    // Resolve the output path (default = the project's <stem>.kicad_pcb).
    let path = match input.get("path").and_then(Value::as_str) {
        Some(p) => std::path::PathBuf::from(p),
        None => ctx.pcb_path(),
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating export dir {}", parent.display()))?;
    }
    std::fs::write(&path, board_text.as_bytes())
        .with_context(|| format!("writing synthesized board to {}", path.display()))?;

    // Read the just-written board back for its net-code / layer map, then splice
    // the routed copper onto it. (write_solution validates by re-parse before it
    // promotes the file, so a malformed splice never lands.)
    let board = read_problem(&path)
        .with_context(|| format!("re-reading synthesized board {}", path.display()))?;

    // Tighten the board outline to fit the actual copper. The requested bounds
    // are a routing *budget*; a finished board should be cropped to the parts +
    // copper (+ an edge margin) so it reads as professional instead of a small
    // cluster stranded on an oversized blank. This is a pure edge-cuts change
    // computed AFTER routing — the copper geometry is untouched, so it cannot
    // create a DRC regression (the outline only ever shrinks toward the copper,
    // never clips it). Re-synthesize the outline at the tight bounds.
    let tight = content_bounds(&board.problem, &stored.solution, &draft.bounds, BOARD_EDGE_MARGIN_MM);
    // Copper-plane zones (power pours) for a multilayer board, computed from the
    // foreign copper reaching each inner layer, at the final (tight) bounds. The
    // obstacle positions are absolute, so the first board's read is reusable here.
    let zones = plane_zones(
        &stored.planes,
        &board.problem,
        &stored.solution,
        &tight,
        &draft.rules,
        &draft.keepouts,
    );
    let board = if tight != draft.bounds || !zones.is_empty() {
        match synthesize_board_full(&parts, &tight, draft.rules.layer_count, &zones) {
            Ok(t) => {
                std::fs::write(&path, t.as_bytes())
                    .with_context(|| format!("writing tight-outline board to {}", path.display()))?;
                read_problem(&path)
                    .with_context(|| format!("re-reading tight-outline board {}", path.display()))?
            }
            Err(_) => board,
        }
    } else {
        board
    };
    write_solution(&path, &stored.solution, &board)
        .with_context(|| format!("writing routed copper onto {}", path.display()))?;

    let mut out = json!({
        "ok": true,
        "path": path.display().to_string(),
        "part_count": draft.parts.len(),
        "failed_nets": stored.failed.len(),
        "traces": stored.solution.traces.len(),
        "vias": stored.solution.vias.len(),
    });

    // DRC, when a real KiCAD ≥ 8 is on PATH (kicad-cli pcb drc lands in 8).
    if kicad_major(ctx) >= 8 {
        match KicadCli::new(ctx.env()).drc(&path) {
            Ok(report) => {
                let copper: Vec<&Violation> = report
                    .violations
                    .iter()
                    .filter(|v| !is_non_copper(v))
                    .collect();
                let tolerated = report.violations.len() - copper.len();
                out["drc"] = json!({
                    "ran": true,
                    "kicad_version": ctx.env().cli_version,
                    "copper_violations": copper.len(),
                    // Error-severity copper faults (shorts/clearance/width) only —
                    // the hard "never ship a DRC fault" metric; must be 0.
                    "copper_errors": report.copper_error_count(),
                    "unconnected_items": report.unconnected_items.len(),
                    "tolerated_footprint_warnings": tolerated,
                    "errors": report.error_count(),
                    "carve_out": "lib_footprint_mismatch warnings are tolerated — they \
                                  are an inert library-bookkeeping diff, not a copper fault",
                });
                out["note"] = json!(if copper.is_empty()
                    && report.unconnected_items.is_empty()
                    && report.error_count() == 0
                {
                    "board exported and DRC-clean (0 copper violations, 0 unconnected)."
                } else {
                    "board exported, but KiCAD DRC found copper/connectivity issues — \
                     inspect drc, re-route or triage, then export again."
                });
            }
            Err(e) => {
                out["drc"] = json!({ "ran": false, "error": e.to_string() });
                out["note"] = json!("board exported; DRC could not run (see drc.error).");
            }
        }
    } else {
        out["drc"] = json!({
            "ran": false,
            "skipped": "no KiCAD >= 8 on PATH (kicad-cli pcb drc needs KiCAD 8+)",
        });
        out["note"] = json!("board exported; DRC skipped (no KiCAD CLI available).");
    }

    Ok(out)
}
