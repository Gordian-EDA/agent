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
//! only via a part `lock` in the Board-DSL, snapped/legalized by the engine on place.
//!
//! The serde shape reuses pcb-engine types directly ([`PlacementHints`],
//! [`Placement`], [`LockedAt`], [`Rect`], [`LayerRef`]) so a draft round-trips
//! straight into a `PlaceProblem` in Task 2 without a translation layer.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use circuit_lang::model::PinTarget;
use sch_layout::lift::lift;

use kicad_cli_rs::cli::{KicadCli, Violation};
use kicad_sexpr::pcb::{read_problem, write_solution};
use pcb_synth::placefp::{part_from_footprint, part_from_footprint_layers};
use pcb_synth::synth::{
    plane_fill_rects, synthesize_board_full, synthesize_board_layers, SynthPart, ZoneSpec,
};
use drc_lint::connectivity::Violation as ConnViolation;
use drc_lint::lint::{DrcViolation, lint};
use negotiated_mesh::pathing::global_route;
use negotiated_mesh::pipeline::{RouterKind, metrics, route_auto};
use pcb_place::placement::{
    GroupHint, LockedAt, Part, PlaceProblem, PlaceReport, PlaceResult, Placement, PlacementHints,
    Rect, to_route_problem,
};
use pcb_model::{
    Bounds, FailedNet, LayerRef, Obstacle, Point2, RouteProblem, RouteSolution, Via, ViaSpan,
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
    /// Optional custom board OUTLINE (closed polygon, mm) — circle, square, star, any
    /// shape. When set it becomes the Edge.Cuts at export (so the render shows the real
    /// shape and the agent can iterate); `bounds` stays the polygon's bounding box for
    /// placement/routing. None = the default rectangular outline from `bounds`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outline: Option<Vec<Point2>>,
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
    /// Per-net trace-width overrides (net name → mm) — fat copper for power/high-current
    /// nets, thin for signals. A net not listed uses `min_trace_width`.
    #[serde(default)]
    pub net_widths: std::collections::BTreeMap<String, f64>,
    /// Copper POURS on signal layers: a flood of a net (usually GND) on "top"/"bottom",
    /// carved around foreign copper. The HF return-path / shielding case (distinct from
    /// the inner 4-layer power planes). Empty = no signal-layer pours.
    #[serde(default)]
    pub pours: Vec<PourSpec>,
}

/// A copper pour request: flood `net` on signal layer `layer` ("top" or "bottom").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PourSpec {
    pub net: String,
    pub layer: String,
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
            net_widths: std::collections::BTreeMap::new(),
            pours: Vec::new(),
        }
    }
}

/// One part on the board: a reference, the footprint `Lib:Name` lib_id, the
/// per-pad net assignment (pad number → net name), and an optional locked
/// position (set via a part `lock` in the Board-DSL).
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
    pub rect: pcb_place::placement::Rect,
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
            // The model binds nets by pad NUMBER and never needs each pad's coordinates (it
            // doesn't place pads), so return the number list + a compact shape SUMMARY rather
            // than the full per-pad table — for a 256-ball BGA the old table was ~20k chars
            // re-sent every turn. min_pitch + pad_size let the model judge fine-pitch (pick a
            // clearance/via); technologies/layers tell it SMD vs thru-hole.
            let pad_numbers: Vec<&str> =
                fp.pads.iter().map(|p| p.number.as_str()).filter(|n| !n.is_empty()).collect();
            let mut min_pitch = f64::INFINITY;
            for (i, a) in fp.pads.iter().enumerate() {
                for b in &fp.pads[i + 1..] {
                    let d = ((a.at[0] - b.at[0]).powi(2) + (a.at[1] - b.at[1]).powi(2)).sqrt();
                    if d > 1e-6 && d < min_pitch {
                        min_pitch = d;
                    }
                }
            }
            let (mut wmin, mut wmax) = (f64::INFINITY, 0.0_f64);
            for p in &fp.pads {
                let s = p.size[0].min(p.size[1]);
                wmin = wmin.min(s);
                wmax = wmax.max(p.size[0].max(p.size[1]));
            }
            let techs: std::collections::BTreeSet<&str> =
                fp.pads.iter().map(|p| technology_str(p.technology)).collect();
            Ok(json!({
                "lib_id": lib_id,
                "name": fp.name,
                "descr": fp.descr,
                "pad_count": fp.pads.len(),
                "pad_numbers": pad_numbers,
                "min_pitch_mm": if min_pitch.is_finite() { (min_pitch * 1000.0).round() / 1000.0 } else { 0.0 },
                "pad_min_dim_mm": if wmin.is_finite() { (wmin * 1000.0).round() / 1000.0 } else { 0.0 },
                "pad_max_dim_mm": (wmax * 1000.0).round() / 1000.0,
                "technologies": techs,
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

/// Render a [`kicad_sexpr::footlib::PadTechnology`] as a stable lowercase
/// string for the LLM.
fn technology_str(t: kicad_sexpr::footlib::PadTechnology) -> &'static str {
    use kicad_sexpr::footlib::PadTechnology::*;
    match t {
        Smd => "smd",
        ThruHole => "thru_hole",
        NpThruHole => "np_thru_hole",
        Other => "other",
    }
}

fn courtyard_source_str(s: kicad_sexpr::footlib::CourtyardSource) -> &'static str {
    use kicad_sexpr::footlib::CourtyardSource::*;
    match s {
        Crtyd => "crtyd",
        PadSilkFallback => "pad_silk_fallback",
    }
}

fn bbox_json(b: &kicad_sexpr::footlib::BBox) -> Value {
    json!({
        "min_x": b.min_x,
        "min_y": b.min_y,
        "max_x": b.max_x,
        "max_y": b.max_y,
        "width": b.width(),
        "height": b.height(),
    })
}


// ── derive_board ──────────────────────────────────────────────────────────────


/// `derive_board` — build the board draft from the committed schematic + the
/// footprint map, instead of re-typing parts by hand. Connectivity comes from the
/// schematic's netlist (via `lift`, which keys pins by pad number — KiCAD does the
/// pin→pad resolution for us), footprints from `.autopcb/footprints.json`. The
/// caller supplies only `bounds` and `rules`; parts and nets come from the
/// schematic. Delegates to `create_board` for footprint resolution, validation,
/// and the draft build. See `docs/specs/schematic-driven-pcb.md`.
pub fn derive_board(input: Value, ctx: &ToolCtx) -> Result<Value> {
    if !ctx.sch_path().exists() {
        return Ok(json!({
            "error": "no .kicad_sch yet — commit the schematic with apply_design first, \
                      then derive_board"
        }));
    }
    let yaml = match lift(ctx.env(), ctx.sch_path()) {
        Ok(y) => y,
        Err(e) => {
            return Ok(json!({ "error": format!("could not read the schematic netlist: {e}") }));
        }
    };
    let Some(design) = circuit_lang::compile(&yaml, ctx.provider()).design else {
        return Ok(json!({ "error": "the schematic netlist did not compile back to a design" }));
    };
    let overwrite = input.get("overwrite").and_then(Value::as_bool).unwrap_or(false);
    if BoardDraft::load(ctx).is_some() && !overwrite {
        return Ok(json!({ "error": "a board draft already exists — pass overwrite=true to replace it" }));
    }

    // Seed a BoardDraft directly from the schematic: one part per component, pads
    // from the (pad-number-keyed) pins flattened across units, footprint taken
    // from the symbol's footprint field. Parts whose symbol carries no footprint
    // are flagged in `missing_footprints` for `assign_footprint`.
    let mut parts = Vec::new();
    let mut missing_footprints = Vec::new();
    for (refdes, c) in design.blocks.values().flat_map(|b| b.components.iter()) {
        let footprint = c.footprint.clone().unwrap_or_default();
        if footprint.is_empty() {
            missing_footprints.push(refdes.clone());
        }
        let mut pad_nets = BTreeMap::new();
        for pins in std::iter::once(&c.pins).chain(c.units.values()) {
            for (pad, target) in pins {
                if let PinTarget::Net(net) = target {
                    pad_nets.insert(pad.clone(), net.clone());
                }
            }
        }
        parts.push(DraftPart {
            reference: refdes.clone(),
            footprint,
            pad_nets,
            locked: None,
        });
    }

    let bounds = if input.get("bounds").is_some() {
        match parse_bounds(input.get("bounds")) {
            Ok(b) => b,
            Err(e) => return Ok(json!({ "error": e })),
        }
    } else {
        Bounds { min_x: 0.0, min_y: 0.0, max_x: 50.0, max_y: 40.0 }
    };
    let layer_count = input
        .get("rules")
        .and_then(|r| r.get("layers"))
        .and_then(Value::as_u64)
        .unwrap_or(2) as u32;

    let part_count = parts.len();
    let draft = BoardDraft {
        bounds,
        rules: DraftRules { layer_count, ..Default::default() },
        parts,
        keepouts: Vec::new(),
        hints: PlacementHints::default(),
        last_placement: None,
        last_place_illegal: false,
        outline: None,
    };
    draft.save(ctx)?;

    let note = if missing_footprints.is_empty() {
        "board seeded from the schematic — run place_board, then route_board, then open_board to refine it interactively"
    } else {
        "board seeded; some parts have no footprint — set each with assign_footprint (use search_footprints for the lib_id), then place_board"
    };
    Ok(json!({
        "ok": true,
        "part_count": part_count,
        "missing_footprints": missing_footprints,
        "note": note,
    }))
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
    if !matches!(layer_count, 2 | 4 | 6 | 8) {
        return Err(format!("rules.layers must be 2, 4, 6, or 8, got {layer_count}"));
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
    // Per-net trace widths: {"VCC": 0.8, "GND": 0.8} — fat power, thin signals.
    let mut net_widths = std::collections::BTreeMap::new();
    if let Some(nw) = obj.get("net_widths") {
        let map = nw
            .as_object()
            .ok_or_else(|| "rules.net_widths must be an object {net: width_mm}".to_string())?;
        for (net, w) in map {
            let w = w
                .as_f64()
                .ok_or_else(|| format!("rules.net_widths[{net}] must be a number (mm)"))?;
            if w <= 0.0 {
                return Err(format!("rules.net_widths[{net}] must be > 0, got {w}"));
            }
            net_widths.insert(net.clone(), w);
        }
    }
    // Copper pours: [{"net":"GND","layer":"bottom"}] — flood a net on a signal layer.
    let mut pours = Vec::new();
    if let Some(pv) = obj.get("pours") {
        let arr = pv
            .as_array()
            .ok_or_else(|| "rules.pours must be an array of {net, layer}".to_string())?;
        for p in arr {
            let net = p.get("net").and_then(Value::as_str)
                .ok_or_else(|| "rules.pours[].net must be a string".to_string())?;
            let layer = p.get("layer").and_then(Value::as_str).unwrap_or("bottom");
            // A pour floods a SIGNAL layer (top/bottom, or an inner signal layer on a
            // 6-layer board) — never a GND/VCC PLANE (already a full copper layer) or a
            // non-existent layer. Resolve + reject up front rather than silently drop it.
            match resolve_pour_layer(layer, layer_count) {
                None => {
                    return Err(format!(
                        "rules.pours[].layer '{layer}' is not a valid copper layer on a \
                         {layer_count}-layer board — use top/bottom, or innerN on a 6-layer board"
                    ));
                }
                Some((idx, _))
                    if grid_astar::router::plane_layers(layer_count as usize).contains(&idx) =>
                {
                    return Err(format!(
                        "rules.pours[].layer '{layer}' is a GND/VCC PLANE on a {layer_count}-layer \
                         board — a plane is already full copper; pour on a signal layer instead"
                    ));
                }
                Some(_) => {}
            }
            pours.push(PourSpec { net: net.to_string(), layer: layer.to_string() });
        }
    }
    Ok(DraftRules {
        clearance: num("clearance", d.clearance),
        min_trace_width: num("min_trace_width", d.min_trace_width),
        via_diameter,
        via_drill,
        layer_count,
        net_widths,
        pours,
    })
}

/// KiCAD 9 built-in (standard-fab) minimums, verified against `kicad-cli pcb drc`:
/// a via below these trips `via_diameter` / `drill_out_of_range` / `annular_width`.
const KICAD_MIN_VIA_DIAMETER: f64 = 0.5;
const KICAD_MIN_VIA_DRILL: f64 = 0.3;
const KICAD_MIN_ANNULAR: f64 = 0.1;
/// KiCAD's hole-to-hole (drill edge to drill edge) minimum. Applies between ANY two drilled
/// holes regardless of net — two barrels cannot overlap mechanically — so via placement must
/// honour it even for SAME-NET vias (which may share copper but never a hole).
const KICAD_HOLE_CLEAR_MM: f64 = 0.25;
/// HDI blind/micro via-in-pad size for the fine-pitch plane-stitch fallback (increment 3). Smaller
/// than a through via so it fits IN the pad where a 0.5–0.6 through-via has no room between
/// fine-pitch balls, while keeping KiCAD's standard 0.1 mm annular so `kicad-cli pcb drc` accepts
/// it (proven in the feasibility spike). See docs/specs/hdi-microvia-feasibility.md.
const HDI_VIA_DIAMETER: f64 = 0.4;
const HDI_VIA_DRILL: f64 = 0.2;

/// Parse one part JSON into a validated [`DraftPart`] for [`build_board_draft`]: footprint
/// resolution, intrinsic pad-clearance rejection, and lock parsing. Returns the error-shaped
/// `Value` on any problem so the caller can return it directly (the model then fixes just
/// that part).
fn parse_draft_part(
    pj: &Value,
    index: &kicad_sexpr::footlib::FootprintIndex,
    clearance: f64,
) -> std::result::Result<DraftPart, Value> {
    let reference = pj
        .get("reference")
        .and_then(Value::as_str)
        .ok_or_else(|| json!({ "error": "a part is missing its string `reference`" }))?
        .to_string();
    let footprint = pj
        .get("footprint")
        .and_then(Value::as_str)
        .ok_or_else(|| json!({ "error": format!("part {reference}: missing string `footprint` lib_id") }))?
        .to_string();
    let Some(resolved_fp) = index.footprint(&footprint) else {
        return Err(json!({
            "error": format!(
                "part {reference}: unknown footprint `{footprint}` — \
                 search_footprints for the real lib_id, never guess it"
            ),
            "suggestions": index.suggest(&footprint),
        }));
    };
    let pad_nets: BTreeMap<String, String> = match pj.get("pad_nets") {
        None | Some(Value::Null) => BTreeMap::new(),
        Some(v) => serde_json::from_value(v.clone()).map_err(|e| {
            json!({ "error": format!("part {reference}: pad_nets must map pad number → net name: {e}") })
        })?,
    };
    // Reject a footprint whose own pads (different-net OR un-netted/NC) sit closer than the
    // board clearance — an inherent clearance DRC fault no routing can fix.
    if let Some((a, b, gap)) =
        pcb_synth::placefp::pad_clearance_violations(&resolved_fp, &pad_nets, clearance).first()
    {
        return Err(json!({
            "error": format!(
                "part {reference}: footprint `{footprint}` pads {a} and {b} are only {gap:.3}mm \
                 apart (< the {clearance:.3}mm rules.clearance) — they are on different nets, so \
                 this is a built-in clearance violation. Lower rules.clearance (e.g. to {:.2}) or \
                 use a coarser-pitch footprint.",
                (gap - 0.01_f64).max(0.05),
            ),
        }));
    }
    // Optional lock: pin a part at a position/rotation. Accepts {x,y,rotation?} or {at:{x,y},…}.
    let locked = match pj.get("locked") {
        None | Some(Value::Null) => None,
        Some(l) => {
            let at = l.get("at").unwrap_or(l);
            match (at.get("x").and_then(Value::as_f64), at.get("y").and_then(Value::as_f64)) {
                (Some(x), Some(y)) => {
                    let raw = l.get("rotation").and_then(Value::as_i64).unwrap_or(0) as i32;
                    let rotation = axis_aligned_rotation(raw)
                        .map_err(|e| json!({ "error": format!("part {reference}: {e}") }))?;
                    Some(LockedAt { at: Point2 { x, y }, rotation })
                }
                _ => {
                    return Err(json!({ "error": format!("part {reference}: `locked` needs numeric x and y") }));
                }
            }
        }
    };
    Ok(DraftPart { reference, footprint, pad_nets, locked })
}

/// Build (and persist) the working [`BoardDraft`] from a `{bounds, parts, rules?, outline?,
/// overwrite?}` spec. **Internal builder — NOT an agent tool.** The agent reaches the board
/// only through [`derive_board`], which assembles this spec from the committed schematic + the
/// footprint map. Also called directly by the deterministic test harnesses (board_harness /
/// pcb_gate / board_artifact) that build boards from standalone JSON, no schematic.
pub fn build_board_draft(input: Value, ctx: &ToolCtx) -> Result<Value> {
    let overwrite = input.get("overwrite").and_then(Value::as_bool).unwrap_or(false);
    if BoardDraft::load(ctx).is_some() && !overwrite {
        return Ok(json!({
            "error": "a board draft already exists — pass overwrite=true to replace it",
        }));
    }

    // Optional custom OUTLINE (polygon points, mm) — circle/square/star/any shape. When
    // given, `bounds` is its bounding box (placement/routing extent) and the polygon
    // becomes the Edge.Cuts at export (the render then shows the true shape).
    let outline: Option<Vec<Point2>> = match input.get("outline") {
        None | Some(Value::Null) => None,
        Some(o) => {
            let arr = match o.as_array() {
                Some(a) if a.len() >= 3 => a,
                _ => return Ok(json!({ "error": "outline must be an array of >= 3 [x,y] points" })),
            };
            let mut pts = Vec::with_capacity(arr.len());
            for p in arr {
                match p.as_array().map(|xy| (xy.len(), xy)) {
                    Some((2, xy)) => match (xy[0].as_f64(), xy[1].as_f64()) {
                        (Some(x), Some(y)) => pts.push(Point2 { x, y }),
                        _ => return Ok(json!({ "error": "outline point must be [x, y] numbers" })),
                    },
                    _ => return Ok(json!({ "error": "outline point must be a [x, y] pair" })),
                }
            }
            Some(pts)
        }
    };
    let bounds = match &outline {
        Some(o) => Bounds {
            min_x: o.iter().map(|p| p.x).fold(f64::INFINITY, f64::min),
            max_x: o.iter().map(|p| p.x).fold(f64::NEG_INFINITY, f64::max),
            min_y: o.iter().map(|p| p.y).fold(f64::INFINITY, f64::min),
            max_y: o.iter().map(|p| p.y).fold(f64::NEG_INFINITY, f64::max),
        },
        None => match parse_bounds(input.get("bounds")) {
            Ok(b) => b,
            Err(msg) => return Ok(json!({ "error": msg })),
        },
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
    for pj in parts_json {
        match parse_draft_part(pj, index, rules.clearance) {
            Ok(p) => parts.push(p),
            Err(e) => return Ok(e),
        }
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
        outline,
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

/// TEST-HARNESS support (NOT an agent tool): set a draft's keepouts + placement-hint
/// groups from a circuit-spec JSON (`{keepouts: [{rect, layers}], hints: {groups: […]}}`),
/// reusing the same parsers the engine uses. The agent authors keepouts/groups in the
/// Board-DSL; this lets the deterministic harnesses seed them on a built draft.
pub fn apply_spec_extras(draft: &mut BoardDraft, spec: &Value) {
    if let Some(kos) = spec.get("keepouts").and_then(Value::as_array) {
        let (bounds, layers) = (draft.bounds.clone(), draft.rules.layer_count);
        draft.keepouts = kos
            .iter()
            .enumerate()
            .filter_map(|(i, k)| parse_keepout(k, &bounds, layers, i).ok())
            .collect();
    }
    if let Some(groups) = spec
        .get("hints")
        .and_then(|h| h.get("groups"))
        .and_then(Value::as_array)
    {
        let known: Vec<&str> = draft.parts.iter().map(|p| p.reference.as_str()).collect();
        draft.hints.groups = groups
            .iter()
            .filter_map(|g| parse_group_hint(g, &known).ok())
            .collect();
    }
}

// ── get_board ────────────────────────────────────────────────────────────────

pub fn get_board(ctx: &ToolCtx) -> Result<Value> {
    let Some(draft) = BoardDraft::load(ctx) else {
        return Ok(json!({
            "error": "no board draft yet — run derive_board first",
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

    // Lean draft view: each part's pad→net map is exactly what the model itself passed to
    // create_board, so echoing it back on every get_board call only re-bloats the context
    // (43–64% of a dense BGA board's draft, re-sent each turn). Replace it with a pad_count;
    // the per-net pin SUMMARY below carries the connectivity view the model actually inspects.
    let mut draft_json = serde_json::to_value(&draft)?;
    if let Some(parts) = draft_json.get_mut("parts").and_then(Value::as_array_mut) {
        for part in parts {
            if let Some(obj) = part.as_object_mut() {
                let n = obj
                    .get("pad_nets")
                    .and_then(Value::as_object)
                    .map_or(0, serde_json::Map::len);
                obj.remove("pad_nets");
                obj.insert("pad_count".into(), json!(n));
            }
        }
    }

    Ok(json!({
        "draft": draft_json,
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
        // A DSL part `lock` pins the part for the placer (carried through here).
        part.locked = dp.locked.clone();
        parts.push(part);
    }
    // Keep-outs that block a SIGNAL layer (top/bottom) are placement obstacles too:
    // a part dropped inside one has its pads trapped (no track can leave). Inner-only
    // (plane) keep-outs don't constrain placement, so they're excluded here.
    let keepouts: Vec<pcb_place::placement::Rect> = draft
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
        bounds: routing_bounds(draft),
        clearance: draft.rules.clearance,
        layer_count: draft.rules.layer_count,
        min_trace_width: draft.rules.min_trace_width,
        parts,
        keepouts,
        outline: draft.outline.clone(),
    })
}

/// KiCAD's copper-to-board-edge clearance (its default). Copper closer than this to the
/// Edge.Cuts is a `copper_edge_clearance` fault.
const EDGE_CLEAR_MM: f64 = 0.5;

/// The bounds the placer + router actually work inside. For a board with NO custom outline the
/// export auto-tightens the edge to copper + 1 mm, so any copper is already ≥ 1 mm from the
/// finished edge — no inset needed. But a CUSTOM outline is exported verbatim, so copper routed
/// to the raw `bounds` lands right on that edge and trips KiCAD's 0.5 mm copper-to-edge rule
/// (a dense custom-outline board shipped 42 such faults). Inset the working bounds by the edge
/// clearance so place + route keep copper off the edge; the exported Edge.Cuts stays the user's
/// real outline. (Inset the bbox; the lint also checks distance to the outline POLYGON edges,
/// catching the non-bbox edges of a non-rectangular outline.)
fn routing_bounds(draft: &BoardDraft) -> Bounds {
    if draft.outline.is_none() {
        return draft.bounds.clone();
    }
    let b = &draft.bounds;
    // Never invert a small board: clamp the inset so min stays < max.
    let inset = EDGE_CLEAR_MM.min((b.max_x - b.min_x) / 2.0 - 0.1).min((b.max_y - b.min_y) / 2.0 - 0.1);
    Bounds {
        min_x: b.min_x + inset,
        max_x: b.max_x - inset,
        min_y: b.min_y + inset,
        max_y: b.max_y - inset,
    }
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
            "error": "no board draft yet — run derive_board first",
        }));
    };

    let problem = match place_problem_from_draft(&draft, ctx) {
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

    // UNIFIED FAN-OUT placement (the routing-neatness + compactness lever): for a
    // board with a dominant fine-pitch IC, lay the WHOLE board as a radial fan-out —
    // IC centred, caps then series resistors (in IC-pad order) then passives on
    // density-aware concentric rings, connectors on the outer frame — overlap-free
    // by construction, with bounds sized to fit. Escapes route radially (short,
    // parallel) and the board is compact. Falls back to cap-ring auto-surround when
    // there's no clear dominant IC.
    // Run the placement PIPELINE — structured fan-out fast-path → optimize fallback.
    // The stages, the $NO_UNIFIED override, and the legality fallback all live in (and
    // are documented on) the single visible entry pcb_place::place_board. The agent
    // overrides any of this with the interactive geometry tools (move_part/route_track).
    let result = pcb_place::placement::place_board(&problem, &hints);

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
        let pairs = pcb_place::placement::decoupling_pairs(&problem);
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
                        "{ic_ref} has {} decoupling caps the placer scattered. For a tidy ring, \
                         add a `place.groups` entry to the DSL — {{<name>: {{members: [<the caps>], \
                         surround: {ic_ref}}}}} — and design_board again, then place_board.",
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
            "placement is NOT legal — the board is too tight for these parts. The fix is more \
             room, not rearrangement: enlarge `board.outline` in the DSL to at least \
             suggested_min_bounds_mm and design_board again, then re-run place_board. \
             Locking/nudging parts won't help when the board is simply too small for the courtyards."
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

// ── placement-hint helpers (group parsing for the DSL) ───────────────────────

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
fn parse_edge(v: &Value) -> std::result::Result<pcb_place::placement::Edge, String> {
    use pcb_place::placement::Edge;
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


// ── keepout helpers (rect parsing for the DSL) ───────────────────────────────

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
    solution: &pcb_model::RouteSolution,
    failed: &[FailedNet],
    plane_nets: &std::collections::BTreeSet<String>,
) -> LintSplit {
    let failed_nets: std::collections::BTreeSet<&str> =
        failed.iter().map(|f| f.connection.as_str()).collect();

    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut real = 0usize;
    let mut expected_gaps = 0usize;

    for v in lint(rp, solution) {
        // An Unconnected is an EXPECTED gap (not an engine bug) when:
        //  - the net was already reported failed (an honest finisher/global drop), OR
        //  - the net is a copper PLANE net. A plane net is NOT trace-routed — it was
        //    removed from the routed connections and its pins connect through the inner
        //    plane (emitted at export) plus a per-pad stitching via; any pad whose via
        //    couldn't be placed is already reported as a failed stitch. So the trace-
        //    connectivity oracle, which sees the stitch vias but not the plane copper,
        //    reads every stitched plane pad as "unconnected" — a false signal. KiCAD DRC
        //    (which has the plane) is the authority on real plane connectivity.
        if let DrcViolation::Connectivity {
            violation: ConnViolation::Unconnected { connection, .. },
        } = &v
            && (failed_nets.contains(connection.as_str()) || plane_nets.contains(connection))
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

/// When routing leaves nets unrouted, decide whether they CONCENTRATE on one part (a fine-pitch
/// pin-escape bottleneck — not fixable by moving OTHER parts) or are scattered (movable congestion).
/// Returns `(reference, footprint, pins_on_this_part, total_failed_pins)` for the dominant part when
/// it owns a clear majority (≥60%) of the failed-net pad incidences across ≥3 pins; else `None`.
/// Plane-stitch pseudo-failures ("<N plane stitching vias>") are not net names, so they don't match
/// any pad and are naturally excluded.
fn escape_bottleneck(
    parts: &[DraftPart],
    failed: &[FailedNet],
) -> Option<(String, String, usize, usize)> {
    let failed_nets: std::collections::BTreeSet<&str> =
        failed.iter().map(|f| f.connection.as_str()).collect();
    let mut per_part: Vec<(String, String, usize)> = Vec::new();
    let mut total = 0usize;
    for p in parts {
        let c = p
            .pad_nets
            .values()
            .filter(|n| failed_nets.contains(n.as_str()))
            .count();
        if c > 0 {
            per_part.push((p.reference.clone(), p.footprint.clone(), c));
            total += c;
        }
    }
    per_part.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    let (r, fp, c) = per_part.into_iter().next()?;
    (total >= 3 && c * 100 >= total * 60).then_some((r, fp, c, total))
}

pub fn route_board(_input: Value, ctx: &ToolCtx) -> Result<Value> {
    let Some(draft) = BoardDraft::load(ctx) else {
        return Ok(json!({
            "error": "no board draft yet — run derive_board first",
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
    // Per-net trace widths from the board rules: fat power copper, thin signals. (Plane
    // nets on a 4-layer board are pours, not traces, so a width on them is simply moot.)
    rp.net_widths = draft.rules.net_widths.clone();
    // Via size from the board rules. `to_route_problem` inherits it from the PlaceProblem,
    // which has no via field, so it defaults to 0.6/0.3 — meaning the router's SIGNAL vias
    // ignored rules.via_diameter entirely (only the stitch vias + the .kicad_pro min-via
    // honoured it). On a board with a non-default via that mismatch shipped DRC faults
    // (e.g. a 1.0mm rule sets min-via 0.95 but the router emitted 0.6 vias → via_diameter
    // violations the in-house lint never sees), and made the via-size lever a no-op for
    // signal escape. Propagate it so EVERY via — signal, stitch, fanout — is one size.
    rp.via_diameter = draft.rules.via_diameter;
    rp.via_drill = draft.rules.via_drill;

    // On a multilayer board the highest-fanout power/ground nets become copper
    // PLANES (emitted as zones at export) instead of point-to-point traces — the
    // only way a dense part's many power pins connect. Signals route on the outer
    // pair; each plane pad is stitched up to its plane with a through-via.
    let planes = assign_planes(&draft);
    let (result, planes) = if planes.is_empty() {
        (route_auto(&rp), planes)
    } else {
        let r0 = route_with_planes(rp.clone(), &planes, &draft.rules);
        // Only the ADJACENT inner plane (In1) is reachable by a micro via-in-pad escape, so which
        // power net sits there decides how many fine-pitch balls route (HDI increment 3). If the
        // default assignment STRANDED plane pads and there are exactly two planes, try the other
        // layer assignment and keep whichever lands more plane copper — the DRC oracle gates both,
        // so this only ever trades honest-unrouted for routed, never correctness. The 2× route cost
        // is paid only when the first pass actually left strands (a fully-routed board skips it).
        let stranded = r0
            .failed
            .iter()
            .any(|f| f.connection.contains("plane stitching"));
        if stranded && planes.len() == 2 {
            let swapped = vec![
                (planes[0].0.clone(), planes[1].1),
                (planes[1].0.clone(), planes[0].1),
            ];
            let r1 = route_with_planes(rp.clone(), &swapped, &draft.rules);
            if r1.solution.vias.len() > r0.solution.vias.len() {
                (r1, swapped)
            } else {
                (r0, planes)
            }
        } else {
            (r0, planes)
        }
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
    let plane_net_set: std::collections::BTreeSet<String> =
        planes.iter().map(|(n, _)| n.clone()).collect();
    let split = lint_summary(&rp, &result.solution, &result.failed, &plane_net_set);

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
        // First distinguish a fine-pitch ESCAPE bottleneck (failures concentrated on ONE part) from
        // scattered congestion — they call for opposite triage, and steering the model to move OTHER
        // parts on an escape limit just burns iterations on an unfixable failure.
        if let Some((r, fp, c, total)) = escape_bottleneck(&draft.parts, &result.failed) {
            out["escape_bottleneck"] =
                json!({ "ref": r, "footprint": fp, "failed_pins": c, "total_failed_pins": total });
            out["note"] = json!(format!(
                "{c} of {total} unrouted pins belong to ONE part — {r} ({fp}). This is a fine-pitch \
                 PIN-ESCAPE bottleneck, not movable congestion: those inner pins have no room to \
                 escape the field, so moving OTHER parts or relaxing clearance will NOT help. \
                 Options, best first: (1) give {r} more open margin (no parts/keepouts hugging it) \
                 and re-place; (2) route fewer of its signals, or split them across more connectors; \
                 (3) choose a coarser-pitch footprint; (4) ACCEPT these as a density limit — the \
                 board is fidelity-clean (0 DRC faults) and these nets are honestly unrouted, so it \
                 is safe to export as-is. NOTE: adding copper layers does not currently relieve a \
                 fine-pitch signal escape (only power/ground pins benefit, via planes)."
            ));
        } else {
            out["note"] = json!(
                "some nets did not route — read each failure `reason` (global:/assign:/cell:/\
                 finisher: provenance) and the congestion hotspots, then triage by RE-AUTHORING \
                 the Board-DSL and design_board: spread parts via `place.groups`, enlarge \
                 `board.outline`, relax `rules`, or replace a blocking `keepout` with a gapped \
                 pair. Re-place and re-route after each change."
            );
        }
        // Layer escalation: on a 2-layer board with unrouted nets, more copper layers
        // are usually the highest-leverage fix — a 4-layer stack adds GND/VCC PLANES
        // (so every power pin connects through a via instead of competing for surface
        // copper) and keeps F/B clear for signals. The engine supports 2 or 4 layers;
        // the agent chooses via create_board rules.layers (it is not auto-escalated, so
        // routing stays deterministic and the board's layer count is an explicit design
        // choice). Surface the option here so the model knows to reach for it.
        if rp.layer_count <= 2 {
            out["layer_suggestion"] = json!(format!(
                "{} net(s) failed on a 2-layer board. Re-run derive_board with rules.layers=4: \
                 it adds GND+VCC power planes (power pins drop straight to a plane via a \
                 drilled via) and frees F.Cu/B.Cu for signals — usually the biggest win on \
                 dense or multi-power-net boards. Then re-place and re-route.",
                result.failed.len()
            ));
        }
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
    // Up to 2 planes (GND/VCC), placed on the CENTRED plane layers for this stackup
    // (4-layer → In1,In2; 6-layer → In2,In3) so the other inner layers stay signal.
    // Must match pcb_engine's `plane_mask_for`, or the router would route on a plane.
    let plane_idx = grid_astar::router::plane_layers(draft.rules.layer_count as usize);
    nets.into_iter()
        .take(plane_idx.len())
        .enumerate()
        .map(|(i, (net, _))| (net, plane_idx[i]))
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

/// Would a stitch via at `at` (radius `via_r`) on net `net` clear every FOREIGN pad,
/// via, and routed track? (Same-net copper is fine to touch.)
#[allow(clippy::too_many_arguments)] // internal clearance helper; flat args keep the hot loop readable
fn stitch_via_clears(
    at: &Point2,
    net: &str,
    obstacles: &[Obstacle],
    vias: &[pcb_model::Via],
    traces: &[pcb_model::Trace],
    via_r: f64,
    via_drill: f64,
    clearance: f64,
) -> bool {
    let min_via2 = (2.0 * via_r + clearance).powi(2);
    obstacles.iter().all(|ob| {
        ob.connected_to.iter().any(|n| n == net) || {
            let dx = (at.x - ob.center.x).abs() - ob.width / 2.0;
            let dy = (at.y - ob.center.y).abs() - ob.height / 2.0;
            dx.max(0.0).powi(2) + dy.max(0.0).powi(2) >= (via_r + clearance).powi(2)
        }
    }) && vias.iter().all(|v| {
        let c2 = (at.x - v.at.x).powi(2) + (at.y - v.at.y).powi(2);
        // Drill-to-drill (hole) clearance applies to EVERY via pair — two barrels cannot
        // overlap, same net or not (else a KiCAD hole-to-hole / holes_co_located fault, which
        // the net-independent lint catches and then drops the WHOLE plane net, disconnecting
        // all its power). A FOREIGN via additionally needs full copper clearance (min_via2,
        // already hole-aware via clr_via); a same-net via may share copper but never a hole.
        let hole2 = (via_drill / 2.0 + v.drill / 2.0 + KICAD_HOLE_CLEAR_MM).powi(2);
        let need2 = if v.connection == net { hole2 } else { min_via2.max(hole2) };
        c2 >= need2
    }) && traces.iter().all(|t| {
            t.connection == net || {
                let need = via_r + t.width / 2.0 + clearance;
                !t.path.windows(2).any(|w| seg_point_dist(&w[0], &w[1], at) < need)
            }
        })
}

/// Would a straight fanout trace `a`→`b` (half-width `hw`) on net `net` clear every
/// FOREIGN pad, VIA, and routed track? Sampled densely along the (short) segment.
/// Checking foreign vias here is what the first fanout attempt missed (the bga100
/// clearance faults were the trace grazing a via).
#[allow(clippy::too_many_arguments)] // internal clearance helper; flat args keep the hot loop readable
fn fanout_seg_clears(
    a: &Point2,
    b: &Point2,
    net: &str,
    obstacles: &[Obstacle],
    vias: &[pcb_model::Via],
    traces: &[pcb_model::Trace],
    hw: f64,
    clearance: f64,
) -> bool {
    let len = ((b.x - a.x).powi(2) + (b.y - a.y).powi(2)).sqrt();
    let n = ((len / 0.1).ceil() as usize).max(8);
    for i in 0..=n {
        let t = i as f64 / n as f64;
        let p = Point2 { x: a.x + t * (b.x - a.x), y: a.y + t * (b.y - a.y) };
        let pad_ok = obstacles.iter().all(|ob| {
            ob.connected_to.iter().any(|nn| nn == net) || {
                let dx = (p.x - ob.center.x).abs() - ob.width / 2.0;
                let dy = (p.y - ob.center.y).abs() - ob.height / 2.0;
                dx.max(0.0).powi(2) + dy.max(0.0).powi(2) >= (hw + clearance).powi(2)
            }
        });
        if !pad_ok {
            return false;
        }
        let via_ok = vias.iter().all(|v| {
            v.connection == net || {
                let need = hw + v.diameter / 2.0 + clearance;
                (p.x - v.at.x).powi(2) + (p.y - v.at.y).powi(2) >= need * need
            }
        });
        if !via_ok {
            return false;
        }
        let trk_ok = traces.iter().all(|tr| {
            tr.connection == net || {
                let need = hw + tr.width / 2.0 + clearance;
                !tr.path.windows(2).any(|w| seg_point_dist(&w[0], &w[1], &p) < need)
            }
        });
        if !trk_ok {
            return false;
        }
    }
    true
}

fn route_with_planes(
    mut rp: RouteProblem,
    planes: &[(String, u32)],
    rules: &DraftRules,
) -> negotiated_mesh::pipeline::RouteResult {
    rp.layer_count = 2;
    let plane_names: std::collections::BTreeSet<String> =
        planes.iter().map(|(n, _)| n.clone()).collect();
    // net → its copper-plane layer index (1 = In1 …) for the HDI via-in-pad fallback below.
    let plane_layer: std::collections::BTreeMap<&str, u32> =
        planes.iter().map(|(n, l)| (n.as_str(), *l)).collect();
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
    let clr = rules.clearance;
    // A FANOUT via lands in fresh board, so unlike the in-place stitch it can sit near
    // another drilled hole. KiCAD's hole-to-hole rule (0.25mm) is on DRILL edges, but
    // our clearance is on COPPER edges; the via's drill is `via_annular` inside its
    // copper, so a copper gap of `clr` only buys a hole gap of `clr + via_annular` when
    // the neighbour has ~0 annular. Bump the fanout via's clearance so the hole gap
    // clears 0.25 with margin — this is what makes the fanout DRC-clean WITHOUT a
    // drill-aware obstacle model (the deficit was always just this arithmetic).
    let via_annular = (rules.via_diameter - rules.via_drill) / 2.0;
    let clr_via = clr.max(KICAD_HOLE_CLEAR_MM - via_annular + 0.05);
    const DIRS: [(f64, f64); 8] = [
        (1.0, 0.0), (-1.0, 0.0), (0.0, 1.0), (0.0, -1.0),
        (0.707, 0.707), (-0.707, 0.707), (0.707, -0.707), (-0.707, -0.707),
    ];
    let step = rules.via_diameter + clr;
    let mut skipped = 0usize;
    for (net, at, thru) in stitches {
        // A through-hole plane pad already connects to its inner plane (its barrel
        // spans every copper layer); a stitching via here would only co-locate a
        // second drill with the pad's own hole.
        if thru {
            continue;
        }
        // 1) In place: drop the stitch via on the pad if it clears. Uses the
        //    hole-clearance-aware clr_via (not bare clr) — the via's drill must clear a
        //    neighbour's drill by KiCAD's 0.25mm, same as a fanout via; at a fine
        //    clearance (e.g. 0.1mm) bare clr left only a clr+via_annular hole gap < 0.25
        //    (a latent fault the 0.13mm boards passed only by luck). For the default
        //    0.2mm clearance clr_via == clr, so those boards are unchanged.
        if stitch_via_clears(
            &at, &net, &rp.obstacles, &result.solution.vias, &result.solution.traces, via_r,
            rules.via_drill, clr_via,
        ) {
            result.solution.vias.push(Via {
                connection: net,
                at,
                diameter: rules.via_diameter,
                drill: rules.via_drill,
                span: ViaSpan::Through,
            });
            continue;
        }
        // 2) FANOUT: a fine-pitch pad has no room for a via between neighbours, but
        //    open board nearby does. Route a short outward trace to the first open
        //    point where the via AND the trace clear, then stitch there — the dog-bone
        //    escape that routes a perimeter power pin an in-place via can't. Stays
        //    inside the board/outline; conservative so it never ships a DRC fault.
        let tw = rp.net_width(&net);
        let mut placed = false;
        'search: for k in 1..=4 {
            let r = k as f64 * step;
            for (dx, dy) in DIRS {
                let cand = Point2 { x: at.x + dx * r, y: at.y + dy * r };
                let in_board = cand.x - via_r >= rp.bounds.min_x + clr
                    && cand.x + via_r <= rp.bounds.max_x - clr
                    && cand.y - via_r >= rp.bounds.min_y + clr
                    && cand.y + via_r <= rp.bounds.max_y - clr
                    && rp
                        .outline
                        .as_ref()
                        .is_none_or(|poly| pcb_model::point_in_polygon(&cand, poly));
                if in_board
                    && stitch_via_clears(
                        &cand, &net, &rp.obstacles, &result.solution.vias,
                        &result.solution.traces, via_r, rules.via_drill, clr_via,
                    )
                    && fanout_seg_clears(
                        &at, &cand, &net, &rp.obstacles, &result.solution.vias,
                        &result.solution.traces, tw / 2.0, clr,
                    )
                {
                    result.solution.traces.push(pcb_model::Trace {
                        connection: net.clone(),
                        layer: LayerRef::top(),
                        width: tw,
                        path: vec![at.clone(), cand.clone()],
                    });
                    result.solution.vias.push(Via {
                        connection: net.clone(),
                        at: cand,
                        diameter: rules.via_diameter,
                        drill: rules.via_drill,
                        span: ViaSpan::Through,
                    });
                    placed = true;
                    break 'search;
                }
            }
        }
        // 3) HDI VIA-IN-PAD fallback: a smaller blind/micro via reaches this pad's own plane
        //    where a full through-via has no room between fine-pitch balls. `micro` when the
        //    plane is the layer directly below the top (adjacent, F→In1), `blind` for a deeper
        //    inner plane (F→In2…). It sits IN the pad (same net), so KiCAD's zone fill connects
        //    it in its own plane and carves an anti-pad in any foreign plane it pierces — no
        //    short. The DRC oracle (drop_violating_copper + kicad-cli) still gates it: a via that
        //    can't clear is dropped and reported unrouted, never shipped failing.
        if !placed {
            // Only a true laser MICROVIA helps the room problem: KiCAD relaxes the size rule for
            // micro vias (down to the netclass microvia min) but holds blind/buried vias to the
            // FULL through-via minimum — so a blind via is never smaller than the through-via that
            // already failed to fit here, and would only re-trip the size/room limit. We therefore
            // emit a micro via only when the pad's plane is the layer DIRECTLY below the top
            // (adjacent F→In1): the via-in-pad drops straight onto its own plane (same net → KiCAD's
            // zone fill connects it; nothing foreign is pierced → no anti-pad, no short). A deeper
            // inner plane (In2…) would need STACKED micro vias; those are kicad-cli-DRC-valid
            // (verified) but their co-located holes trip the in-house lint's hole-clearance check,
            // which then drops the whole net — so the stack needs a span-aware hole exemption first
            // (see docs/specs/hdi-microvia-feasibility.md). The DRC oracle still gates this; a via
            // that can't clear is reported unrouted, never shipped failing.
            if plane_layer.get(net.as_str()) == Some(&1)
                && stitch_via_clears(
                    &at, &net, &rp.obstacles, &result.solution.vias,
                    &result.solution.traces, HDI_VIA_DIAMETER / 2.0, HDI_VIA_DRILL, clr_via,
                )
            {
                result.solution.vias.push(Via {
                    connection: net.clone(),
                    at: at.clone(),
                    diameter: HDI_VIA_DIAMETER,
                    drill: HDI_VIA_DRILL,
                    span: ViaSpan::Partial { from: 0, to: 1, micro: true },
                });
                placed = true;
            }
        }
        if !placed {
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
    // Final fidelity pass — close the structural gap that the stitch/fanout vias above
    // are added in the AGENT layer, OUTSIDE the engine's route_auto reconcile, so they
    // never saw the DRC oracle. Route the COMPLETE copper (engine route + plane stitches)
    // back through the SAME geometry lint and drop any net whose copper still violates
    // clearance/via/width/bounds. The engine must never EMIT a DRC-failing via — even a
    // hand-rolled stitch check that slips at an unusual via/clearance is caught here, and
    // the net is reported honestly unrouted instead of shipping a fault. SAFE: a clean
    // board lints to zero so nothing is dropped (verified byte-for-byte on the 57-board
    // suite); only a would-fault config trades copper for an honest failure.
    let dropped = drc_lint::lint::drop_violating_copper(&rp, &mut result.solution);
    for net in dropped {
        if !result.failed.iter().any(|f| f.connection == net) {
            result.failed.push(FailedNet {
                connection: net,
                reason: "DRC oracle: dropped after stitching — copper could not clear \
                         at this via/clearance (reported unrouted, never shipped failing)"
                    .to_owned(),
            });
        }
    }
    result
}

/// Build the copper-plane zones for export: one pour per plane net, filling the
/// Drop floating copper ISLANDS from a precomputed plane/pour fill: keep only the
/// rectangles reachable (by shared-edge adjacency) from one that covers a same-net
/// ANCHOR (a pad, via, or this net's own trace point). KiCAD treats edge-sharing
/// `filled_polygon`s as one connected pour, so an island with no anchor is copper that
/// connects to nothing — KiCAD would delete it on fill and DRC reports `isolated_copper`.
/// Pruning it up front keeps the pour professional (no floating fills) and cannot affect
/// connectivity: a removed island, by definition, carried no net connection. Empty
/// `anchors` ⇒ no-op (never drop a pour we cannot anchor).
fn prune_islands(rects: Vec<[f64; 4]>, anchors: &[Point2]) -> Vec<[f64; 4]> {
    const EPS: f64 = 1e-6;
    let n = rects.len();
    if n == 0 || anchors.is_empty() {
        return rects;
    }
    let covers = |r: &[f64; 4], a: &Point2| {
        a.x >= r[0] - EPS && a.x <= r[2] + EPS && a.y >= r[1] - EPS && a.y <= r[3] + EPS
    };
    // Two tiling rects connect iff they share a positive-length edge (abut on one axis,
    // overlap on the other).
    let touch = |a: &[f64; 4], b: &[f64; 4]| {
        let xov = a[2].min(b[2]) - a[0].max(b[0]);
        let yov = a[3].min(b[3]) - a[1].max(b[1]);
        (yov.abs() < EPS && xov > EPS) || (xov.abs() < EPS && yov > EPS)
    };
    let mut keep = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    for (i, r) in rects.iter().enumerate() {
        if anchors.iter().any(|a| covers(r, a)) {
            keep[i] = true;
            stack.push(i);
        }
    }
    while let Some(i) = stack.pop() {
        for j in 0..n {
            if !keep[j] && touch(&rects[i], &rects[j]) {
                keep[j] = true;
                stack.push(j);
            }
        }
    }
    rects
        .into_iter()
        .enumerate()
        .filter(|(i, _)| keep[*i])
        .map(|(_, r)| r)
        .collect()
}

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
    outline: Option<&[Point2]>,
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
            // Same-net anchors (this plane's stitching vias + its own pads on the layer):
            // fill islands not reachable from one are floating copper — prune them.
            let mut anchors: Vec<Point2> = Vec::new();
            for v in &solution.vias {
                if &v.connection == net {
                    anchors.push(v.at.clone());
                }
            }
            for ob in &board.obstacles {
                let on_layer = ob.layers.iter().any(|lr| lr.index(layer_count) == Some(*layer_idx));
                if on_layer && ob.connected_to.iter().any(|n| n == net) {
                    anchors.push(ob.center.clone());
                }
            }
            let fill = prune_islands(
                plane_fill_rects(bounds, BOARD_EDGE_MARGIN_MM, &keepouts, outline),
                &anchors,
            );
            ZoneSpec {
                net_name: net.clone(),
                layer_name: format!("In{layer_idx}.Cu"),
                fill_rects: fill,
                clearance: rules.clearance,
                min_thickness: rules.min_trace_width.min(ZONE_MIN_THICKNESS_CAP_MM),
            }
        })
        .collect()
}

/// Safety margin added to a plane anti-pad beyond bare (radius + clearance) — a
/// bare keep-out lands exactly on the clearance limit and KiCAD flags it.
const PLANE_ANTIPAD_MARGIN_MM: f64 = 0.15;

/// Cap on a copper zone's `min_thickness`. KiCAD's zone fill removes slivers thinner than
/// `min_thickness` by a deflate/inflate (≈ min_thickness/2 each), and that inflate can grow
/// the fill back INTO a via's anti-pad, eating its clearance. Tying min_thickness to a large
/// `min_trace_width` (e.g. all-0.4mm traces) made the inflate (0.2) exceed the anti-pad margin
/// (0.15) → KiCAD's re-fill produced via-to-plane clearance faults the in-house lint never sees
/// (it doesn't model plane copper). Capping min_thickness keeps the inflate < the anti-pad
/// margin so the pour stays clearance-correct at any trace width; 0.25mm is KiCAD's own default
/// zone minimum, so normal boards (min_trace ≤ 0.25) are unchanged.
const ZONE_MIN_THICKNESS_CAP_MM: f64 = 0.25;

/// Map a pour layer string to its (copper-layer index, KiCAD layer name) on an
/// `lc`-layer board: `top`/`F.Cu` → 0, `bottom`/`B.Cu` → lc-1, `innerN`/`InN.Cu` → N.
/// Returns `None` for an out-of-range or unparseable layer. (A pour on a GND/VCC PLANE
/// layer is resolvable here but rejected at `create_board` — a plane is already full
/// copper.) This is what lets a GND fill sit on an inner SIGNAL layer (In1/In4 on a
/// 6-layer board) for shielding / impedance reference, not just top/bottom.
fn resolve_pour_layer(layer: &str, lc: u32) -> Option<(u32, String)> {
    let idx = match layer {
        "top" | "F.Cu" => 0,
        "bottom" | "B.Cu" => lc.saturating_sub(1),
        other => other
            .strip_prefix("inner")
            .or_else(|| other.strip_prefix("In").and_then(|s| s.strip_suffix(".Cu")))
            .and_then(|n| n.parse::<u32>().ok())?,
    };
    if idx >= lc {
        return None;
    }
    let kname = if idx == 0 {
        "F.Cu".to_string()
    } else if idx == lc - 1 {
        "B.Cu".to_string()
    } else {
        format!("In{idx}.Cu")
    };
    Some((idx, kname))
}

/// Build copper-POUR zones on SIGNAL layers (top/bottom) for the requested nets — a
/// GND flood for HF return paths / shielding, or a 2-layer ground plane. Unlike an
/// inner PLANE, a signal layer carries traces, so the anti-pad keep-out is carved
/// around foreign VIAS, foreign PADS, foreign TRACES (per segment), and user keepouts
/// on that layer. Same-net copper is NOT carved, so the pour ties the net's own pads/
/// traces/vias together (and, on a 4-layer board, to the inner plane through the net's
/// existing stitching vias).
fn pour_zones(
    pours: &[PourSpec],
    board: &RouteProblem,
    solution: &RouteSolution,
    bounds: &Bounds,
    rules: &DraftRules,
    user_keepouts: &[Keepout],
    outline: Option<&[Point2]>,
) -> Vec<ZoneSpec> {
    let lc = rules.layer_count;
    let via_half = rules.via_diameter / 2.0 + rules.clearance + PLANE_ANTIPAD_MARGIN_MM;
    pours
        .iter()
        .filter_map(|p| {
            let (idx, kname) = resolve_pour_layer(&p.layer, lc)?;
            let net = &p.net;
            let mut ko: Vec<(Point2, f64, f64)> = Vec::new();
            // Foreign vias (a through via reaches every layer).
            for v in &solution.vias {
                if &v.connection != net {
                    ko.push((v.at.clone(), via_half, via_half));
                }
            }
            // Foreign pads on this layer.
            for ob in &board.obstacles {
                let on = ob.layers.iter().any(|l| l.index(lc) == Some(idx));
                if on && !ob.connected_to.iter().any(|n| n == net) {
                    let h = ob.width.max(ob.height) / 2.0 + rules.clearance + PLANE_ANTIPAD_MARGIN_MM;
                    ko.push((ob.center.clone(), h, h));
                }
            }
            // Foreign traces on this layer — carve each segment's bbox + half-width.
            for t in &solution.traces {
                if t.layer.index(lc) == Some(idx) && &t.connection != net {
                    let inf = t.width / 2.0 + rules.clearance + PLANE_ANTIPAD_MARGIN_MM;
                    for w in t.path.windows(2) {
                        let (a, b) = (&w[0], &w[1]);
                        ko.push((
                            Point2 { x: (a.x + b.x) / 2.0, y: (a.y + b.y) / 2.0 },
                            (a.x - b.x).abs() / 2.0 + inf,
                            (a.y - b.y).abs() / 2.0 + inf,
                        ));
                    }
                }
            }
            // User keepouts on this layer.
            for k in user_keepouts {
                if k.layers.iter().any(|l| l.index(lc) == Some(idx)) {
                    let cx = (k.rect.min_x + k.rect.max_x) / 2.0;
                    let cy = (k.rect.min_y + k.rect.max_y) / 2.0;
                    ko.push((
                        Point2 { x: cx, y: cy },
                        (k.rect.max_x - k.rect.min_x) / 2.0 + rules.clearance,
                        (k.rect.max_y - k.rect.min_y) / 2.0 + rules.clearance,
                    ));
                }
            }
            // Same-net anchors on this layer (pads + vias + this net's own traces):
            // any fill island not reachable from one is floating copper — prune it.
            let mut anchors: Vec<Point2> = Vec::new();
            for v in &solution.vias {
                if &v.connection == net {
                    anchors.push(v.at.clone());
                }
            }
            for ob in &board.obstacles {
                let on = ob.layers.iter().any(|l| l.index(lc) == Some(idx));
                if on && ob.connected_to.iter().any(|n| n == net) {
                    anchors.push(ob.center.clone());
                }
            }
            for t in &solution.traces {
                if t.layer.index(lc) == Some(idx) && &t.connection == net {
                    anchors.extend(t.path.iter().cloned());
                }
            }
            let fill = prune_islands(plane_fill_rects(bounds, BOARD_EDGE_MARGIN_MM, &ko, outline), &anchors);
            Some(ZoneSpec {
                net_name: net.clone(),
                layer_name: kname,
                fill_rects: fill,
                clearance: rules.clearance,
                min_thickness: rules.min_trace_width.min(ZONE_MIN_THICKNESS_CAP_MM),
            })
        })
        .collect()
}

/// Render the board to a PNG using the placement or routed SVG, save under
/// `.autopcb/renders/`, and attach via `IMAGE_PATH_KEY`.
///
/// `view` may be `"placed"` or `"routed"`. When omitted the default is
/// `"routed"` when `route.json` exists, `"placed"` otherwise.
pub fn render_board(input: Value, ctx: &ToolCtx) -> Result<Value> {
    // ── load draft ───────────────────────────────────────────────────────────
    let Some(draft) = crate::tools_pcb::BoardDraft::load(ctx) else {
        return Ok(json!({
            "error": "no board draft yet — run derive_board first",
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
            pcb_svg::svg::render_placement(&problem, &draft.hints, &result)
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
            pcb_svg::svg::render_svg(&rp, &stored.solution, &stored.failed)
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
    keepouts: &[Keepout],
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
    // Keepouts are ROUTING obstacles (copper is kept out of them), NOT board-defining
    // features. Including them inflated the outline whenever a keepout sat in otherwise
    // empty space (e.g. planes-keepout: parts in the upper-left, a keepout on the far
    // right → a board twice as wide as the copper, scored "vastly oversized" by the
    // critic). The outline tightens to actual COPPER; a keepout in dead space no longer
    // bloats the board. (No copper ever sits inside a keepout, so this can't clip anything.)
    let _ = keepouts;
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
/// Write a sibling `.kicad_pro` for `board_path` declaring the board's design rules as
/// the "Default" net class, so KiCAD DRC (and any downstream tool that opens the board)
/// checks copper against the engine's clearance / trace width / via — NOT KiCAD's
/// built-in 0.2 mm netclass default, which false-flags a finer-pitch board whose
/// footprint pads are inherently closer than 0.2 mm. KiCAD loads the same-stem project.
fn write_kicad_project(board_path: &std::path::Path, rules: &DraftRules) -> std::io::Result<()> {
    let stem = board_path.file_stem().and_then(|s| s.to_str()).unwrap_or("board");
    let pro = board_path.with_extension("kicad_pro");
    let vmin = (rules.via_diameter - 0.05).max(0.1);
    // IMPORTANT (verified empirically Jun 19): `kicad-cli pcb drc` reads the NET_SETTINGS classes
    // below — so the netclass clearance / track width / via size DO gate DRC against the engine's
    // rules — but it does NOT enforce this `design_settings.rules` MINIMUMS block: setting
    // min_via_diameter to 2.0 here did not flag 0.6 mm vias. These minimums are therefore for the
    // KiCAD GUI only; kicad-cli falls back to its built-in constraint defaults (via ≥ 0.5, drill ≥
    // 0.3, annular ≥ 0.1, hole-to-hole ≥ 0.25, copper-to-edge ≥ 0.5). That is SAFE because
    // create_board's via pre-checks (KICAD_MIN_VIA_*) and the in-house lint (HOLE_CLEAR / EDGE_CLEAR
    // = 0.25 / 0.5) are set to MATCH those same defaults — the DRC gate is meaningful via the
    // netclass + matching pre-checks, NOT this block. Don't add a rule here expecting kicad-cli to
    // honour it (it won't); enforce new minimums in the in-house lint + a create_board pre-check.
    let doc = json!({
        "board": {"design_settings": {"rules": {
            "min_clearance": 0.0, "min_track_width": 0.0,
            "min_via_diameter": vmin, "min_through_hole_diameter": 0.1
        }}},
        "meta": {"filename": format!("{stem}.kicad_pro"), "version": 1},
        "net_settings": {"classes": [{
            "name": "Default",
            "clearance": rules.clearance, "track_width": rules.min_trace_width,
            "via_diameter": rules.via_diameter, "via_drill": rules.via_drill,
            "microvia_diameter": 0.3, "microvia_drill": 0.1,
            "diff_pair_gap": 0.25, "diff_pair_width": 0.2,
            "bus_width": 12.0, "line_style": 0, "wire_width": 6.0,
            "pcb_color": "rgba(0, 0, 0, 0.000)", "schematic_color": "rgba(0, 0, 0, 0.000)"
        }], "meta": {"version": 3}}
    });
    std::fs::write(pro, serde_json::to_string_pretty(&doc).unwrap_or_default())
}

/// drc` and reports the counts.
pub fn export_board(input: Value, ctx: &ToolCtx) -> Result<Value> {
    let Some(draft) = BoardDraft::load(ctx) else {
        return Ok(json!({
            "error": "no board draft yet — run derive_board first",
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
    let tight = content_bounds(&board.problem, &stored.solution, &draft.bounds, &draft.keepouts, BOARD_EDGE_MARGIN_MM);
    // Copper-plane zones (power pours) for a multilayer board, computed from the
    // foreign copper reaching each inner layer, at the final (tight) bounds. The
    // obstacle positions are absolute, so the first board's read is reusable here.
    // Planes/pours fill the BOARD: for a custom outline that's the outline's bbox (the
    // fill is then clipped to the polygon), not the content-tight bbox — otherwise a pour
    // shrinks to a square around the parts instead of flooding the shaped board.
    let fill_bounds = if draft.outline.is_some() { &draft.bounds } else { &tight };
    let outline = draft.outline.as_deref();
    let mut zones = plane_zones(
        &stored.planes,
        &board.problem,
        &stored.solution,
        fill_bounds,
        &draft.rules,
        &draft.keepouts,
        outline,
    );
    // Signal-layer copper pours (GND flood for HF return / 2-layer ground plane).
    zones.extend(pour_zones(
        &draft.rules.pours,
        &board.problem,
        &stored.solution,
        fill_bounds,
        &draft.rules,
        &draft.keepouts,
        outline,
    ));
    // Export each routing keep-out as a KiCAD rule area so the finished board carries the
    // design intent the placer/router worked around — and KiCAD independently confirms no
    // track/via landed inside it. Resolve each keep-out's layers to KiCAD names; drop a
    // keep-out whose layers don't resolve rather than emit a malformed zone.
    let keepout_zones: Vec<pcb_synth::synth::KeepoutZone> = draft
        .keepouts
        .iter()
        .map(|k| {
            let layers: Vec<String> = k
                .layers
                .iter()
                .filter_map(|l| resolve_pour_layer(&l.0, draft.rules.layer_count).map(|(_, n)| n))
                .collect();
            pcb_synth::synth::KeepoutZone {
                layers,
                min: [k.rect.min_x, k.rect.min_y],
                max: [k.rect.max_x, k.rect.max_y],
            }
        })
        .filter(|k| !k.layers.is_empty())
        .collect();
    let board = if tight != draft.bounds
        || !zones.is_empty()
        || !keepout_zones.is_empty()
        || draft.outline.is_some()
    {
        match synthesize_board_full(
            &parts,
            &tight,
            draft.rules.layer_count,
            &zones,
            &keepout_zones,
            draft.outline.as_deref(),
        ) {
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
    // Sibling .kicad_pro declaring the board's design rules, so kicad-cli DRC (and any
    // downstream tool) checks against the SAME clearance/width/via the router used — not
    // KiCAD's 0.2 mm netclass default, which false-flags a finer-pitch board. KiCAD loads
    // the same-stem project when opening the board.
    let _ = write_kicad_project(&path, &draft.rules);

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

#[cfg(test)]
mod pads_missing_tests {
    use super::pads_missing;

    #[test]
    fn flags_only_pads_absent_from_the_footprint() {
        // R-style: pins 1,2 on a footprint with pads 1,2 — nothing missing.
        assert!(pads_missing(["1", "2"], &["1", "2"]).is_empty());
        // A pin mapped to a pad the footprint lacks (3 on a 2-pad part) is flagged.
        assert_eq!(pads_missing(["1", "2", "3"], &["1", "2"]), vec!["3".to_string()]);
        // BGA-style alphanumeric pads; the inner ball isn't on a perimeter footprint.
        assert_eq!(
            pads_missing(["A1", "C5"], &["A1", "A2", "B1"]),
            vec!["C5".to_string()]
        );
        // Result is sorted + deduped.
        assert_eq!(pads_missing(["5", "5", "4"], &["1"]), vec!["4".to_string(), "5".to_string()]);
    }
}

#[cfg(test)]
mod escape_bottleneck_tests {
    use super::*;
    use std::collections::BTreeMap;

    fn part(reference: &str, footprint: &str, nets: &[(&str, &str)]) -> DraftPart {
        DraftPart {
            reference: reference.to_owned(),
            footprint: footprint.to_owned(),
            pad_nets: nets.iter().map(|(p, n)| (p.to_string(), n.to_string())).collect::<BTreeMap<_, _>>(),
            locked: None,
        }
    }
    fn failed(nets: &[&str]) -> Vec<FailedNet> {
        nets.iter().map(|n| FailedNet { connection: n.to_string(), reason: String::new() }).collect()
    }

    #[test]
    fn concentrated_failures_on_one_part_are_an_escape_bottleneck() {
        // A BGA whose 4 inner pins fail + a cap with 1 unrelated fail → 4/5 on U1 (≥60%) → flagged.
        let parts = vec![
            part("U1", "Package_BGA:BGA-100", &[("A1", "S1"), ("A2", "S2"), ("A3", "S3"), ("A4", "S4")]),
            part("C1", "Capacitor_SMD:C_0402", &[("1", "VCC"), ("2", "GNDX")]),
        ];
        let f = failed(&["S1", "S2", "S3", "S4", "GNDX"]);
        let got = escape_bottleneck(&parts, &f).expect("should flag a bottleneck");
        assert_eq!((got.0.as_str(), got.2, got.3), ("U1", 4, 5));
    }

    #[test]
    fn scattered_failures_are_not_a_bottleneck() {
        // 1 fail each on three different parts → no single part ≥60% → None (movable congestion).
        let parts = vec![
            part("U1", "fp", &[("1", "A")]),
            part("U2", "fp", &[("1", "B")]),
            part("U3", "fp", &[("1", "C")]),
        ];
        assert!(escape_bottleneck(&parts, &failed(&["A", "B", "C"])).is_none());
    }

    #[test]
    fn plane_stitch_pseudo_failures_are_ignored() {
        // "<N plane stitching vias>" is not a net name → matches no pad → no bottleneck.
        let parts = vec![part("U1", "fp", &[("1", "GND")])];
        assert!(escape_bottleneck(&parts, &failed(&["<7 plane stitching vias>"]) ).is_none());
    }
}

// ── Interactive IPC board editing ────────────────────────────────────────────
//
// Once the engine has seeded a board (derive_board → place_board → route_board →
// export_board), `open_board` launches a live headless KiCAD and the geometry
// tools edit the REAL board over IPC. This is where the LLM directly controls
// geometry (the engine is the assist that produced the starting point).

use kicad_ipc::proto::kiapi::board::types::BoardLayer;
use kicad_ipc::{footprint_reference, Session};

fn ipc_err(e: kicad_ipc::Error) -> anyhow::Error {
    anyhow::anyhow!(e.to_string())
}

fn mm_to_nm(mm: f64) -> i64 {
    (mm * 1_000_000.0).round() as i64
}

/// Parse a copper-layer name ("F.Cu", "B.Cu", "In1.Cu", "top", "bottom").
fn parse_copper_layer(name: &str) -> std::result::Result<BoardLayer, String> {
    Ok(match name.to_ascii_lowercase().replace('.', "_").as_str() {
        "f_cu" | "top" | "front" => BoardLayer::BlFCu,
        "b_cu" | "bottom" | "back" => BoardLayer::BlBCu,
        "in1_cu" | "in1" => BoardLayer::BlIn1Cu,
        "in2_cu" | "in2" => BoardLayer::BlIn2Cu,
        "in3_cu" | "in3" => BoardLayer::BlIn3Cu,
        "in4_cu" | "in4" => BoardLayer::BlIn4Cu,
        other => return Err(format!("unknown copper layer `{other}` (use F.Cu / B.Cu / In1.Cu …)")),
    })
}

/// Open the exported board in a live headless KiCAD for interactive editing.
pub fn open_board(_input: Value, ctx: &ToolCtx) -> Result<Value> {
    let path = ctx.pcb_path();
    if !path.exists() {
        return Ok(json!({
            "error": "no .kicad_pcb yet — seed the board first (derive_board → place_board → route_board → export_board), then open_board"
        }));
    }
    match Session::launch_headless(&path) {
        Ok(s) => *ctx.kicad() = Some(s),
        Err(e) => return Ok(json!({ "error": format!("could not open the board in KiCAD: {e}") })),
    }
    board_state(ctx)
}

/// Read the live board: footprints (ref + position mm), track/net counts.
pub fn board_state(ctx: &ToolCtx) -> Result<Value> {
    let mut guard = ctx.kicad();
    let Some(session) = guard.as_mut() else {
        return Ok(json!({ "error": "no board open — call open_board first" }));
    };
    let k = session.kicad();
    let fps = k.footprints().map_err(ipc_err)?;
    let tracks = k.tracks().map_err(ipc_err)?;
    let nets = k.nets().map_err(ipc_err)?;
    let parts: Vec<Value> = fps
        .iter()
        .map(|f| {
            let p = f.position.clone().unwrap_or_default();
            json!({
                "reference": footprint_reference(f),
                "x": p.x_nm as f64 / 1e6,
                "y": p.y_nm as f64 / 1e6,
            })
        })
        .collect();
    Ok(json!({
        "ok": true,
        "footprints": parts.len(),
        "parts": parts,
        "tracks": tracks.len(),
        "nets": nets,
    }))
}

/// Move a part (reference) to (x,y) mm, optional rotation degrees.
pub fn move_part(input: Value, ctx: &ToolCtx) -> Result<Value> {
    let reference = require_str(&input, "reference")?;
    let x = match req_num(&input, "x", "move_part") { Ok(v) => v, Err(e) => return Ok(json!({ "error": e })) };
    let y = match req_num(&input, "y", "move_part") { Ok(v) => v, Err(e) => return Ok(json!({ "error": e })) };
    let rot = input.get("rotation").and_then(Value::as_f64);
    let mut guard = ctx.kicad();
    let Some(session) = guard.as_mut() else {
        return Ok(json!({ "error": "no board open — call open_board first" }));
    };
    match session.kicad().move_footprint(&reference, mm_to_nm(x), mm_to_nm(y), rot) {
        Ok(()) => Ok(json!({ "ok": true, "reference": reference, "x": x, "y": y })),
        Err(e) => Ok(json!({ "error": e.to_string() })),
    }
}

/// Route a straight track segment: start [x,y], end [x,y] (mm), width (mm),
/// layer (F.Cu/…), optional net.
pub fn route_track(input: Value, ctx: &ToolCtx) -> Result<Value> {
    let start = input.get("start").and_then(|v| v.as_array());
    let end = input.get("end").and_then(|v| v.as_array());
    let (Some(s), Some(e)) = (start, end) else {
        return Ok(json!({ "error": "route_track needs `start` and `end` as [x,y] mm arrays" }));
    };
    let coord = |a: &[Value], i: usize| a.get(i).and_then(Value::as_f64);
    let (Some(sx), Some(sy), Some(ex), Some(ey)) = (coord(s, 0), coord(s, 1), coord(e, 0), coord(e, 1)) else {
        return Ok(json!({ "error": "start/end must be [x,y] numbers (mm)" }));
    };
    let width = input.get("width").and_then(Value::as_f64).unwrap_or(0.2);
    let layer = match parse_copper_layer(input.get("layer").and_then(Value::as_str).unwrap_or("F.Cu")) {
        Ok(l) => l,
        Err(err) => return Ok(json!({ "error": err })),
    };
    let net = input.get("net").and_then(Value::as_str);
    let mut guard = ctx.kicad();
    let Some(session) = guard.as_mut() else {
        return Ok(json!({ "error": "no board open — call open_board first" }));
    };
    match session.kicad().add_track(
        (mm_to_nm(sx), mm_to_nm(sy)),
        (mm_to_nm(ex), mm_to_nm(ey)),
        mm_to_nm(width),
        layer,
        net,
    ) {
        Ok(()) => Ok(json!({ "ok": true })),
        Err(e) => Ok(json!({ "error": e.to_string() })),
    }
}

/// Set (or update) a net class with a track width + clearance (mm) and assign
/// nets to it — "wide copper for power". (Note: also achievable per-track via
/// route_track width.)
pub fn set_net_width(input: Value, ctx: &ToolCtx) -> Result<Value> {
    let name = require_str(&input, "name")?;
    let width = input.get("width").and_then(Value::as_f64).unwrap_or(0.5);
    let clearance = input.get("clearance").and_then(Value::as_f64).unwrap_or(0.2);
    let nets: Vec<String> = input
        .get("nets")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let net_refs: Vec<&str> = nets.iter().map(String::as_str).collect();
    let mut guard = ctx.kicad();
    let Some(session) = guard.as_mut() else {
        return Ok(json!({ "error": "no board open — call open_board first" }));
    };
    match session.kicad().set_net_class(&name, mm_to_nm(width), mm_to_nm(clearance), &net_refs) {
        Ok(()) => Ok(json!({ "ok": true, "net_class": name, "width": width, "nets": nets })),
        Err(e) => Ok(json!({ "error": e.to_string() })),
    }
}

/// Pad numbers the part nets that the chosen footprint does NOT have (sorted, deduped).
fn pads_missing<'a>(pad_keys: impl IntoIterator<Item = &'a str>, fp_pads: &[&str]) -> Vec<String> {
    let mut missing: Vec<String> = pad_keys
        .into_iter()
        .filter(|p| !fp_pads.contains(p))
        .map(String::from)
        .collect();
    missing.sort();
    missing.dedup();
    missing
}

/// Assign a footprint to a part in the draft (fills a missing footprint before placement).
pub fn assign_footprint(input: Value, ctx: &ToolCtx) -> Result<Value> {
    let reference = require_str(&input, "reference")?;
    let footprint = require_str(&input, "footprint")?;
    let Some(mut draft) = BoardDraft::load(ctx) else {
        return Ok(json!({ "error": "no board draft — run derive_board first" }));
    };
    let index = ctx.footprint_index()?;
    let Some(fp) = index.footprint(&footprint) else {
        return Ok(json!({
            "error": format!("unknown footprint `{footprint}`"),
            "suggestions": index.suggest(&footprint),
        }));
    };
    let Some(part) = draft.parts.iter_mut().find(|p| p.reference == reference) else {
        return Ok(json!({ "error": format!("no part `{reference}` on the board") }));
    };
    // The footprint must carry every pad the part nets.
    let fp_pads: Vec<&str> = fp.pads.iter().map(|pp| pp.number.as_str()).collect();
    let missing = pads_missing(part.pad_nets.keys().map(String::as_str), &fp_pads);
    if !missing.is_empty() {
        return Ok(json!({
            "error": format!("footprint `{footprint}` lacks pads the part nets: {missing:?}"),
            "note": "pick a footprint whose pads match the part's pins (search_footprints / get_footprint_info)",
        }));
    }
    part.footprint = footprint.clone();
    draft.save(ctx)?;
    Ok(json!({ "ok": true, "reference": reference, "footprint": footprint }))
}

/// Auto-route the exported board with the Freerouting autorouter (the heavy-duty
/// assist for dense boards the in-house router can't escape). Routes from scratch
/// at the board's design rules, writes the routed copper back to the project
/// `.kicad_pcb`, and reports DRC. Requires an exported (placed) board.
pub fn autoroute(_input: Value, ctx: &ToolCtx) -> Result<Value> {
    let board_path = ctx.pcb_path();
    if !board_path.exists() {
        return Ok(json!({ "error": "no .kicad_pcb — run export_board first (derive_board → place_board → export_board → autoroute)" }));
    }
    let problem = match read_problem(&board_path) {
        Ok(p) => p,
        Err(e) => return Ok(json!({ "error": format!("could not read the board: {e}") })),
    };
    let rules = specctra::RouteRules::from_board(&problem);
    let geo = match specctra::freeroute_with_rules(&board_path, rules) {
        Ok(g) => g,
        Err(e) => return Ok(json!({ "error": format!("freerouting failed: {e}") })),
    };
    let (wires, vias) = (geo.wires.len(), geo.vias.len());
    let solution = geo.to_solution(&problem);
    if let Err(e) = write_solution(&board_path, &solution, &problem) {
        return Ok(json!({ "error": format!("could not write the routed board: {e}") }));
    }
    let _ = specctra::write_net_settings(&board_path, rules);

    // DRC via KiCAD (the external authority); copper faults only.
    let mut copper_violations = None;
    let mut unconnected = None;
    if let Ok(report) = KicadCli::new(ctx.env()).drc(&board_path) {
        copper_violations = Some(report.violations.iter().filter(|v| !is_non_copper(v)).count());
        unconnected = Some(report.unconnected_items.len());
    }
    Ok(json!({
        "ok": true,
        "router": "freerouting",
        "wires": wires,
        "vias": vias,
        "copper_violations": copper_violations,
        "unconnected_items": unconnected,
        "note": "routed with Freerouting and written to the board. open_board to inspect/refine, \
                 or render_board. A few honest unconnected nets on a dense board are acceptable.",
    }))
}

/// Save the live KiCAD board to disk if a session is open. Returns whether it saved.
pub fn save_session_if_open(ctx: &ToolCtx) -> Result<bool> {
    let mut guard = ctx.kicad();
    if let Some(session) = guard.as_mut() {
        session.kicad().save().map_err(ipc_err)?;
        Ok(true)
    } else {
        Ok(false)
    }
}
