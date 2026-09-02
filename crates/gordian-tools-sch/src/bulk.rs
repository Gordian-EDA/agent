//! Solver-owned bulk creation, arrangement and rewiring over the live schematic.

use anyhow::{Context, Result, anyhow};
use gordian_runtime::AgentRuntime;
use gordian_runtime::config::PlacementEngineKind;
use sch_floorplan::live::{ArrangeReport, PlaceReport, PlacementBudget, Selection};
use sch_model::engine::PlacementEngine;
use serde::Deserialize;
use serde_json::{Value, json};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Duration;

use crate::session::{Allow, Edit, attach_connectivity};

/// Every accepted shape of an `intent.relations` entry, with an example of each.
///
/// Serde can only report the first malformed field and says nothing about what it
/// wanted, so a caller who mis-shapes a relation has to guess. Relations are the
/// field that is actually guessed wrong, so the refusal carries the whole grammar.
const RELATION_SHAPES: &str = "each entry is an object tagged by `kind`: \
     {\"kind\":\"left_of\",\"a\":\"R1\",\"b\":\"U1\"} (also right_of, above, below); \
     {\"kind\":\"group\",\"name\":\"leds\",\"members\":[\"R3\",\"D1\"],\"side\":\"right\",\"anchor\":\"U1\"} \
     (`side` alone is fine; [\"right\",\"U1\"] and {\"side\":\"right\",\"anchor\":\"U1\"} also parse); \
     {\"kind\":\"align\",\"members\":[\"C1\",\"C2\"],\"axis\":\"horizontal\"}";

/// Deserialize a tool's arguments, naming the field that was wrong.
///
/// Serde's own message says what is malformed but not where; without the path a caller
/// can only resubmit the whole payload and hope.
fn typed<T: serde::de::DeserializeOwned>(input: Value, tool: &str) -> Result<T> {
    serde_path_to_error::deserialize(input).map_err(|e| {
        let path = e.path().to_string();
        let help = if path.contains("relations") {
            format!(" — {RELATION_SHAPES}")
        } else {
            String::new()
        };
        anyhow!("invalid {tool} input at `{path}`: {}{help}", e.into_inner())
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
    let mut payload: sch_check::PlacePartsInput = typed(input, "place_parts")?;
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
    let minted = match resolve_pin_net_refs(&mut payload, &mut edit)? {
        Ok(minted) => minted,
        Err(error) => return Ok(error),
    };
    let existing_netlist = sch_doc::connect::extract(&edit.doc);
    let existing = sch_check::ExistingSheet {
        net_pins: existing_netlist
            .nets
            .iter()
            .map(|net| (net.name.clone(), net.pins.len()))
            .collect(),
        refs: edit
            .doc
            .symbols()
            .map(|symbol| symbol.refdes().to_string())
            .collect(),
    };
    let (design, _, _) = sch_check::into_design(&payload, ctx.provider(), &existing);
    let footprint_mismatch =
        gordian_runtime::footprint_compat::design_pin_mismatches(ctx, &design)?
            .iter()
            .map(gordian_runtime::footprint_compat::FootprintPinMismatch::payload)
            .collect::<Vec<_>>();
    if !footprint_mismatch.is_empty() {
        return Ok(json!({
            "ok": false,
            "code": "invalid_payload",
            "footprint_mismatch": footprint_mismatch,
            "note": "symbol/footprint compatibility is checked before placement; use each compatible suggestion directly",
        }));
    }
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
    let requested = match payload.engine.as_deref().map(engine_named) {
        Some(Some(kind)) => Some(kind),
        Some(None) => {
            return Ok(json!({
                "error": format!(
                    "unknown engine `{}`; use \"anneal\", \"spine\" or \"cluster\"",
                    payload.engine.clone().unwrap_or_default()
                ),
            }));
        }
        None => None,
    };
    let sheet_parts = edit.doc.symbols().count() + payload.parts.len();
    // A mismatch restores the document, so trying another engine costs only time.
    // The engines fail on different sheets, and the model has no way to tell which
    // will work — leaving it to guess turned one campaign case into a dead end.
    let mut tried = Vec::new();
    let mut skipped = Vec::new();
    let mut report = None;
    // The last attempt's timing, reported on the result whether it committed or not.
    let mut placement = json!(null);
    // The ladder shares ONE budget: a second engine only gets what the first left,
    // so three attempts can never stack past the tool's timeout.
    let ladder_started = std::time::Instant::now();
    let policy = PlacementBudget::new(sheet_parts);
    let ladder_budget = policy.budget;
    for kind in engines_to_try(requested) {
        let remaining = ladder_budget.saturating_sub(ladder_started.elapsed());
        if requested.is_none() && !policy.engine_fits(kind, remaining) {
            let name = engine_kind_name(kind);
            tracing::info!(
                engine = name,
                parts = sheet_parts,
                remaining_ms = remaining.as_millis().min(u128::from(u64::MAX)) as u64,
                "skipping placement engine that cannot finish in the remaining budget"
            );
            skipped.push(name);
            continue;
        }
        let (budget, engine) = budgeted_within(ctx, remaining, sheet_parts, Some(kind));
        let engine_name = engine.name();
        let timing = Timing::start("place_parts", &budget, engine_name);
        let attempt =
            guarded_place_parts(ctx.env(), &mut edit.doc, &payload, engine, Some(budget))?;
        let GuardedPlacement::Completed(result) = attempt else {
            timing.done("panicked");
            tried.push(engine_name);
            continue;
        };
        match *result {
            Ok(placed) => {
                let elapsed_ms = timing.done(if placed.committed {
                    "committed"
                } else {
                    "refused"
                });
                placement = json!({
                    "engine": timing.engine,
                    "parts": timing.parts,
                    "budget_ms": timing.budget_ms,
                    "elapsed_ms": elapsed_ms,
                });
                tried.push(engine_name);
                let committed = placed.committed;
                report = Some(placed);
                if committed {
                    break;
                }
            }
            Err(error @ sch_floorplan::live::Error::Budget { .. }) => {
                timing.done("overran");
                return Ok(budget_refusal(&error));
            }
            Err(sch_floorplan::live::Error::InvalidPayload(audit)) => {
                return Ok(json!({
                    "ok": false,
                    "code": "invalid_payload",
                    "input_errors": audit.input_errors,
                    "duplicate_refs": audit.duplicate_refs,
                    "unknown_pins": audit.unknown_pins,
                    "footprint_mismatch": audit.footprint_mismatch,
                    "dangling": audit.dangling,
                    "did_you_mean": audit.did_you_mean,
                    "unreliable_nets": audit.unreliable_nets,
                    "note": "this lists EVERY fault in the payload — fix them all before retrying. \
                             `place_parts` appends to the sheet, so resubmit only the parts named \
                             here, not the whole payload. `input_errors` are unresolvable lib_ids \
                             and pin conflicts; `duplicate_refs` give the next free refdes; \
                             `unknown_pins` name a key the symbol does not have. `dangling` pins are \
                             NOT fatal on their own — they are listed so you can finish them.",
                }));
            }
            Err(error) => return Err(error.into()),
        }
    }
    let report = match report {
        Some(report) => report,
        None => {
            return Ok(json!({
                "error": "no placement engine completed",
                "engines_tried": tried,
                "engines_skipped": skipped,
            }));
        }
    };
    if !report.committed {
        return Ok(refused_place(
            report,
            &payload,
            &tried,
            &skipped,
            sheet_parts,
        ));
    }
    let refs = report.placed.clone();
    let mut value = edit
        .commit(
            ctx,
            "place_parts",
            "Place schematic parts",
            json!(report),
            // Naming a previously auto-named net renames it; that is the point of an
            // `@ref.pin` target, so the guard is told rather than surprised.
            Allow::nothing().parts(refs).joining_nets(minted).creating(),
        )
        .context("committing placed parts")?;
    if value.get("error").is_some() {
        return Ok(value);
    }
    value["placement"] = placement;
    value["engines_tried"] = json!(tried);
    value["engines_skipped"] = json!(skipped);
    let refs = report.placed.join(" ");
    attach_connectivity(&mut value, ctx, report.placed, &format!("PLACED  {refs}"))?;
    with_check(value, ctx).context("checking placed parts")
}

enum GuardedPlacement {
    Completed(Box<sch_floorplan::live::Result<PlaceReport>>),
    Panicked,
}

fn guarded_place_parts(
    env: &kicad::KicadInstallation,
    doc: &mut sch_doc::SchDoc,
    payload: &sch_check::PlacePartsInput,
    engine: Box<dyn PlacementEngine>,
    budget: Option<PlacementBudget>,
) -> Result<GuardedPlacement> {
    let snapshot = doc.snapshot();
    let engine_name = engine.name();
    match catch_unwind(AssertUnwindSafe(|| {
        sch_floorplan::live::place_parts(env, doc, payload, engine, budget)
    })) {
        Ok(result) => Ok(GuardedPlacement::Completed(Box::new(result))),
        Err(panic) => {
            let message = panic
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("non-string panic payload");
            tracing::error!(
                engine = engine_name,
                panic = message,
                "placement engine panicked"
            );
            doc.restore(snapshot)?;
            Ok(GuardedPlacement::Panicked)
        }
    }
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
        engine,
        Some(budget),
    ) {
        Ok(report) => report,
        Err(error @ sch_floorplan::live::Error::Budget { .. }) => {
            timing.done("overran");
            return Ok(budget_refusal(&error));
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
    let budget = PlacementBudget::new(edit.doc.symbols().count());
    let timing = Timing::start("rewire", &budget, "rewire");
    let report =
        match sch_floorplan::live::rewire(ctx.env(), &mut edit.doc, &selection, Some(budget)) {
            Ok(report) => report,
            Err(error @ sch_floorplan::live::Error::Budget { .. }) => {
                timing.done("overran");
                return Ok(budget_refusal(&error));
            }
            Err(error) => return Err(error.into()),
        };
    timing.done(if report.committed {
        "committed"
    } else {
        "refused"
    });
    finish_arrangement(edit, report, ctx, "rewire")
}

fn budget_refusal(error: &sch_floorplan::live::Error) -> Value {
    let sch_floorplan::live::Error::Budget {
        budget,
        elapsed,
        parts,
        engine,
        phase,
    } = error
    else {
        unreachable!("budget_refusal only formats a budget error")
    };
    let millis = |duration: Duration| duration.as_millis().min(u128::from(u64::MAX)) as u64;
    let overrun = elapsed.saturating_sub(*budget);
    let overran_ms = if elapsed > budget {
        millis(overrun).max(1)
    } else {
        0
    };
    json!({
        "error": error.to_string(),
        "placement": {
            "engine": engine,
            "parts": parts,
            "budget_ms": millis(*budget),
            "elapsed_ms": millis(*elapsed),
            "overran_ms": overran_ms,
            "phase": phase,
        }
    })
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

/// Rewrite every `"@R1.2"` pin target to a real net name, labelling the referenced
/// pin first when its net has only a KiCAD-generated name.
///
/// This is what makes a net reachable that has no name of its own: the model can
/// say "join whatever P3 pin 1 is on" instead of guessing at `Net-(P3-Pad1)`,
/// which is regenerated from the net's own pins and forks it if written as a label.
#[allow(clippy::type_complexity)]
fn resolve_pin_net_refs(
    payload: &mut sch_check::PlacePartsInput,
    edit: &mut Edit,
) -> Result<std::result::Result<Vec<String>, Value>> {
    let specs: std::collections::BTreeSet<String> = payload
        .parts
        .iter()
        .flat_map(|part| part.pins.values())
        .filter(|net| net.starts_with(crate::refs::NET_OF_PIN))
        .cloned()
        .collect();
    if specs.is_empty() {
        return Ok(Ok(Vec::new()));
    }
    let mut minted = Vec::new();
    let mut resolved = std::collections::BTreeMap::new();
    for spec in specs {
        match crate::refs::net_of_pin(&edit.doc, edit.before(), &spec) {
            Ok(found) => {
                if let crate::refs::PinNet::Mint {
                    refdes,
                    number,
                    net,
                } = &found
                    && let Ok(pin) = crate::refs::pin(&edit.doc, &format!("{refdes}.{number}"))
                {
                    edit.doc
                        .add_label(sch_doc::LabelKind::Local, net, crate::wiring::pose(pin.at));
                    minted.push(net.clone());
                    if let Some(was) =
                        crate::refs::net_of(edit.before(), refdes, number).map(str::to_string)
                    {
                        minted.push(was);
                    }
                }
                resolved.insert(spec, found.net().to_string());
            }
            Err(error) => {
                return Ok(Err(json!({
                    "ok": false,
                    "code": "unknown_pin_net_ref",
                    "error": format!("{spec}: {error}"),
                })));
            }
        }
    }
    for part in &mut payload.parts {
        for net in part.pins.values_mut() {
            if let Some(found) = resolved.get(net.as_str()) {
                *net = found.clone();
            }
        }
    }
    Ok(Ok(minted))
}

/// The engine a payload named, if it named a real one.
fn engine_named(name: &str) -> Option<PlacementEngineKind> {
    match name {
        "anneal" => Some(PlacementEngineKind::Anneal),
        "spine" => Some(PlacementEngineKind::Spine),
        "cluster" => Some(PlacementEngineKind::Cluster),
        _ => None,
    }
}

/// The engines to attempt, in order: the one asked for, else the sheet's default
/// followed by the others as fallbacks.
fn engines_to_try(requested: Option<PlacementEngineKind>) -> Vec<PlacementEngineKind> {
    if let Some(kind) = requested {
        return vec![kind];
    }
    vec![
        PlacementEngineKind::Spine,
        PlacementEngineKind::Cluster,
        PlacementEngineKind::Anneal,
    ]
}

fn refused_place(
    report: PlaceReport,
    payload: &sch_check::PlacePartsInput,
    tried: &[&str],
    skipped: &[&str],
    sheet_parts: usize,
) -> Value {
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
    let mut blocks: std::collections::BTreeMap<&str, usize> = Default::default();
    for part in &payload.parts {
        let block = part
            .block
            .as_deref()
            .or(payload.block.as_deref())
            .unwrap_or(sch_check::place_parts::DEFAULT_BLOCK);
        *blocks.entry(block).or_default() += 1;
    }
    let split: Vec<String> = blocks
        .iter()
        .map(|(name, count)| format!("{name} ({count} parts)"))
        .collect();
    let block_guidance = if sheet_parts >= 60 {
        " This sheet is large: place one named functional block per call with the `block` field; \
         each call takes the region path and freezes symbols already on the sheet."
    } else {
        ""
    };
    json!({
        "error": format!(
            "refused: the placed result does not match the requested connectivity ({}); nothing \
             was written. This is a placement-engine failure, not a payload error — {} already \
             tried it. Send one payload per block instead ({}); a smaller block is what has \
             recovered this every time.{}",
            why.join("; "),
            tried.join(", then "),
            split.join(", "),
            block_guidance,
        ),
        "engines_tried": tried,
        "engines_skipped": skipped,
        "split_into": split,
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

fn engine_kind_name(selected: PlacementEngineKind) -> &'static str {
    match selected {
        PlacementEngineKind::Anneal => "anneal",
        PlacementEngineKind::Spine => "spine",
        PlacementEngineKind::Cluster => "cluster",
    }
}

/// One placement call's wall time, logged when it ends — the record that says
/// whether the deadline policy is holding on real designs.
struct Timing {
    tool: &'static str,
    engine: &'static str,
    budget_ms: u64,
    parts: usize,
    started: std::time::Instant,
}

impl Timing {
    fn start(tool: &'static str, budget: &PlacementBudget, engine: &'static str) -> Self {
        Self {
            tool,
            engine,
            budget_ms: budget.budget.as_millis().min(u128::from(u64::MAX)) as u64,
            parts: budget.parts,
            started: std::time::Instant::now(),
        }
    }

    fn done(&self, outcome: &str) -> u64 {
        let elapsed_ms = self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        tracing::info!(
            tool = self.tool,
            engine = self.engine,
            parts = self.parts,
            budget_ms = self.budget_ms,
            elapsed_ms,
            outcome,
            "placement finished"
        );
        elapsed_ms
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
    budgeted_within(ctx, PlacementBudget::new(parts).budget, parts, engine)
}

/// A budget clipped to `remaining` — what a later rung of the engine ladder gets.
fn budgeted_within(
    ctx: &AgentRuntime,
    remaining: Duration,
    parts: usize,
    engine: Option<PlacementEngineKind>,
) -> (PlacementBudget, Box<dyn PlacementEngine>) {
    let budget = PlacementBudget::within(remaining, parts);
    let engine = budget.engine(engine.or(ctx.config().engines.schematic_placer));
    (budget, placement_engine(engine))
}

#[cfg(test)]
mod tests {
    use sch_model::engine::{CandidateEvaluator, PlacementOutput, SchematicPlaceProblem};
    use serde_json::json;

    use super::*;

    struct PanicEngine;

    impl PlacementEngine for PanicEngine {
        fn name(&self) -> &'static str {
            "panic-stub"
        }

        fn place(
            &self,
            _problem: &mut SchematicPlaceProblem,
            _eval: &dyn CandidateEvaluator,
        ) -> PlacementOutput {
            panic!("stub placement panic")
        }
    }

    #[test]
    fn budget_refusal_reports_a_real_sub_millisecond_overrun() {
        let error = PlacementBudget::within(Duration::from_secs(1), 70).overrun(
            Duration::from_secs(1) + Duration::from_nanos(1),
            "spine",
            "verify",
        );
        let value = budget_refusal(&error);
        assert_eq!(value.pointer("/placement/overran_ms"), Some(&json!(1)));
    }

    #[test]
    fn panicking_engine_restores_the_document_and_falls_through() {
        let Some(ctx) = AgentRuntime::detect_for_test() else {
            eprintln!("SKIP: no KiCAD detected");
            return;
        };
        let payload: sch_check::PlacePartsInput = serde_json::from_value(json!({
            "parts": [
                {"ref": "R1", "part": "Device:R", "pins": {"1": "VCC", "2": "MID"}},
                {"ref": "R2", "part": "Device:R", "pins": {"1": "MID", "2": "GND"}}
            ]
        }))
        .unwrap();
        let mut doc = sch_floorplan::live::blank_sheet().unwrap();
        let before = doc.to_text();
        let mut tried = Vec::new();

        match guarded_place_parts(ctx.env(), &mut doc, &payload, Box::new(PanicEngine), None)
            .unwrap()
        {
            GuardedPlacement::Panicked => tried.push(PanicEngine.name()),
            GuardedPlacement::Completed(_) => panic!("panic stub unexpectedly completed"),
        }
        assert_eq!(doc.to_text(), before);

        let report = match guarded_place_parts(
            ctx.env(),
            &mut doc,
            &payload,
            Box::new(spine_place::SpinePlace),
            None,
        )
        .unwrap()
        {
            GuardedPlacement::Completed(result) => {
                tried.push("spine");
                (*result).unwrap()
            }
            GuardedPlacement::Panicked => panic!("fallback engine panicked"),
        };

        assert!(report.committed, "fallback placement was refused");
        assert_eq!(tried, ["panic-stub", "spine"]);
    }
}
