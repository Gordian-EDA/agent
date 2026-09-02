//! Solver-owned bulk creation, arrangement and rewiring over the live schematic.

use anyhow::{Context, Result, anyhow};
use gordian_runtime::AgentRuntime;
use gordian_runtime::config::PlacementEngineKind;
use sch_model::engine::PlacementEngine;
use sch_floorplan::live::{ArrangeReport, PlaceReport, PlacementBudget, Selection};
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
    engine: Option<PlacementEngineKind>,
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
    let derived: Vec<String> = payload
        .parts
        .iter()
        .flat_map(|part| part.pins.values())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .filter_map(|net| crate::refs::derived_name_refusal(edit.before(), net))
        .collect();
    if !derived.is_empty() {
        return Ok(json!({ "ok": false, "code": "derived_net_name", "nets": derived }));
    }
    let (budget, engine) = budgeted(ctx, edit.doc.symbols().count() + payload.parts.len(), None);
    let timing = Timing::start("place_parts", &budget, engine.name());
    let report = match sch_floorplan::live::place_parts(
        ctx.env(),
        &mut edit.doc,
        &payload,
        engine.as_ref(),
        Some(budget),
    ) {
        Ok(report) => report,
        Err(error @ sch_floorplan::live::Error::Budget { .. }) => {
            timing.done("overran");
            return Ok(json!({ "error": error.to_string() }));
        }
        Err(sch_floorplan::live::Error::InvalidPayload(audit)) => {
            return Ok(json!({
                "ok": false,
                "code": "invalid_payload",
                "dangling": audit.dangling,
                "duplicate_refs": audit.duplicate_refs,
                "did_you_mean": audit.did_you_mean,
                "unknown_pins": audit.unknown_pins,
                "note": "each dangling pin names a net that would carry no second pin. \
                         `on_sheet: false` means the sheet has no such net — name a net the \
                         payload or the sheet already carries, or declare the pin that joins it \
                         in this same call. Re-read the schematic before retrying if an earlier \
                         edit emptied the net.",
            }));
        }
        Err(error) => return Err(error.into()),
    };
    timing.done(if report.committed {
        "committed"
    } else {
        "refused"
    });
    if !report.committed {
        return Ok(refused_place(report));
    }
    let refs = report.placed.clone();
    let value = edit
        .commit(
            ctx,
            "place_parts",
            "Place schematic parts",
            json!(report),
            Allow::nothing().parts(refs).creating(),
        )
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
    let (budget, engine) = budgeted(ctx, edit.doc.symbols().count(), input.engine);
    let timing = Timing::start("arrange", &budget, engine.name());
    let report = match sch_floorplan::live::arrange(
        ctx.env(),
        &mut edit.doc,
        &selection,
        engine.as_ref(),
        Some(budget),
    ) {
        Ok(report) => report,
        Err(error @ sch_floorplan::live::Error::Budget { .. }) => {
            timing.done("overran");
            return Ok(json!({ "error": error.to_string() }));
        }
        Err(error) => return Err(error.into()),
    };
    timing.done(if report.committed {
        "committed"
    } else {
        "refused"
    });
    finish_arrangement(edit, report, ctx, "arrange")
}

pub(crate) fn rewire(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let input: SelectionInput = typed(input, "rewire")?;
    if input.engine.is_some() {
        return Err(anyhow!("rewire does not accept an engine"));
    }
    let selection = selection(&input)?;
    let mut edit = Edit::open(ctx)?;
    let report = sch_floorplan::live::rewire(ctx.env(), &mut edit.doc, &selection)?;
    finish_arrangement(edit, report, ctx, "rewire")
}

fn finish_arrangement(
    edit: Edit,
    report: ArrangeReport,
    ctx: &AgentRuntime,
    tool: &str,
) -> Result<Value> {
    if !report.committed {
        return Ok(json!({
            "error": "refused: the solver's result changed connectivity; nothing was written",
            "report": report,
        }));
    }
    let summary = if tool == "arrange" {
        "Arrange schematic symbols"
    } else {
        "Rewire schematic symbols"
    };
    let value = edit.commit(ctx, tool, summary, json!(report), Allow::nothing())?;
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

fn placement_engine(selected: PlacementEngineKind) -> Box<dyn PlacementEngine> {
    match selected {
        PlacementEngineKind::Anneal => Box::new(anneal_place::Anneal),
        PlacementEngineKind::Spine => Box::new(spine_place::SpinePlace),
        PlacementEngineKind::Cluster => Box::new(cluster_place::ClusterPlace),
    }
}

/// One placement call's wall time, logged when it ends — the record that says
/// whether the deadline policy is holding on real designs.
struct Timing {
    tool: &'static str,
    engine: &'static str,
    budget_secs: u64,
    parts: usize,
    started: std::time::Instant,
}

impl Timing {
    fn start(tool: &'static str, budget: &PlacementBudget, engine: &'static str) -> Self {
        Self {
            tool,
            engine,
            budget_secs: budget.budget.as_secs(),
            parts: budget.parts,
            started: std::time::Instant::now(),
        }
    }

    fn done(self, outcome: &str) {
        tracing::info!(
            tool = self.tool,
            engine = self.engine,
            parts = self.parts,
            budget_s = self.budget_secs,
            elapsed_s = self.started.elapsed().as_secs_f64(),
            outcome,
            "placement finished"
        );
    }
}

/// The deadline policy for a call that leaves `parts` on the sheet, with the engine
/// it chose — honouring an explicit `engine` on the call, then the configured
/// override. The engine routes the WHOLE sheet per candidate, so the sheet's size is
/// what the budget must be read against, never the size of the block being placed.
fn budgeted(
    ctx: &AgentRuntime,
    parts: usize,
    engine: Option<PlacementEngineKind>,
) -> (PlacementBudget, Box<dyn PlacementEngine>) {
    let budget = PlacementBudget::new(parts);
    let engine = budget.engine(engine.or(ctx.config().engines.schematic_placer));
    (budget, placement_engine(engine))
}
