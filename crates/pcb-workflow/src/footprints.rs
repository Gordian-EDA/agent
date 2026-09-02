//! Footprint discovery tools: `search_footprints` and `get_footprint_info`.

use serde_json::{Value, json};

use kicad_footprint::{FootprintId, SearchQuery, unknown_footprint_message};

use gordian_runtime::AgentRuntime;
use gordian_runtime::tool::{require_search_query, require_str};

pub fn search_footprints(input: Value, ctx: &AgentRuntime) -> anyhow::Result<Value> {
    if let Some(queries) = input.get("queries") {
        let queries = queries
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("`queries` must be an array"))?;
        if queries.is_empty() || queries.len() > 4 {
            anyhow::bail!("`queries` must contain 1 to 4 searches");
        }
        let mut results = Vec::with_capacity(queries.len());
        for item in queries {
            let query = require_search_query(item)?;
            let limit = footprint_search_limit(item, ctx);
            let mut result = search_footprints_one(&query, limit, ctx)?;
            result["query"] = json!(query);
            results.push(result);
        }
        return Ok(json!({ "results": results }));
    }
    let query = require_search_query(&input)?;
    search_footprints_one(&query, footprint_search_limit(&input, ctx), ctx)
}

fn footprint_search_limit(input: &Value, ctx: &AgentRuntime) -> usize {
    input
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(ctx.config().tools.default_search_limit)
}

fn search_footprints_one(query: &str, limit: usize, ctx: &AgentRuntime) -> anyhow::Result<Value> {
    let normalized = query.to_ascii_lowercase();
    let (search_query, note) = if normalized.contains("rp2040") {
        (
            "QFN-56 7x7 0.4".to_string(),
            Some(
                "RP2040 uses a 56-pin QFN package in KiCad libraries; do not substitute a BGA footprint.",
            ),
        )
    } else {
        (query.to_owned(), None)
    };

    let hits: Vec<Value> = ctx
        .footprint_catalog()?
        .search(SearchQuery::new(search_query).limit(limit))
        .into_iter()
        .map(|h| json!({ "lib_id": h.id.to_string(), "pad_count": h.pad_count }))
        .collect();

    let mut out = json!({ "hits": hits });
    if let Some(note) = note {
        out["note"] = json!(note);
    }
    Ok(out)
}

pub fn get_footprint_info(input: Value, ctx: &AgentRuntime) -> anyhow::Result<Value> {
    let lib_id = require_str(&input, "lib_id")?;
    let catalog = ctx.footprint_catalog()?;

    let id = match FootprintId::parse(&lib_id) {
        Ok(id) => id,
        Err(_) => {
            let suggestions = catalog.suggest(&lib_id);
            return Ok(json!({
                "error": unknown_footprint_message(&lib_id, &suggestions),
                "suggestions": suggestion_strings(suggestions),
            }));
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
        Err(e) if e.is_not_found() => {
            let suggestions = catalog.suggest(&lib_id);
            Ok(json!({
                "error": unknown_footprint_message(&lib_id, &suggestions),
                "suggestions": suggestion_strings(suggestions),
            }))
        }
        Err(e) => Ok(json!({
            "error": format!("footprint `{lib_id}` could not be read: {e}"),
        })),
    }
}

fn suggestion_strings(suggestions: Vec<FootprintId>) -> Vec<String> {
    suggestions.iter().map(ToString::to_string).collect()
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
