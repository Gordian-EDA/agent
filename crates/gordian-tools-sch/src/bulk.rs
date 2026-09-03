//! Solver-owned bulk creation, arrangement and rewiring over the live schematic.

use anyhow::{Context, Result, anyhow};
use gordian_runtime::AgentRuntime;
use gordian_runtime::config::PlacementEngineKind;
use sch_check::place_parts::PartSpec;
use sch_floorplan::live::{ArrangeReport, PlaceReport, PlacementBudget, Selection};
use sch_model::engine::PlacementEngine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
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
    block: Option<String>,
    intent: Option<sch_check::Intent>,
    engine: Option<PlacementEngineKind>,
}

#[derive(Debug, Serialize)]
struct UnresolvedFootprint {
    #[serde(rename = "ref")]
    refdes: String,
    requested: String,
    did_you_mean: Vec<String>,
}

pub(crate) fn selection_schema(engine: bool) -> Value {
    let mut properties = json!({
        "refs": {
            "type": "array",
            "items": { "type": "string" },
            "minItems": 1,
            "description": "Reference designators, bench symbols included."
        },
        "bbox": {
            "type": "array",
            "items": { "type": "number" },
            "minItems": 4,
            "maxItems": 4,
            "description": "Select symbols whose origins lie inside [x1,y1,x2,y2] mm. Never the bench."
        },
        "block": {
            "type": "string",
            "description": "Select every symbol tagged with this functional block, bench included."
        }
    });
    if engine {
        properties["intent"] =
            sch_check::place_parts_input_schema()["properties"]["intent"].clone();
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
            { "required": ["bbox"] },
            { "required": ["block"] }
        ],
        "additionalProperties": false
    })
}

pub(crate) fn place_parts(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let mut input = input;
    let warnings = sanitize_place_parts_input(&mut input);
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
    if let Some(error) = coalesce_payload_parts(&mut payload) {
        return Ok(with_warnings(error, &warnings));
    }
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
        reserved: edit.reserved().clone(),
    };
    let renamed = rename_occupied_references(&mut payload, &existing);
    let minted = match resolve_pin_net_refs(&mut payload, &mut edit)? {
        Ok(minted) => minted,
        Err(error) => return Ok(with_renamed(with_warnings(error, &warnings), &renamed)),
    };
    sch_check::place_parts::assign_references(&mut payload, ctx.provider(), &existing);
    let mut footprints_unresolved = clear_unknown_footprints(ctx, &mut payload)?;
    let (design, _, _) = sch_check::into_design(&payload, ctx.provider(), &existing);
    for mismatch in gordian_runtime::footprint_compat::design_pin_mismatches(ctx, &design)? {
        let did_you_mean = mismatch.suggestion.into_iter().collect();
        footprints_unresolved.push(UnresolvedFootprint {
            refdes: mismatch.reference.clone(),
            requested: mismatch.footprint,
            did_you_mean,
        });
        for part in &mut payload.parts {
            if part.refdes.as_deref() == Some(&mismatch.reference) {
                part.footprint = None;
            }
        }
    }
    let (_, diags, mut audit) = sch_check::into_design(&payload, ctx.provider(), &existing);
    audit.input_errors = diags
        .0
        .iter()
        .filter(|diagnostic| diagnostic.severity == sch_check::Severity::Error)
        .map(|diagnostic| format!("{}: {}", diagnostic.code, diagnostic.message))
        .collect();
    if !audit.is_valid() || diags.has_errors() {
        return Ok(with_renamed(
            invalid_payload_response(audit, &warnings),
            &renamed,
        ));
    }
    if audit.unplaced.len() == payload.parts.len() {
        return Ok(with_renamed(
            nothing_placed_response(json!(audit.unplaced), &warnings),
            &renamed,
        ));
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
        return Ok(with_renamed(
            json!({ "ok": false, "code": "derived_net_name", "nets": derived }),
            &renamed,
        ));
    }
    let requested = match payload.engine.as_deref().map(engine_named) {
        Some(Some(kind)) => Some(kind),
        Some(None) => {
            return Ok(with_renamed(
                json!({
                    "error": format!(
                        "unknown engine `{}`; use \"anneal\", \"spine\" or \"cluster\"",
                        payload.engine.clone().unwrap_or_default()
                    ),
                }),
                &renamed,
            ));
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
                return Ok(with_renamed(budget_refusal(&error), &renamed));
            }
            Err(sch_floorplan::live::Error::InvalidPayload(audit)) => {
                return Ok(with_renamed(
                    invalid_payload_response(*audit, &warnings),
                    &renamed,
                ));
            }
            Err(sch_floorplan::live::Error::Nothing) => {
                return Ok(with_renamed(
                    nothing_placed_response(non_placeable_parts(&payload), &warnings),
                    &renamed,
                ));
            }
            Err(error) => return Err(error.into()),
        }
    }
    let report = match report {
        Some(report) => report,
        None => {
            return Ok(with_renamed(
                json!({
                    "error": "no placement engine completed",
                    "engines_tried": tried,
                    "engines_skipped": skipped,
                }),
                &renamed,
            ));
        }
    };
    if !report.committed {
        // Every engine drew a sheet that did not mean what the payload said. The
        // connectivity is not the engines' to throw away: the parts go to the
        // bench, named at every pin, and `arrange` lays them out from there.
        return bench_payload(
            ctx,
            edit,
            &payload,
            &mismatch_clause(&report.mismatch),
            &tried,
            &skipped,
            &warnings,
        )
        .map(|value| with_renamed(value, &renamed));
    }
    let refs = report.placed.clone();
    let mut value = edit
        .commit(
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
    let value = with_check(value, ctx).context("checking placed parts")?;
    let value = with_unresolved_footprints(value, &footprints_unresolved);
    Ok(with_renamed(with_warnings(value, &warnings), &renamed))
}

fn nothing_placed_response(unplaced: Value, warnings: &[String]) -> Value {
    with_warnings(
        json!({
            "ok": false,
            "code": "nothing_placed",
            "unplaced": unplaced,
            "note": "no part in this payload can be placed, so the sheet is unchanged. \
                     Each entry names the part and why it was left out.",
        }),
        warnings,
    )
}

fn non_placeable_parts(payload: &sch_check::PlacePartsInput) -> Value {
    Value::Array(
        payload
            .parts
            .iter()
            .map(|part| {
                json!({
                    "ref": part.refdes.clone().unwrap_or_else(|| part.part.clone()),
                    "part": part.part,
                    "reason": if part.part.starts_with("power:") || part.part.starts_with("label:") {
                        "connectivity furniture is generated by wiring and cannot be placed as a part"
                    } else {
                        "no placeable symbol geometry was produced"
                    },
                })
            })
            .collect(),
    )
}

fn with_renamed(mut value: Value, renamed: &BTreeMap<String, String>) -> Value {
    if !renamed.is_empty() {
        value["renamed"] = json!(renamed);
    }
    value
}

/// Collapse repeated declarations of one part, rejecting only declarations that
/// make the reference ambiguous or assign one field two different values.
fn coalesce_payload_parts(payload: &mut sch_check::PlacePartsInput) -> Option<Value> {
    let mut parts: Vec<PartSpec> = Vec::new();
    let mut by_ref = BTreeMap::new();
    for part in std::mem::take(&mut payload.parts) {
        let Some(refdes) = part.refdes.clone() else {
            parts.push(part);
            continue;
        };
        let Some(&index) = by_ref.get(&refdes) else {
            by_ref.insert(refdes, parts.len());
            parts.push(part);
            continue;
        };
        let held_part = parts[index].part.clone();
        if held_part != part.part {
            let incoming_part = part.part.clone();
            payload.parts = parts;
            return Some(duplicate_part_refusal(
                &refdes,
                &held_part,
                &incoming_part,
                "names two different parts",
            ));
        }
        let held = &mut parts[index];
        if let Err(field) = merge_part(held, part) {
            let lib_id = held.part.clone();
            payload.parts = parts;
            return Some(duplicate_part_refusal(
                &refdes,
                &lib_id,
                &lib_id,
                &format!("assigns conflicting `{field}` values"),
            ));
        }
    }
    payload.parts = parts;
    None
}

fn merge_part(held: &mut PartSpec, incoming: PartSpec) -> std::result::Result<(), String> {
    merge_option(&mut held.block, incoming.block, "block")?;
    merge_option(&mut held.value, incoming.value, "value")?;
    merge_option(&mut held.footprint, incoming.footprint, "footprint")?;
    held.dnp |= incoming.dnp;
    merge_map(&mut held.props, incoming.props, "props")?;
    merge_map(&mut held.pins, incoming.pins, "pins")?;
    merge_map(&mut held.decouple, incoming.decouple, "decouple")?;
    Ok(())
}

fn merge_option<T: PartialEq>(
    held: &mut Option<T>,
    incoming: Option<T>,
    field: &str,
) -> std::result::Result<(), String> {
    match (held.as_ref(), incoming) {
        (Some(a), Some(b)) if *a != b => Err(field.to_string()),
        (None, Some(value)) => {
            *held = Some(value);
            Ok(())
        }
        _ => Ok(()),
    }
}

fn merge_map<V>(
    held: &mut indexmap::IndexMap<String, V>,
    incoming: indexmap::IndexMap<String, V>,
    field: &str,
) -> std::result::Result<(), String>
where
    V: PartialEq,
{
    for (key, value) in incoming {
        match held.get(&key) {
            Some(held_value) if *held_value != value => return Err(field.to_string()),
            Some(_) => {}
            None => {
                held.insert(key, value);
            }
        }
    }
    Ok(())
}

fn duplicate_part_refusal(refdes: &str, first: &str, second: &str, why: &str) -> Value {
    json!({
        "ok": false,
        "code": "invalid_payload",
        "input_errors": [format!(
            "duplicate-ref: `{refdes}` {why} (`{first}` and `{second}`); give each physical part a distinct reference"
        )],
        "duplicate_refs": [],
        "unknown_pins": [],
        "footprint_mismatch": [],
        "unplaced": [],
        "dangling": [],
        "did_you_mean": {},
        "unreliable_nets": [],
    })
}

/// Rename references already present on the sheet, then rewrite every reference
/// carried by electrical or layout intent.
fn rename_occupied_references(
    payload: &mut sch_check::PlacePartsInput,
    existing: &sch_check::ExistingSheet,
) -> BTreeMap<String, String> {
    let mut occupied: BTreeSet<String> = existing
        .refs
        .union(&existing.reserved)
        .cloned()
        .chain(payload.parts.iter().filter_map(|part| part.refdes.clone()))
        .collect();
    let mut renamed = BTreeMap::new();
    for part in &mut payload.parts {
        let Some(refdes) = part
            .refdes
            .as_ref()
            .filter(|refdes| existing.refs.contains(*refdes))
            .cloned()
        else {
            continue;
        };
        let prefix = refdes.trim_end_matches(|ch: char| ch.is_ascii_digit());
        let next = (1..)
            .map(|number| format!("{prefix}{number}"))
            .find(|candidate| !occupied.contains(candidate))
            .expect("the reference-number suffix space is unbounded");
        occupied.insert(next.clone());
        part.refdes = Some(next.clone());
        renamed.insert(refdes, next);
    }
    rewrite_payload_references(payload, &renamed);
    renamed
}

fn rewrite_payload_references(
    payload: &mut sch_check::PlacePartsInput,
    renamed: &BTreeMap<String, String>,
) {
    for part in &mut payload.parts {
        for net in part.pins.values_mut() {
            for (old, new) in renamed {
                if let Some(pin) = net.strip_prefix(&format!("@{old}.")) {
                    *net = format!("@{new}.{pin}");
                    break;
                }
            }
        }
    }
    for grid in payload.layout.values_mut() {
        for refdes in grid.iter_mut().flatten().flatten() {
            rewrite_ref(refdes, renamed);
        }
    }
    let Some(intent) = &mut payload.intent else {
        return;
    };
    intent.place = std::mem::take(&mut intent.place)
        .into_iter()
        .map(|(refdes, cell)| (renamed.get(&refdes).cloned().unwrap_or(refdes), cell))
        .collect();
    intent.mirror = std::mem::take(&mut intent.mirror)
        .into_iter()
        .map(|refdes| renamed.get(&refdes).cloned().unwrap_or(refdes))
        .collect();
    for relation in &mut intent.relations {
        use sch_model::ir::{GroupSide, Relation};
        match relation {
            Relation::LeftOf { a, b }
            | Relation::RightOf { a, b }
            | Relation::Above { a, b }
            | Relation::Below { a, b } => {
                rewrite_ref(a, renamed);
                rewrite_ref(b, renamed);
            }
            Relation::Group {
                members,
                side,
                anchor,
                ..
            } => {
                for member in members {
                    rewrite_ref(member, renamed);
                }
                if let Some(anchor) = anchor {
                    rewrite_ref(anchor, renamed);
                }
                match side {
                    Some(GroupSide::Anchored(_, anchor))
                    | Some(GroupSide::Named { anchor, .. }) => rewrite_ref(anchor, renamed),
                    Some(GroupSide::Edge(_)) | None => {}
                }
            }
            Relation::Align { members, .. } => {
                for member in members {
                    rewrite_ref(member, renamed);
                }
            }
        }
    }
}

fn rewrite_ref(refdes: &mut String, renamed: &BTreeMap<String, String>) {
    if let Some(replacement) = renamed.get(refdes) {
        *refdes = replacement.clone();
    }
}

fn clear_unknown_footprints(
    ctx: &AgentRuntime,
    payload: &mut sch_check::PlacePartsInput,
) -> Result<Vec<UnresolvedFootprint>> {
    let mut unresolved = Vec::new();
    for part in &mut payload.parts {
        let Some(requested) = part
            .footprint
            .as_deref()
            .filter(|footprint| !footprint.is_empty())
            .map(str::to_owned)
        else {
            continue;
        };
        let Some(did_you_mean) =
            gordian_runtime::footprint_compat::unresolved_footprint_suggestions(
                ctx, &part.part, &requested,
            )?
        else {
            continue;
        };
        unresolved.push(UnresolvedFootprint {
            refdes: part
                .refdes
                .clone()
                .unwrap_or_else(|| format!("unassigned {}", part.part)),
            requested,
            did_you_mean,
        });
        part.footprint = None;
    }
    Ok(unresolved)
}

fn with_unresolved_footprints(mut value: Value, unresolved: &[UnresolvedFootprint]) -> Value {
    if unresolved.is_empty() {
        return value;
    }
    value["footprints_unresolved"] = json!(unresolved);
    let gaps = value["gaps"]
        .as_array_mut()
        .expect("place_parts gaps array");
    gaps.extend(unresolved.iter().map(|issue| {
        json!({
            "kind": "footprint_unresolved",
            "refdes": issue.refdes,
            "suggestion": format!(
                "assign a compatible footprint to {} with assign_footprints",
                issue.refdes
            ),
        })
    }));
    value
}

/// Render the one exhaustive refusal shape used by both audit phases.
fn invalid_payload_response(audit: sch_check::PayloadAudit, warnings: &[String]) -> Value {
    with_warnings(
        json!({
            "ok": false,
            "code": "invalid_payload",
            "input_errors": audit.input_errors,
            "duplicate_refs": audit.duplicate_refs,
            "unknown_pins": audit.unknown_pins,
            "footprint_mismatch": audit.footprint_mismatch,
            "unplaced": audit.unplaced,
            "dangling": audit.dangling,
            "did_you_mean": audit.did_you_mean,
            "unreliable_nets": audit.unreliable_nets,
            "note": "this lists EVERY fault in the payload — fix them all before retrying. \
                     `place_parts` appends to the sheet, so resubmit only the parts named \
                     here, not the whole payload. `input_errors` are unresolvable lib_ids \
                     and pin conflicts; occupied references are repaired in `renamed`, while \
                     `duplicate_refs` identify one ref used for incompatible declarations; \
                     `unknown_pins` name a key the symbol does not have; `footprint_mismatch` \
                     includes the closest same-library pad-set repair. `unplaced` parts could \
                     not be resolved at all and were left out. `dangling` pins are NOT fatal \
                     on their own — they are listed so you can finish them.",
        }),
        warnings,
    )
}

fn with_warnings(mut value: Value, warnings: &[String]) -> Value {
    if !warnings.is_empty() {
        value["warnings"] = json!(warnings);
    }
    value
}

/// Drop isolated layout-hint defects without weakening the electrical part schema.
fn sanitize_place_parts_input(input: &mut Value) -> Vec<String> {
    let mut warnings = Vec::new();
    if let Some(parts) = input.get_mut("parts").and_then(Value::as_array_mut) {
        for (index, part) in parts.iter_mut().enumerate() {
            if part
                .as_object_mut()
                .and_then(|part| part.remove(""))
                .is_some()
            {
                warnings.push(format!("dropped empty field at parts[{index}]."));
            }
        }
    }
    // A layout hint of the wrong SHAPE — `"rails": "+3V3"` where a map belongs — is
    // still only a hint. Dropping it costs a rail band; refusing the payload costs
    // the whole block.
    if let Some(intent) = input.get_mut("intent") {
        if intent.as_object().is_some() {
            let malformed: Vec<String> = intent
                .as_object()
                .expect("just checked")
                .iter()
                .filter(|(field, value)| match field.as_str() {
                    "rails" | "ports" | "place" => !value.is_object(),
                    "relations" => !value.is_array(),
                    "mirror" => !value.is_array(),
                    _ => false,
                })
                .map(|(field, _)| field.clone())
                .collect();
            for field in malformed {
                intent.as_object_mut().expect("just checked").remove(&field);
                warnings.push(format!("dropped malformed intent.{field}: wrong shape"));
            }
        } else {
            warnings.push("dropped malformed intent: expected an object".to_string());
            input
                .as_object_mut()
                .expect("place_parts input is an object")
                .remove("intent");
        }
    }
    let Some(intent) = input.get_mut("intent").and_then(Value::as_object_mut) else {
        return warnings;
    };
    if let Some(ports) = intent.get_mut("ports").and_then(Value::as_object_mut) {
        ports.retain(|net, side| {
            if serde_json::from_value::<sch_model::ir::Side>(side.clone()).is_ok() {
                true
            } else {
                warnings.push(format!(
                    "dropped malformed intent.ports.{net}: expected left, right, top, or bottom"
                ));
                false
            }
        });
    }
    if let Some(rails) = intent.get_mut("rails").and_then(Value::as_object_mut) {
        rails.retain(|net, side| match side.as_str() {
            Some("top" | "bottom") => true,
            Some("left") => {
                *side = json!("top");
                warnings.push(format!(
                    "mapped intent.rails.{net} from left to the engine-supported top band"
                ));
                true
            }
            Some("right") => {
                *side = json!("bottom");
                warnings.push(format!(
                    "mapped intent.rails.{net} from right to the engine-supported bottom band"
                ));
                true
            }
            _ => {
                warnings.push(format!(
                    "dropped malformed intent.rails.{net}: expected left, right, top, or bottom"
                ));
                false
            }
        });
    }
    if let Some(relations) = intent.get_mut("relations").and_then(Value::as_array_mut) {
        let mut valid = Vec::with_capacity(relations.len());
        for (index, relation) in relations.drain(..).enumerate() {
            match serde_json::from_value::<sch_model::ir::Relation>(relation.clone()) {
                Ok(_) => valid.push(relation),
                Err(error) => warnings.push(format!(
                    "dropped malformed intent.relations[{index}]: {error}"
                )),
            }
        }
        *relations = valid;
    }
    warnings
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

/// Re-lay out a selection, or — when the search runs out of clock — do the half of
/// the work that has no search in it.
///
/// The engines stop themselves at the search deadline, so an overrun means an
/// engine ignored it and the whole call was abandoned. Coming back empty is the
/// one outcome worth avoiding: a re-wire in place redraws the same selection's
/// wiring from the same netlist with no search at all, which is the honest
/// best-so-far — the layout is what it was, and the drawing is current.
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
        input.intent.clone(),
        engine,
        Some(budget),
    ) {
        Ok(report) => report,
        Err(error @ sch_floorplan::live::Error::Budget { .. }) => {
            timing.done("overran");
            return arrange_in_place(ctx, &selection, budget_refusal(&error));
        }
        Err(error) => return Err(error.into()),
    };
    timing.done(if report.committed {
        "committed"
    } else {
        "refused"
    });
    finish_arrangement(edit, report, ctx)
}

/// The searchless half of `arrange`, run after its budget was spent.
fn arrange_in_place(ctx: &AgentRuntime, selection: &Selection, overrun: Value) -> Result<Value> {
    let mut edit = Edit::open(ctx)?;
    let budget = PlacementBudget::new(edit.doc.symbols().count());
    let report =
        match sch_floorplan::live::rewire(ctx.env(), &mut edit.doc, selection, Some(budget)) {
            Ok(report) => report,
            Err(_) => return Ok(overrun),
        };
    if !report.committed {
        return Ok(overrun);
    }
    let mut value = finish_arrangement(edit, report, ctx)?;
    value["placement"] = overrun["placement"].clone();
    value["note"] = json!(
        "the placement search overran its budget; the selection's wiring was redrawn where it \
         stands instead. Arrange a smaller selection to move it."
    );
    Ok(value)
}

pub(crate) fn rewire(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let input: SelectionInput = typed(input, "rewire")?;
    if input.engine.is_some() || input.intent.is_some() {
        return Err(anyhow!(
            "rewire moves nothing, so it takes no engine or intent"
        ));
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
    finish_arrangement(edit, report, ctx)
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

fn finish_arrangement(edit: Edit, report: ArrangeReport, ctx: &AgentRuntime) -> Result<Value> {
    if !report.committed {
        // A re-layout draws the selection's wires FROM THE NETLIST, so this is not
        // supposed to be reachable — it is the last guard, and what it protects is
        // the netlist, which is intact either way.
        return Ok(json!({
            "ok": false,
            "code": "layout_unchanged",
            "error": format!(
                "the re-layout would have changed connectivity ({:?}); the sheet and its \
                 netlist are untouched. Arrange a smaller selection — one symbol at a time \
                 with `refs` always works.",
                report.mismatch
            ),
            "report": report,
        }));
    }
    // The re-layout owns the selection's own drawing: its rail symbols and flags
    // are replaced, and a net it now draws as a wire loses the authored name its
    // erased label carried. The partition is what may not move, and the live gate
    // has already checked that.
    let allow = Allow::nothing()
        .parts(report.moved.clone())
        .unname_nets(report.nets.clone())
        .creating();
    let value = edit.commit(json!(report), allow)?;
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
        if let Some(net) = payload_pin_net(payload, &spec) {
            resolved.insert(spec, net);
            continue;
        }
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

/// Resolve an `@ref.pin` that points at another part in the same payload.
fn payload_pin_net(payload: &sch_check::PlacePartsInput, spec: &str) -> Option<String> {
    let (refdes, pin) = spec
        .strip_prefix(crate::refs::NET_OF_PIN)?
        .rsplit_once('.')?;
    let net = payload
        .parts
        .iter()
        .find(|part| part.refdes.as_deref() == Some(refdes))?
        .pins
        .get(pin)?;
    (!net.starts_with(crate::refs::NET_OF_PIN)).then(|| net.clone())
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

/// The tool behind `add_parts`: a payload with no layout at all.
pub(crate) fn add_parts(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let mut input = input;
    let warnings = sanitize_place_parts_input(&mut input);
    let mut payload: sch_check::PlacePartsInput = typed(input, "add_parts")?;
    let mut edit = if ctx.sch_path().is_file() {
        Edit::open(ctx).context("opening the existing schematic")?
    } else {
        Edit::create(ctx, sch_floorplan::live::blank_sheet()?)
    };
    if let Some(error) = coalesce_payload_parts(&mut payload) {
        return Ok(with_warnings(error, &warnings));
    }
    let existing = sch_check::ExistingSheet {
        refs: edit
            .doc
            .symbols()
            .map(|symbol| symbol.refdes().to_string())
            .collect(),
        reserved: edit.reserved().clone(),
        ..Default::default()
    };
    let renamed = rename_occupied_references(&mut payload, &existing);
    match resolve_pin_net_refs(&mut payload, &mut edit)? {
        Ok(_) => {}
        Err(error) => return Ok(with_renamed(with_warnings(error, &warnings), &renamed)),
    }
    sch_check::place_parts::assign_references(&mut payload, ctx.provider(), &existing);
    bench_payload(
        ctx,
        edit,
        &payload,
        "added without layout — call arrange to lay it out",
        &[],
        &[],
        &warnings,
    )
    .map(|value| with_renamed(value, &renamed))
}

/// Write a payload's connectivity to the bench and commit it.
///
/// The bench is what makes a placement failure survivable: the parts and their
/// nets are written, only the LAYOUT is missing, and `arrange` is the one call
/// that finishes them. Nothing here can short anything — no wire is drawn.
fn bench_payload(
    ctx: &AgentRuntime,
    mut edit: Edit,
    payload: &sch_check::PlacePartsInput,
    why: &str,
    tried: &[&str],
    skipped: &[&str],
    warnings: &[String],
) -> Result<Value> {
    let report = match sch_floorplan::live::add_parts(ctx.env(), &mut edit.doc, payload, None, why)
    {
        Ok(report) => report,
        Err(sch_floorplan::live::Error::InvalidPayload(audit)) => {
            return Ok(invalid_payload_response(*audit, warnings));
        }
        Err(error) => return Err(error.into()),
    };
    if !report.committed {
        return Ok(with_warnings(
            json!({
                "ok": false,
                "code": "bench_mismatch",
                "error": "the bench draw did not preserve connectivity; nothing was written",
                "report": report,
            }),
            warnings,
        ));
    }
    let refs: Vec<String> = report
        .benched
        .iter()
        .map(|benched| benched.refdes.clone())
        .collect();
    let mut value = edit
        .commit(
            json!(report),
            Allow::nothing().parts(refs.clone()).creating(),
        )
        .context("committing benched parts")?;
    if value.get("error").is_some() {
        return Ok(value);
    }
    if !tried.is_empty() {
        value["engines_tried"] = json!(tried);
        value["engines_skipped"] = json!(skipped);
    }
    let joined = refs.join(" ");
    attach_connectivity(&mut value, ctx, refs, &format!("BENCHED  {joined}"))?;
    let value = with_check(value, ctx).context("checking benched parts")?;
    Ok(with_warnings(value, warnings))
}

/// One clause naming what the engines got wrong, for the bench report.
fn mismatch_clause(mismatch: &sch_floorplan::live::Mismatch) -> String {
    let mut why = Vec::new();
    if !mismatch.shorted.is_empty() {
        let pairs: Vec<String> = mismatch
            .shorted
            .iter()
            .map(|(a, b)| format!("{a}+{b}"))
            .collect();
        why.push(format!("shorted {}", pairs.join(", ")));
    }
    if !mismatch.scattered.is_empty() {
        why.push(format!("scattered {}", mismatch.scattered.join(", ")));
    }
    if !mismatch.disturbed.is_empty() {
        why.push(format!(
            "disturbed existing {}",
            mismatch.disturbed.join(", ")
        ));
    }
    format!(
        "no placement engine could draw it truthfully ({})",
        why.join("; ")
    )
}

fn selection(input: &SelectionInput) -> Result<Selection> {
    match (&input.refs, input.bbox, &input.block) {
        (Some(refs), None, None) if !refs.is_empty() => Ok(Selection::Refs(refs.clone())),
        (None, Some(bbox), None) => Ok(Selection::Bbox(bbox)),
        (None, None, Some(block)) if !block.is_empty() => Ok(Selection::Block(block.clone())),
        _ => Err(anyhow!(
            "provide exactly one non-empty `refs`, `bbox` or `block` selection"
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
