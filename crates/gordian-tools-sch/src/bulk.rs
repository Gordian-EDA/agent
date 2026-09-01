//! Solver-owned bulk creation, arrangement and rewiring over the live schematic.

use anyhow::{Context, Result, anyhow};
use gordian_runtime::AgentRuntime;
use gordian_runtime::config::SchematicPlacementEngine;
use sch_floorplan::contract::PlacementEngine;
use sch_floorplan::live::{ArrangeReport, PlaceReport, Selection};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::session::{Allow, Edit};

/// Deserialize a tool's arguments, naming the field that was wrong.
///
/// Serde's own message says what is malformed but not where; without the path a caller
/// can only resubmit the whole payload and hope.
fn typed<T: serde::de::DeserializeOwned>(input: Value, tool: &str) -> Result<T> {
    serde_path_to_error::deserialize(input).map_err(|e| {
        let path = e.path().to_string();
        anyhow!("invalid {tool} input at `{path}`: {}", e.into_inner())
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectionInput {
    refs: Option<Vec<String>>,
    bbox: Option<[f64; 4]>,
    engine: Option<SchematicPlacementEngine>,
}

pub(crate) fn selection_schema(engine: bool) -> Value {
    let mut properties = json!({
        "refs": {
            "type": "array",
            "items": { "type": "string" },
            "minItems": 1
        },
        "bbox": {
            "type": "array",
            "items": { "type": "number" },
            "minItems": 4,
            "maxItems": 4,
            "description": "Select symbols whose origins lie inside [x1,y1,x2,y2] mm."
        }
    });
    if engine {
        properties["engine"] = json!({
            "type": "string",
            "enum": ["anneal", "spine", "cluster"],
            "description": "Optional placement engine override."
        });
    }
    json!({
        "type": "object",
        "properties": properties,
        "oneOf": [
            { "required": ["refs"] },
            { "required": ["bbox"] }
        ],
        "additionalProperties": false
    })
}

pub(crate) fn place_parts(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let payload: sch_check::PlacePartsInput = typed(input, "place_parts")?;
    // The exact payload is what reproduces a placement; nothing else in the log does.
    tracing::debug!(
        payload = %serde_json::to_string(&payload).unwrap_or_default(),
        "place_parts"
    );
    let mut edit = if ctx.sch_path().is_file() {
        Edit::open(ctx).context("opening the existing schematic")?
    } else {
        Edit::create(ctx, sch_floorplan::live::blank_sheet()?)
    };
    let refs = payload
        .parts
        .iter()
        .map(|part| part.refdes.clone())
        .collect::<Vec<_>>();
    let report = match sch_floorplan::live::place_parts(
        ctx.env(),
        &mut edit.doc,
        &payload,
        placement_engine(ctx.config().engines.schematic_placer).as_ref(),
    ) {
        Ok(report) => report,
        Err(sch_floorplan::live::Error::InvalidPayload(audit)) => {
            return Ok(json!({
                "ok": false,
                "code": "invalid_payload",
                "dangling": audit.dangling,
                "did_you_mean": audit.did_you_mean,
                "unknown_pins": audit.unknown_pins,
            }));
        }
        Err(error) => return Err(error.into()),
    };
    if !report.committed {
        return Ok(refused_place(report));
    }
    let value = edit
        .commit(json!(report), Allow::nothing().parts(refs).creating())
        .context("committing placed parts")?;
    if value.get("error").is_some() {
        return Ok(value);
    }
    with_check(value, ctx).context("checking placed parts")
}

pub(crate) fn arrange(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let input: SelectionInput = typed(input, "arrange")?;
    let selection = selection(&input)?;
    let mut edit = Edit::open(ctx)?;
    let report = sch_floorplan::live::arrange(
        ctx.env(),
        &mut edit.doc,
        &selection,
        placement_engine(
            input
                .engine
                .unwrap_or(ctx.config().engines.schematic_placer),
        )
        .as_ref(),
    )?;
    finish_arrangement(edit, report, ctx)
}

pub(crate) fn rewire(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let input: SelectionInput = typed(input, "rewire")?;
    if input.engine.is_some() {
        return Err(anyhow!("rewire does not accept an engine"));
    }
    let selection = selection(&input)?;
    let mut edit = Edit::open(ctx)?;
    let report = sch_floorplan::live::rewire(ctx.env(), &mut edit.doc, &selection)?;
    finish_arrangement(edit, report, ctx)
}

fn finish_arrangement(edit: Edit, report: ArrangeReport, ctx: &AgentRuntime) -> Result<Value> {
    if !report.committed {
        return Ok(json!({
            "error": "refused: the solver's result changed connectivity; nothing was written",
            "report": report,
        }));
    }
    let value = edit.commit(json!(report), Allow::nothing())?;
    if value.get("error").is_some() {
        return Ok(value);
    }
    with_check(value, ctx)
}

fn refused_place(report: PlaceReport) -> Value {
    let m = &report.mismatch;
    let mut why = Vec::new();
    if !m.shorted.is_empty() {
        let pairs: Vec<String> = m.shorted.iter().map(|(a, b)| format!("{a}+{b}")).collect();
        why.push(format!("shorted {}", pairs.join(", ")));
    }
    if !m.scattered.is_empty() {
        why.push(format!("scattered {}", m.scattered.join(", ")));
    }
    if !m.disturbed.is_empty() {
        why.push(format!("disturbed existing {}", m.disturbed.join(", ")));
    }
    json!({
        "error": format!(
            "refused: the placed result does not match the requested connectivity ({}); nothing was written. This is a placement-engine failure, not a payload error — retrying the same payload will not help; report it and try `engine: \"anneal\"` or a smaller block",
            why.join("; ")
        ),
        "report": report,
    })
}

fn selection(input: &SelectionInput) -> Result<Selection> {
    match (&input.refs, input.bbox) {
        (Some(refs), None) if !refs.is_empty() => Ok(Selection::Refs(refs.clone())),
        (None, Some(bbox)) => Ok(Selection::Bbox(bbox)),
        _ => Err(anyhow!(
            "provide exactly one non-empty `refs` or `bbox` selection"
        )),
    }
}

fn with_check(mut value: Value, ctx: &AgentRuntime) -> Result<Value> {
    let check = crate::check::check_schematic(json!({}), ctx)?;
    value["gaps"] = check
        .pointer("/completeness/gaps")
        .cloned()
        .unwrap_or_else(|| json!([]));
    value["check_schematic"] = check;
    Ok(value)
}

fn placement_engine(selected: SchematicPlacementEngine) -> Box<dyn PlacementEngine> {
    match selected {
        SchematicPlacementEngine::Anneal => Box::new(anneal_place::Anneal),
        SchematicPlacementEngine::Spine => Box::new(spine_place::SpinePlace),
        SchematicPlacementEngine::Cluster => Box::new(cluster_place::ClusterPlace),
    }
}
