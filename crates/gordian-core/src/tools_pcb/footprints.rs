//! Footprint discovery + assignment tools: `search_footprints`,
//! `get_footprint_info`, and `assign_footprint` (fill a part's missing footprint
//! before placement).

use serde_json::{Value, json};

use kicad_footprint::{FootprintId, SearchQuery};

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
        .footprint_catalog()?
        .search(SearchQuery::new(query).limit(limit))
        .into_iter()
        .map(|h| json!({ "lib_id": h.id.to_string(), "pad_count": h.pad_count }))
        .collect();

    Ok(json!({ "hits": hits }))
}

pub fn get_footprint_info(input: Value, ctx: &PcbToolCtx) -> anyhow::Result<Value> {
    let lib_id = require_str(&input, "lib_id")?;
    let catalog = ctx.footprint_catalog()?;

    let id = match FootprintId::parse(&lib_id) {
        Ok(id) => id,
        Err(_) => {
            return Ok(json!({ "error": format!("invalid footprint id `{lib_id}`") }));
        }
    };

    match catalog.footprint(&id) {
        Ok(fp) => {
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
                    let d = a.at.dist(b.at);
                    if d > geom::EPS && d < min_pitch {
                        min_pitch = d;
                    }
                }
            }
            let (mut wmin, mut wmax) = (f64::INFINITY, 0.0_f64);
            for p in &fp.pads {
                let s = p.size.x.min(p.size.y);
                wmin = wmin.min(s);
                wmax = wmax.max(p.size.x.max(p.size.y));
            }
            let techs: std::collections::BTreeSet<&str> =
                fp.pads.iter().map(|p| p.technology.as_str()).collect();
            Ok(json!({
                "lib_id": id.to_string(),
                "name": fp.name,
                "descr": fp.descr,
                "pad_count": fp.pads.len(),
                "pad_numbers": pad_numbers,
                "min_pitch_mm": if min_pitch.is_finite() { (min_pitch * 1000.0).round() / 1000.0 } else { 0.0 },
                "pad_min_dim_mm": if wmin.is_finite() { (wmin * 1000.0).round() / 1000.0 } else { 0.0 },
                "pad_max_dim_mm": (wmax * 1000.0).round() / 1000.0,
                "technologies": techs,
                "courtyard": bbox_json(&fp.courtyard),
                "courtyard_source": fp.courtyard_source.as_str(),
                "bbox": bbox_json(&fp.bounds),
            }))
        }
        Err(e) if e.is_not_found() => Ok(json!({
            "error": format!("unknown footprint `{lib_id}`"),
            "suggestions": catalog.suggest(&id).iter().map(|i| i.to_string()).collect::<Vec<_>>(),
        })),
        Err(e) => Ok(json!({
            "error": format!("footprint `{lib_id}` could not be read: {e}"),
        })),
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
