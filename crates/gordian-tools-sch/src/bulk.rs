//! Solver-owned bulk creation, arrangement and rewiring over the live schematic.

use anyhow::{Context, Result, anyhow};
use gordian_runtime::AgentRuntime;
use gordian_runtime::config::SchematicPlacementEngine;
use sch_floorplan::contract::PlacementEngine;
use sch_floorplan::live::{ArrangeReport, PlaceReport, Selection};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::session::{Allow, Edit};

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
    let payload: sch_check::PlacePartsInput =
        serde_json::from_value(input).context("invalid place_parts input")?;
    let mut edit = if ctx.sch_path().is_file() {
        Edit::open(ctx)?
    } else {
        Edit::create(ctx, sch_floorplan::live::blank_sheet()?)
    };
    let refs = payload
        .parts
        .iter()
        .map(|part| part.refdes.clone())
        .collect::<Vec<_>>();
    let report = sch_floorplan::live::place_parts(
        ctx.env(),
        &mut edit.doc,
        &payload,
        placement_engine(ctx.config().engines.schematic_placer).as_ref(),
    )?;
    if !report.committed {
        return Ok(refused_place(report));
    }
    let value = edit.commit(json!(report), Allow::nothing().parts(refs).creating())?;
    with_check(value, ctx)
}

pub(crate) fn arrange(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let input: SelectionInput = serde_json::from_value(input).context("invalid arrange input")?;
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
    let input: SelectionInput = serde_json::from_value(input).context("invalid rewire input")?;
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
    with_check(value, ctx)
}

fn refused_place(report: PlaceReport) -> Value {
    json!({
        "error": "refused: the placed result does not match the requested connectivity; nothing was written",
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
    value["check_schematic"] = crate::check::check_schematic(json!({}), ctx)?;
    Ok(value)
}

fn placement_engine(selected: SchematicPlacementEngine) -> Box<dyn PlacementEngine> {
    match selected {
        SchematicPlacementEngine::Anneal => Box::new(anneal_place::Anneal),
        SchematicPlacementEngine::Spine => Box::new(spine_place::SpinePlace),
        SchematicPlacementEngine::Cluster => Box::new(cluster_place::ClusterPlace),
    }
}
