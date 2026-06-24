//! Footprint discovery + assignment tools: `search_footprints`,
//! `get_footprint_info`, and `assign_footprint` (fill a part's missing footprint
//! before placement).

use serde_json::{Value, json};

use crate::tools::{PcbToolCtx, require_str};

use super::draft::BoardDraft;

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
pub fn assign_footprint(input: Value, ctx: &PcbToolCtx) -> anyhow::Result<Value> {
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
