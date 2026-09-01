//! The symbol mutators: place, remove, move, retag and swap parts.

use anyhow::Result;
use geom::{EPS, Point2};
use gordian_runtime::AgentRuntime;
use sch_doc::{LabelKind, Pose, SchDoc, placed_pins};
use serde_json::{Value, json};

use crate::place::{Occupancy, Side, snap, snap_point};
use crate::refs;
use crate::session::{Allow, Edit, symbol_source};
use crate::wiring::{spot_beside, spot_near};

/// The next unused designator with this prefix, e.g. `R` → `R7`.
pub(crate) fn next_refdes(doc: &SchDoc, prefix: &str) -> String {
    let taken: Vec<u32> = doc
        .symbols()
        .filter_map(|s| s.refdes().strip_prefix(prefix)?.parse().ok())
        .collect();
    let mut n = 1;
    while taken.contains(&n) {
        n += 1;
    }
    format!("{prefix}{n}")
}

/// Where the caller wants something to sit.
fn destination(
    doc: &SchDoc,
    input: &Value,
    w: f64,
    h: f64,
    skip: &[String],
) -> Result<Point2, String> {
    if let Some(at) = input
        .get("at")
        .or_else(|| input.get("to"))
        .and_then(Value::as_array)
    {
        let n: Vec<f64> = at.iter().filter_map(Value::as_f64).collect();
        if n.len() != 2 {
                return Err("`at`/`to` must be [x, y] in mm".to_string());
        }
        return Ok(snap_point(Point2::new(n[0], n[1])));
    }
    if let Some(anchor) = input.get("near").and_then(Value::as_str) {
        if doc.symbol_by_ref(anchor).is_none() {
            return Err(format!("no symbol `{anchor}` to place near"));
        }
        let side = input
            .get("side")
            .and_then(Value::as_str)
            .and_then(Side::parse)
            .unwrap_or(Side::Right);
        return spot_beside(doc, anchor, side, w, h, skip)
            .ok_or_else(|| format!("no room {side:?} of {anchor}"));
    }
    let content = Occupancy::skipping(doc, skip).content();
    spot_near(
        doc,
        Point2::new(content.max_x + 12.7, content.center().y),
        w,
        h,
        skip,
    )
    .ok_or_else(|| "no free space on the sheet".to_string())
}

/// Place a new part.
pub fn add_symbol(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let Some(lib_id) = input.get("lib_id").and_then(Value::as_str) else {
        return Ok(json!({ "error": "add_symbol needs `lib_id` (e.g. Device:R)" }));
    };
    let mut edit = Edit::open(ctx)?;
    let source = symbol_source(ctx);
    let value = input.get("value").and_then(Value::as_str).unwrap_or("");

    // Park the part far off-sheet first: its body extents are only knowable
    // once its definition is embedded, and the destination depends on them.
    let park = Pose::new(edit.doc.symbols().count() as f64 * 0.0 + 5000.0, 5000.0, 0.0);
    let provisional = next_refdes(&edit.doc, "ZZ");
    if let Err(error) = edit.doc.add_symbol(lib_id, &provisional, value, park, &source) {
        return Ok(json!({ "error": format!("could not place {lib_id}: {error}") }));
    }
    let refdes = match input.get("ref").and_then(Value::as_str) {
        Some(refdes) => refdes.to_string(),
        None => {
            let prefix = edit
                .doc
                .reference_prefix(lib_id)
                .unwrap_or_else(|| "U".to_string());
            next_refdes(&edit.doc, &prefix)
        }
    };
    if edit.doc.symbol_by_ref(&refdes).is_some_and(|s| s.at != park) {
        return Ok(json!({ "error": format!("`{refdes}` is already on the sheet") }));
    }
    edit.doc.set_field(&provisional, "Reference", &refdes)?;
    if let Some(footprint) = input.get("footprint").and_then(Value::as_str) {
        edit.doc.set_field(&refdes, "Footprint", footprint)?;
    }

    let body = edit
        .doc
        .symbol_by_ref(&refdes)
        .and_then(|s| crate::place::extent(&edit.doc, s));
    let (w, h) = body.map_or((10.0, 10.0), |r| (r.width(), r.height()));
    let skip = vec![refdes.clone()];
    let at = match destination(&edit.doc, &input, w, h, &skip) {
        Ok(at) => at,
        Err(error) => return Ok(json!({ "error": error })),
    };
    // `at` is where the body centre should land; move by that offset.
    let centre = body.map_or(park.point(), |r| r.center());
    edit.doc.move_symbol(
        &refdes,
        snap(park.x + at.x - centre.x),
        snap(park.y + at.y - centre.y),
    )?;

    let placed = edit
        .doc
        .symbol_by_ref(&refdes)
        .map(|s| (s.at.x, s.at.y))
        .unwrap_or_default();
    edit.commit(
        json!(format!(
            "placed {refdes} ({lib_id}) at ({:.2},{:.2})",
            placed.0, placed.1
        )),
        Allow::nothing().part(&refdes).creating(),
    )
}

/// Remove parts, together with the stubs and labels that only served them.
pub fn remove_symbols(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let targets: Vec<String> = input
        .get("refs")
        .and_then(Value::as_array)
        .map(|v| v.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    if targets.is_empty() {
        return Ok(json!({ "error": "remove_symbols needs `refs`" }));
    }
    let mut edit = Edit::open(ctx)?;
    let missing: Vec<&String> = targets
        .iter()
        .filter(|r| edit.doc.symbol_by_ref(r).is_none())
        .collect();
    if !missing.is_empty() {
        return Ok(json!({
            "error": format!("not on the sheet: {}",
                missing.iter().map(|r| r.as_str()).collect::<Vec<_>>().join(", ")),
        }));
    }
    let nets = refs::nets_touching(edit.before(), &targets);
    let allow = Allow::nothing()
        .nets(nets)
        .parts(targets.clone());

    let orphaned: Vec<Point2> = placed_pins(&edit.doc)
        .into_iter()
        .filter(|p| targets.contains(&p.refdes))
        .map(|p| p.at)
        .collect();
    for refdes in &targets {
        edit.doc.remove_symbol(refdes)?;
    }
    let retracted = retract_stubs(&mut edit.doc, &orphaned);
    edit.commit(
        json!({
            "removed": targets,
            "retracted_drawing": retracted,
        }),
        allow,
    )
}

/// Drop the wires and labels that hung off pins which no longer exist.
///
/// A wire counts as a stub when one end sat on a removed pin and the other end
/// meets nothing else; removing it can expose another, so this runs to a
/// fixpoint.
fn retract_stubs(doc: &mut SchDoc, orphaned: &[Point2]) -> usize {
    let mut removed = 0;
    for _ in 0..8 {
        let live: Vec<Point2> = placed_pins(doc).into_iter().map(|p| p.at).collect();
        let ends: Vec<Point2> = doc
            .wires()
            .flat_map(|w| [w.points[0], w.points[w.points.len() - 1]])
            .collect();
        let anchored = |p: Point2| {
            live.iter().any(|q| q.near_eq(p, EPS))
                || doc.labels().any(|l| l.at.point().near_eq(p, EPS))
                || ends.iter().filter(|q| q.near_eq(p, EPS)).count() > 1
        };
        let doomed: Vec<String> = doc
            .wires()
            .filter(|wire| {
                let (a, b) = (wire.points[0], wire.points[wire.points.len() - 1]);
                let touches = |p: Point2| orphaned.iter().any(|q| q.near_eq(p, EPS));
                (touches(a) && !anchored(b)) || (touches(b) && !anchored(a))
            })
            .map(|wire| wire.uuid.clone())
            .collect();
        let dangling_labels: Vec<String> = doc
            .labels()
            .filter(|l| {
                orphaned.iter().any(|q| q.near_eq(l.at.point(), EPS))
                    && !ends.iter().any(|q| q.near_eq(l.at.point(), EPS))
            })
            .map(|l| l.uuid.clone())
            .collect();
        let batch: Vec<String> = doomed.into_iter().chain(dangling_labels).collect();
        if batch.is_empty() {
            break;
        }
        removed += doc.remove_drawing(&batch);
    }
    removed
}

/// Move parts, refusing any move that would land one on top of another.
pub fn move_symbols(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let moves: Vec<Value> = input
        .get("moves")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if moves.is_empty() {
        return Ok(json!({ "error": "move_symbols needs `moves`" }));
    }
    let mut edit = Edit::open(ctx)?;
    let all: Vec<String> = moves
        .iter()
        .filter_map(|m| m.get("ref").and_then(Value::as_str).map(str::to_string))
        .collect();
    if all.len() != moves.len() {
        return Ok(json!({ "error": "every move needs a `ref`" }));
    }
    let mut placed = Vec::new();
    for step in &moves {
        let refdes = step["ref"].as_str().unwrap_or_default().to_string();
        let Some(symbol) = edit.doc.symbol_by_ref(&refdes) else {
            return Ok(json!({ "error": format!("no symbol `{refdes}` on the sheet") }));
        };
        let origin = symbol.at.point();
        let body = crate::place::extent(&edit.doc, symbol);
        let (w, h) = body.map_or((10.0, 10.0), |r| (r.width(), r.height()));
        let centre = body.map_or(origin, |r| r.center());
        let target = if let Some(by) = step.get("by").and_then(Value::as_array) {
            let n: Vec<f64> = by.iter().filter_map(Value::as_f64).collect();
            if n.len() != 2 {
                return Ok(json!({ "error": "`by` must be [dx, dy] in mm" }));
            }
            Point2::new(centre.x + n[0], centre.y + n[1])
        } else {
            match destination(&edit.doc, step, w, h, &all) {
                Ok(at) => at,
                Err(error) => return Ok(json!({ "error": error })),
            }
        };
        if !Occupancy::skipping(&edit.doc, &all).free(snap_point(target), w, h) {
            return Ok(json!({
                "error": format!(
                    "{refdes} would overlap another part at ({:.2},{:.2}); nothing was moved",
                    target.x, target.y
                ),
            }));
        }
        edit.doc.move_symbol(
            &refdes,
            snap(origin.x + target.x - centre.x),
            snap(origin.y + target.y - centre.y),
        )?;
        placed.push(json!({ "ref": refdes, "at": [snap(target.x), snap(target.y)] }));
    }
    edit.commit(json!({ "moved": placed }), Allow::nothing())
}

/// Set or clear a part's properties.
pub fn set_fields(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let (Some(refdes), Some(fields)) = (
        input.get("ref").and_then(Value::as_str),
        input.get("fields").and_then(Value::as_object),
    ) else {
        return Ok(json!({ "error": "set_fields needs `ref` and `fields`" }));
    };
    let mut edit = Edit::open(ctx)?;
    if edit.doc.symbol_by_ref(refdes).is_none() {
        return Ok(json!({ "error": format!("no symbol `{refdes}` on the sheet") }));
    }
    let was = refs::nets_touching(edit.before(), &[refdes.to_string()]);
    let renamed = fields.get("Reference").and_then(Value::as_str);
    let mut allow = Allow::nothing()
        .nets(was.clone())
        .part(refdes);
    if let Some(new) = renamed {
        // Auto-named nets carry the designator, so a rename renames them too.
        allow = allow
            .part(new)
            .nets(was.iter().map(|n| n.replace(refdes, new)).collect::<Vec<_>>());
    }
    let mut applied = serde_json::Map::new();
    for (name, value) in fields {
        let text = match value {
            Value::Null => "",
            other => other.as_str().unwrap_or_default(),
        };
        edit.doc.set_field(refdes, name, text)?;
        applied.insert(name.clone(), json!(text));
    }
    edit.commit(json!({ "ref": refdes, "fields": applied }), allow)
}

/// Set a part's build attributes.
pub fn set_flags(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let Some(refdes) = input.get("ref").and_then(Value::as_str) else {
        return Ok(json!({ "error": "set_flags needs `ref`" }));
    };
    let mut edit = Edit::open(ctx)?;
    let uuid = match edit.doc.symbol_by_ref(refdes) {
        Some(symbol) => symbol.uuid.clone(),
        None => return Ok(json!({ "error": format!("no symbol `{refdes}` on the sheet") })),
    };
    let dnp = input.get("dnp").and_then(Value::as_bool);
    let in_bom = input.get("in_bom").and_then(Value::as_bool);
    if dnp.is_none() && in_bom.is_none() {
        return Ok(json!({ "error": "set_flags needs `dnp` and/or `in_bom`" }));
    }
    edit.doc.set_flags(&uuid, dnp, in_bom)?;
    let mut applied = serde_json::Map::new();
    if let Some(dnp) = dnp {
        applied.insert("dnp".into(), json!(dnp));
    }
    if let Some(in_bom) = in_bom {
        applied.insert("in_bom".into(), json!(in_bom));
    }
    edit.commit(
        json!({ "ref": refdes, "flags": applied }),
        Allow::nothing().part(refdes),
    )
}

/// Retarget a part at a different library symbol, keeping its connections.
pub fn swap_symbol(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let (Some(refdes), Some(lib_id)) = (
        input.get("ref").and_then(Value::as_str),
        input.get("lib_id").and_then(Value::as_str),
    ) else {
        return Ok(json!({ "error": "swap_symbol needs `ref` and `lib_id`" }));
    };
    let mut edit = Edit::open(ctx)?;
    if edit.doc.symbol_by_ref(refdes).is_none() {
        return Ok(json!({ "error": format!("no symbol `{refdes}` on the sheet") }));
    }
    let before: Vec<(String, String, String)> = placed_pins(&edit.doc)
        .into_iter()
        .filter(|p| p.refdes == refdes)
        .filter_map(|p| {
            let net = refs::net_of(edit.before(), refdes, &p.number)?;
            Some((p.number.clone(), p.name.clone(), net.to_string()))
        })
        .collect();
    let was = refs::nets_touching(edit.before(), &[refdes.to_string()]);

    let dropped = match edit.doc.set_lib_id(refdes, lib_id, &symbol_source(ctx)) {
        Ok(dropped) => dropped,
        Err(error) => return Ok(json!({ "error": format!("could not swap to {lib_id}: {error}") })),
    };
    for (key, field) in [("value", "Value"), ("footprint", "Footprint")] {
        if let Some(text) = input.get(key).and_then(Value::as_str) {
            edit.doc.set_field(refdes, field, text)?;
        }
    }

    // The new part's pins sit where its own body puts them, which need not be
    // where the old ones were. Re-name any pin whose net the swap dropped;
    // pins that landed back on their net are left exactly as they were.
    let pin_map = input.get("pin_map").and_then(Value::as_object);
    let now = placed_pins(&edit.doc);
    let after = sch_doc::connect::extract(&edit.doc);
    let mut unmapped = Vec::new();
    let mut restored = Vec::new();
    for (number, name, net) in &before {
        let wanted = pin_map
            .and_then(|m| m.get(number).or_else(|| m.get(name)))
            .and_then(Value::as_str);
        let landed = now.iter().find(|p| {
            p.refdes == refdes
                && match wanted {
                    Some(key) => p.number == key || p.name.eq_ignore_ascii_case(key),
                    None => p.number == *number || (!name.is_empty() && p.name == *name),
                }
        });
        let Some(pin) = landed else {
            unmapped.push(match name.as_str() {
                "" | "~" => number.clone(),
                name => format!("{number} ({name})"),
            });
            continue;
        };
        if refs::net_of(&after, refdes, &pin.number) == Some(net.as_str()) {
            continue;
        }
        edit.doc
            .add_label(LabelKind::Local, net, Pose::new(pin.at.x, pin.at.y, 0.0));
        restored.push(format!("{refdes}.{}={net}", pin.number));
    }
    if !restored.is_empty() {
        edit.warn(format!(
            "the new body moved pins, so their nets were re-named in place: {}",
            restored.join(" ")
        ));
    }
    if !unmapped.is_empty() {
        edit.warn(format!(
            "{lib_id} has no counterpart for {refdes} pin(s) {}; their nets were dropped",
            unmapped.join(", ")
        ));
    }
    edit.commit(
        json!({
            "ref": refdes,
            "lib_id": lib_id,
            "dropped_pins": dropped,
            "unmapped_pins": unmapped,
        }),
        Allow::nothing()
            .nets(was.clone())
            .part(refdes)
            .creating(),
    )
}
