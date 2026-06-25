//! Board-construction tools and the input parsers they share: `derive_board`
//! (seed the draft from the committed schematic) and `build_board_draft` (the
//! internal builder the deterministic test harnesses call). Footprint
//! resolution, design-rule / bounds / keepout / group parsing, and validation
//! all live here.

use std::collections::BTreeMap;

use anyhow::Result;
use serde_json::{Value, json};

use circuit_lang::model::PinTarget;
use sch_io::read::lift;

use pcb_synth::placefp::part_from_footprint;
use pcb_place::placement::{Edge, GroupHint, LockedAt, PlacementHints, Rect};
use pcb_model::{LayerRef, Point2};

use crate::tools::PcbToolCtx;

use super::draft::{BoardDraft, DraftPart, DraftRules, Keepout, PourSpec};
use super::export::resolve_pour_layer;

// ── derive_board ──────────────────────────────────────────────────────────────

/// `derive_board` — build the board draft from the committed schematic + the
/// footprint map, instead of re-typing parts by hand. Connectivity comes from the
/// schematic's netlist (via `lift`, which keys pins by pad number — KiCAD does the
/// pin→pad resolution for us), footprints from `.gordian/footprints.json`. The
/// caller supplies only `bounds` and `rules`; parts and nets come from the
/// schematic. See `docs/specs/schematic-driven-pcb.md`.
pub fn derive_board(input: Value, ctx: &PcbToolCtx) -> Result<Value> {
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
        Rect { min_x: 0.0, min_y: 0.0, max_x: 50.0, max_y: 40.0 }
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
pub(super) fn req_num(obj: &Value, key: &str, ctx: &str) -> std::result::Result<f64, String> {
    obj.get(key)
        .and_then(Value::as_f64)
        .ok_or_else(|| format!("{ctx}: missing or non-numeric `{key}`"))
}

/// Parse the board `bounds` from snake_case model input into the engine's
/// [`Rect`] (whose serde is camelCase, so we read fields explicitly rather than
/// deserializing directly — the tool API stays snake_case like the others).
fn parse_bounds(v: Option<&Value>) -> std::result::Result<Rect, String> {
    let Some(obj) = v else {
        return Err("missing required `bounds` ({min_x, max_x, min_y, max_y} in mm)".into());
    };
    Ok(Rect {
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
                    let raw = l.get("rotation").and_then(Value::as_f64).unwrap_or(0.0);
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
pub fn build_board_draft(input: Value, ctx: &PcbToolCtx) -> Result<Value> {
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
        Some(o) => Rect {
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
pub(super) fn net_pin_counts(parts: &[DraftPart], ctx: &PcbToolCtx) -> BTreeMap<String, usize> {
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

// ── placement-hint helpers (group parsing for the DSL) ───────────────────────

/// Validate a part rotation is axis-aligned (0/90/180/270), normalizing to
/// `[0,360)`. The placer + synth support only these; rejecting others HERE (at the
/// agent surface) fails fast with a clear message, instead of routing to wrong pad
/// positions (`rotate_offset` is identity for non-axis angles) and failing late at
/// export.
fn axis_aligned_rotation(rot: f64) -> std::result::Result<f64, String> {
    let r = rot.rem_euclid(360.0);
    let snapped = geom::snap_quadrant(r);
    if (snapped - r).abs() <= geom::EPS {
        Ok(snapped)
    } else {
        Err(format!(
            "rotation {rot}° is not supported — use 0, 90, 180, or 270 \
             (the engine places axis-aligned parts only)"
        ))
    }
}

/// Parse one group hint from snake_case model input, validating its members
/// against the draft's known references. Returns the engine [`GroupHint`] on
/// success or a model-readable error string.
pub(super) fn parse_group_hint(v: &Value, known_refs: &[&str]) -> std::result::Result<GroupHint, String> {
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
fn parse_edge(v: &Value) -> std::result::Result<Edge, String> {
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
pub(super) fn parse_keepout(
    v: &Value,
    bounds: &Rect,
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
