//! Solver-owned bulk creation, arrangement and rewiring over the live schematic.

use anyhow::{Context, Result, anyhow};
use gordian_runtime::AgentRuntime;
use sch_check::place_parts::PartSpec;
use sch_floorplan::live::{ArrangeReport, PlaceReport, Selection};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::session::{Allow, Edit, attach_connectivity};

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
    block: Option<String>,
    intent: Option<sch_check::Intent>,
    layout: Option<sch_model::tree::Tree>,
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
        properties["layout"] =
            sch_check::place_parts_input_schema()["properties"]["layout"]["additionalProperties"]
                .clone();
        properties["layout"]["description"] =
            json!("How the selection is arranged: one row/col tree over its parts.");
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
    let resolved_nets = match resolve_pin_net_refs(&mut payload, &mut edit)? {
        Ok(resolved) => resolved,
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
    // The last placement's timing, reported on the result whether it committed or not.
    let mut placement = json!(null);
    let timing = Timing::start("place_parts", payload.parts.len());
    let attempt = guarded_place_parts(ctx.env(), &mut edit.doc, &payload)?;
    let GuardedPlacement::Completed(result) = attempt else {
        timing.done("panicked");
        return Ok(with_renamed(
            json!({"error": "the typesetter panicked; nothing was written"}),
            &renamed,
        ));
    };
    let report = match *result {
        Ok(placed) => {
            let elapsed_ms = timing.done(if placed.committed {
                "committed"
            } else {
                "refused"
            });
            placement = json!({"parts": timing.parts, "elapsed_ms": elapsed_ms});
            placed
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
    };
    if !report.committed {
        // The drawn sheet did not mean what the payload said. The connectivity is not
        // the typesetter's to throw away: the parts go to the bench, named at every
        // pin, and `arrange` lays them out from there.
        let value = bench_payload(
            ctx,
            edit,
            &payload,
            &mismatch_clause(&report.mismatch),
            &warnings,
        )?;
        let value = with_renamed(value, &renamed);
        let value = with_resolved_nets(value, &resolved_nets.reported);
        return Ok(with_nc_overrides(value, &audit.nc_overridden));
    }
    let refs = report.placed.clone();
    let mut value = edit
        .commit(
            json!(report),
            // Naming a previously auto-named net renames it; that is the point of an
            // `@ref.pin` target, so the guard is told rather than surprised.
            Allow::nothing()
                .parts(refs)
                .joining_nets(resolved_nets.allowed.iter().cloned())
                .creating(),
        )
        .context("committing placed parts")?;
    if value.get("error").is_some() {
        return Ok(value);
    }
    value["placement"] = placement;
    let refs = report.placed.join(" ");
    attach_connectivity(&mut value, ctx, report.placed, &format!("PLACED  {refs}"))?;
    let value = with_check(value, ctx).context("checking placed parts")?;
    let value = with_unresolved_footprints(value, &footprints_unresolved);
    let value = with_unresolved_decoupling(value, &audit.decouple_unresolved);
    let value = with_resolved_nets(value, &resolved_nets.reported);
    let value = with_nc_overrides(value, &audit.nc_overridden);
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
        let Some(held) = parts.get(index) else {
            payload.parts = parts;
            return Some(json!({
                "ok": false,
                "code": "invalid_payload",
                "input_errors": [format!(
                    "internal reference lookup for model-provided `{refdes}` was missing; resubmit that part"
                )],
            }));
        };
        let held_part = held.part.clone();
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
        let Some(held) = parts.get_mut(index) else {
            payload.parts = parts;
            return Some(json!({
                "ok": false,
                "code": "invalid_payload",
                "input_errors": [format!(
                    "internal reference lookup for model-provided `{refdes}` was missing; resubmit that part"
                )],
            }));
        };
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
    for tree in payload.layout.values_mut() {
        rewrite_tree_refs(tree, renamed);
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

fn with_unresolved_decoupling(
    mut value: Value,
    unresolved: &[sch_check::place_parts::DecoupleUnresolved],
) -> Value {
    if unresolved.is_empty() {
        return value;
    }
    value["decouple_unresolved"] = json!(unresolved);
    let gaps = value["gaps"]
        .as_array_mut()
        .expect("place_parts gaps array");
    gaps.extend(unresolved.iter().map(|issue| {
        json!({
            "kind": "decouple_unresolved",
            "ref": issue.refdes,
            "why": issue.why,
            "how": issue.how,
        })
    }));
    value
}

fn with_nc_overrides(mut value: Value, overridden: &[sch_check::NcOverride]) -> Value {
    if overridden.is_empty() {
        return value;
    }
    value["nc_overridden"] = json!(overridden);
    let gaps = value["gaps"]
        .as_array_mut()
        .expect("place_parts gaps array");
    gaps.extend(overridden.iter().map(|issue| {
        json!({
            "kind": "library_no_connect_overridden",
            "ref": issue.refdes,
            "pin": issue.pin,
            "requested_net": issue.requested_net,
            "suggestion": "use a functional pin or a compatible symbol if this connection is required",
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
            "decouple_unresolved": audit.decouple_unresolved,
            "nc_overridden": audit.nc_overridden,
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
    // A layout hint of the wrong SHAPE — `"rails": "+3V3"` where a map belongs — or one
    // this build no longer has is still only a hint. Dropping it costs a rail band;
    // refusing the payload costs the whole block, and a hint the model learned from an
    // older build would cost every block it writes.
    if let Some(intent) = input.get_mut("intent") {
        if intent.as_object().is_some() {
            let unusable: Vec<String> = intent
                .as_object()
                .expect("just checked")
                .iter()
                .filter(|(field, value)| match field.as_str() {
                    "rails" | "ports" => !value.is_object(),
                    _ => true,
                })
                .map(|(field, _)| field.clone())
                .collect();
            for field in unusable {
                intent.as_object_mut().expect("just checked").remove(&field);
                warnings.push(format!(
                    "dropped intent.{field}: the sheet reads only `rails` and `ports`; \
                     where the parts go is the `layout` tree"
                ));
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
) -> Result<GuardedPlacement> {
    let snapshot = doc.snapshot();
    match catch_unwind(AssertUnwindSafe(|| {
        sch_floorplan::live::place_parts(env, doc, payload)
    })) {
        Ok(result) => Ok(GuardedPlacement::Completed(Box::new(result))),
        Err(panic) => {
            let message = panic
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("non-string panic payload");
            tracing::error!(panic = message, "the typesetter panicked");
            doc.restore(snapshot)?;
            Ok(GuardedPlacement::Panicked)
        }
    }
}

/// Re-typeset a selection: `sch_floorplan::live::arrange` places it, gated on the
/// module's truthfulness invariant (see its module docs) before anything is kept.
pub(crate) fn arrange(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let input: SelectionInput = typed(input, "arrange")?;
    let mut selection = selection(&input)?;
    let mut edit = Edit::open(ctx)?;
    let selection_notes = resolve_arrangeable_refs(&edit.doc, &mut selection);
    if matches!(&selection, Selection::Refs(refs) if refs.is_empty()) {
        return Ok(selection_notes.finish(json!({
            "changed": "no arrangeable parts selected",
            "note": "power flags and power symbols are connectivity furniture, not arrangeable parts; select one of the nearby real-part references instead",
        })));
    }
    let timing = Timing::start("arrange", edit.doc.symbols().count());
    let report = sch_floorplan::live::arrange(
        ctx.env(),
        &mut edit.doc,
        &selection,
        input.intent.clone(),
        input.layout.clone(),
    )?;
    timing.done(if report.committed {
        "committed"
    } else {
        "refused"
    });
    Ok(selection_notes.finish(finish_arrangement(edit, report, ctx)?))
}

#[derive(Default)]
struct ArrangeSelectionNotes {
    not_arrangeable: Vec<String>,
    missing: Vec<String>,
    arrangeable_nearby: BTreeMap<String, Vec<String>>,
}

impl ArrangeSelectionNotes {
    fn finish(self, mut value: Value) -> Value {
        if !self.not_arrangeable.is_empty() {
            value["not_arrangeable"] = json!(self.not_arrangeable);
            value["arrangeable_nearby"] = json!(self.arrangeable_nearby);
        }
        if !self.missing.is_empty() {
            value["missing"] = json!(self.missing);
        }
        value
    }
}

fn resolve_arrangeable_refs(
    doc: &sch_doc::SchDoc,
    selection: &mut Selection,
) -> ArrangeSelectionNotes {
    let Selection::Refs(requested) = selection else {
        return ArrangeSelectionNotes::default();
    };
    let arrangeable = doc
        .symbols()
        .filter(|symbol| {
            !symbol.refdes().is_empty()
                && !symbol.refdes().starts_with('#')
                && !symbol.lib_id.starts_with("power:")
        })
        .collect::<Vec<_>>();
    let mut notes = ArrangeSelectionNotes::default();
    let mut selected = Vec::new();
    for reference in std::mem::take(requested) {
        let Some(symbol) = doc.symbol_by_ref(&reference) else {
            notes.missing.push(reference);
            continue;
        };
        if !symbol.refdes().starts_with('#') && !symbol.lib_id.starts_with("power:") {
            selected.push(reference);
            continue;
        }
        let mut nearby = arrangeable
            .iter()
            .map(|candidate| {
                let dx = candidate.at.x - symbol.at.x;
                let dy = candidate.at.y - symbol.at.y;
                (dx * dx + dy * dy, candidate.refdes().to_string())
            })
            .collect::<Vec<_>>();
        nearby.sort_by(|left, right| {
            left.0
                .total_cmp(&right.0)
                .then_with(|| left.1.cmp(&right.1))
        });
        notes.arrangeable_nearby.insert(
            reference.clone(),
            nearby
                .into_iter()
                .map(|(_, reference)| reference)
                .take(6)
                .collect(),
        );
        notes.not_arrangeable.push(reference);
    }
    selected.sort();
    selected.dedup();
    notes.not_arrangeable.sort();
    notes.not_arrangeable.dedup();
    notes.missing.sort();
    notes.missing.dedup();
    *requested = selected;
    notes
}

pub(crate) fn rewire(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let input: SelectionInput = typed(input, "rewire")?;
    if input.intent.is_some() || input.layout.is_some() {
        return Err(anyhow!(
            "rewire moves nothing, so it takes no intent or layout"
        ));
    }
    let selection = selection(&input)?;
    let mut edit = Edit::open(ctx)?;
    let timing = Timing::start("rewire", edit.doc.symbols().count());
    let report = sch_floorplan::live::rewire(ctx.env(), &mut edit.doc, &selection)?;
    timing.done(if report.committed {
        "committed"
    } else {
        "refused"
    });
    finish_arrangement(edit, report, ctx)
}

fn finish_arrangement(edit: Edit, report: ArrangeReport, ctx: &AgentRuntime) -> Result<Value> {
    if !report.committed {
        return Ok(json!({
            "ok": false,
            "code": "layout_unchanged",
            "error": format!(
                "internal error: the netlist-driven layout exhausted its wire and label \
                 fallbacks ({:?}); the sheet and its netlist are untouched",
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
#[derive(Default)]
struct ResolvedPayloadNets {
    allowed: Vec<String>,
    reported: std::collections::BTreeMap<String, String>,
}

fn resolve_pin_net_refs(
    payload: &mut sch_check::PlacePartsInput,
    edit: &mut Edit,
) -> Result<std::result::Result<ResolvedPayloadNets, Value>> {
    let specs: std::collections::BTreeSet<String> = payload
        .parts
        .iter()
        .flat_map(|part| part.pins.values())
        .filter(|net| net.starts_with(crate::refs::NET_OF_PIN) || net.starts_with("Net-("))
        .cloned()
        .collect();
    if specs.is_empty() {
        return Ok(Ok(ResolvedPayloadNets::default()));
    }
    let mut result = ResolvedPayloadNets::default();
    let mut resolved = std::collections::BTreeMap::new();
    for original in specs {
        let spec = match crate::refs::derived_net_ref(&edit.doc, &original) {
            Ok(Some(derived)) => {
                result.reported.insert(original.clone(), derived.reported);
                derived.spec
            }
            Ok(None) => original.clone(),
            Err(error) => {
                return Ok(Err(json!({
                    "ok": false,
                    "code": "unknown_pin_net_ref",
                    "error": error,
                })));
            }
        };
        if let Some(net) = payload_pin_net(payload, &spec) {
            resolved.insert(original, net);
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
                    result.allowed.push(net.clone());
                    if let Some(was) =
                        crate::refs::net_of(edit.before(), refdes, number).map(str::to_string)
                    {
                        result.allowed.push(was);
                    }
                }
                resolved.insert(original, found.net().to_string());
            }
            Err(error) => {
                return Ok(Err(json!({
                    "ok": false,
                    "code": "unknown_pin_net_ref",
                    "error": format!("{original}: {error}"),
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
    edit.joined_nets(result.allowed.iter().cloned());
    Ok(Ok(result))
}

fn with_resolved_nets(
    mut value: Value,
    resolved: &std::collections::BTreeMap<String, String>,
) -> Value {
    if !resolved.is_empty() {
        value["resolved_nets"] = json!(resolved);
    }
    value
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

/// Rewrite every refdes a layout tree names.
fn rewrite_tree_refs(tree: &mut sch_model::tree::Tree, renamed: &BTreeMap<String, String>) {
    match tree {
        sch_model::tree::Tree::Leaf(leaf) => rewrite_ref(&mut leaf.part, renamed),
        sch_model::tree::Tree::Container(c) => c
            .children
            .iter_mut()
            .for_each(|child| rewrite_tree_refs(child, renamed)),
    }
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
    let joined = refs.join(" ");
    attach_connectivity(&mut value, ctx, refs, &format!("BENCHED  {joined}"))?;
    let value = with_check(value, ctx).context("checking benched parts")?;
    Ok(with_warnings(value, warnings))
}

/// One clause naming what the typesetter got wrong, for the bench report.
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
    format!("could not be drawn truthfully ({})", why.join("; "))
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

/// One layout call's wall time, logged when it ends.
struct Timing {
    tool: &'static str,
    parts: usize,
    started: std::time::Instant,
}

impl Timing {
    fn start(tool: &'static str, parts: usize) -> Self {
        Self {
            tool,
            parts,
            started: std::time::Instant::now(),
        }
    }

    fn done(&self, outcome: &str) -> u64 {
        let elapsed_ms = self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        tracing::info!(
            tool = self.tool,
            parts = self.parts,
            elapsed_ms,
            outcome,
            "layout finished"
        );
        elapsed_ms
    }
}
