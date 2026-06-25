//! Footprint discovery + assignment tools: `search_footprints`,
//! `get_footprint_info`, and `assign_footprint` (fill a part's missing footprint
//! before placement).

use serde_json::{Value, json};

use geom::Point2;

use crate::tools::{PcbToolCtx, require_str};

/// Default number of footprint-search hits returned when `limit` is omitted.
/// Mirrors `tools::DEFAULT_SEARCH_LIMIT` for the symbol side.
const DEFAULT_SEARCH_LIMIT: usize = 8;

pub fn search_footprints(input: Value, ctx: &PcbToolCtx) -> anyhow::Result<Value> {
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

pub fn get_footprint_info(input: Value, ctx: &PcbToolCtx) -> anyhow::Result<Value> {
    let lib_id = require_str(&input, "lib_id")?;
    let index = ctx.footprint_index()?;

    match index.footprint(&lib_id) {
        Some(fp) => {
            // The model binds nets by pad NUMBER and never needs each pad's coordinates (it
            // doesn't place pads), so return the number list + a compact shape SUMMARY rather
            // than the full per-pad table — for a 256-ball BGA the old table was ~20k chars
            // re-sent every turn. min_pitch + pad_size let the model judge fine-pitch (pick a
            // clearance/via); technologies/layers tell it SMD vs thru-hole.
            let pad_numbers: Vec<&str> = fp
                .pads
                .iter()
                .map(|p| p.number.as_str())
                .filter(|n| !n.is_empty())
                .collect();
            let mut min_pitch = f64::INFINITY;
            for (i, a) in fp.pads.iter().enumerate() {
                for b in &fp.pads[i + 1..] {
                    let d = Point2::new(a.at[0], a.at[1]).dist(Point2::new(b.at[0], b.at[1]));
                    if d > geom::EPS && d < min_pitch {
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
            let techs: std::collections::BTreeSet<&str> = fp
                .pads
                .iter()
                .map(|p| technology_str(p.technology))
                .collect();
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

/// Render a [`kicad_footprint::PadTechnology`] as a stable lowercase
/// string for the LLM.
fn technology_str(t: kicad_footprint::PadTechnology) -> &'static str {
    use kicad_footprint::PadTechnology::*;
    match t {
        Smd => "smd",
        ThruHole => "thru_hole",
        NpThruHole => "np_thru_hole",
        Other => "other",
    }
}

fn courtyard_source_str(s: kicad_footprint::CourtyardSource) -> &'static str {
    use kicad_footprint::CourtyardSource::*;
    match s {
        Crtyd => "crtyd",
        PadSilkFallback => "pad_silk_fallback",
    }
}

fn bbox_json(b: &geom::Rect) -> Value {
    json!({
        "min_x": b.min_x,
        "min_y": b.min_y,
        "max_x": b.max_x,
        "max_y": b.max_y,
        "width": b.width(),
        "height": b.height(),
    })
}

/// Return the circuit-YAML edit needed to assign a footprint.
///
/// Footprint assignment belongs to the schematic/circuit YAML, not PCB state. This helper is
/// deliberately stateless: it does not read board state, inspect the schematic, or write files.
pub fn assign_footprint(input: Value, _ctx: &PcbToolCtx) -> anyhow::Result<Value> {
    let reference = require_str(&input, "reference")?;
    let footprint = require_str(&input, "footprint")?;
    Ok(json!({
        "ok": true,
        "reference": reference,
        "footprint": footprint,
        "edit_design": {
            "instruction": format!(
                "Set the circuit YAML component `{reference}` footprint field to `{footprint}`. \
                 Do not edit PCB files directly; after apply_design, run derive_board to sync the PCB."
            ),
            "yaml_field": "footprint",
            "value": footprint,
        }
    }))
}
