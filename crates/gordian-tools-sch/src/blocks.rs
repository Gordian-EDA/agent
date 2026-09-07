//! Blocks as tools: `create_and_update_block` outlines and titles a set of placed
//! parts; `arrange_blocks` lays the outlined blocks out as a grid.

use anyhow::Result;
use gordian_runtime::AgentRuntime;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::session::{Allow, Edit};

fn typed<T: serde::de::DeserializeOwned>(input: Value, tool: &str) -> Result<T> {
    serde_path_to_error::deserialize(input).map_err(|e| {
        let path = e.path().to_string();
        anyhow::anyhow!("invalid {tool} input at `{path}`: {}", e.into_inner())
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateBlockInput {
    parts: Vec<String>,
    title: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ArrangeBlocksInput {
    rows: Vec<Vec<String>>,
}

pub(crate) fn create_block_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "parts": { "type": "array", "items": { "type": "string" }, "minItems": 1,
                       "description": "Reference designators of every part in the block." },
            "title": { "type": "string", "description": "The block's name, written inside its outline." }
        },
        "required": ["parts", "title"],
        "additionalProperties": false
    })
}

pub(crate) fn arrange_blocks_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "rows": { "type": "array", "minItems": 1,
                      "items": { "type": "array", "items": { "type": "string" }, "minItems": 1 },
                      "description": "Block titles, top row first, each row left to right." }
        },
        "required": ["rows"],
        "additionalProperties": false
    })
}

/// Re-tile the sheet to the rows it was last tiled in, once it has been; a block
/// the rows do not know joins the last row. `None` when the sheet was never tiled.
pub(crate) fn retile(doc: &mut sch_doc::SchDoc, ctx: &AgentRuntime, newcomer: Option<&str>) -> Option<Value> {
    let mut rows = ctx.workspace().block_rows();
    if rows.is_empty() {
        return None;
    }
    if let Some(name) = newcomer
        && !rows.iter().flatten().any(|n| n == name)
    {
        rows.last_mut().expect("rows are not empty").push(name.to_string());
    }
    Some(match sch_floorplan::blocks::arrange_blocks(doc, &rows) {
        Ok(report) => {
            let _ = ctx.workspace().set_block_rows(&rows);
            json!({"rows": rows, "set_aside": report.set_aside})
        }
        Err(error) => json!({"error": error.to_string()}),
    })
}

/// Outline and title a set of parts as one block; a title the sheet already has is
/// redefined with these parts.
pub fn create_and_update_block(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let input: CreateBlockInput = typed(input, "create_and_update_block")?;
    let mut edit = Edit::open(ctx)?;
    let report = match sch_floorplan::blocks::create_block(
        &mut edit.doc,
        &input.title,
        &input.parts,
        Some(&input.title),
    ) {
        Ok(report) => report,
        Err(error) => return Ok(json!({ "error": error.to_string() })),
    };
    let retiled = retile(&mut edit.doc, ctx, Some(&report.name));
    edit.commit(
        json!({
            "block": report.name,
            "ignored": report.ignored,
            "parts": report.parts,
            "frame": [report.frame.min_x, report.frame.min_y, report.frame.max_x, report.frame.max_y],
            "retiled": retiled,
        }),
        Allow::nothing(),
    )
}

/// Lay the outlined blocks out as a grid of rows.
pub fn arrange_blocks(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let input: ArrangeBlocksInput = typed(input, "arrange_blocks")?;
    let mut edit = Edit::open(ctx)?;
    let report = match sch_floorplan::blocks::arrange_blocks(&mut edit.doc, &input.rows) {
        Ok(report) => report,
        Err(error) => return Ok(json!({ "error": error.to_string() })),
    };
    let _ = ctx.workspace().set_block_rows(&input.rows);
    edit.commit(
        json!({
            "moved": report.moved,
            "set_aside": report.set_aside,
            "page": report.page,
        }),
        Allow::nothing(),
    )
}
