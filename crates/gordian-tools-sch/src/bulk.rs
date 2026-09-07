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
    layout: Option<ArrangeLayout>,
}

/// `arrange` lays out ONE selection, so its `layout` is one tree — but `place_parts`
/// takes a tree PER BLOCK, and a caller moving between the two writes the block map
/// here often enough that refusing it just costs a round trip. A map of one block is
/// that block's tree; a map of several is their trees in a row, which is what
/// arranging them together means.
#[derive(Deserialize)]
#[serde(untagged)]
enum ArrangeLayout {
    Tree(Box<sch_model::tree::Tree>),
    Blocks(BTreeMap<String, sch_model::tree::Tree>),
}

impl ArrangeLayout {
    fn tree(self) -> Option<sch_model::tree::Tree> {
        use sch_model::tree::{Align, Axis, Container, Tree, Margin};
        match self {
            ArrangeLayout::Tree(tree) => Some(*tree),
            ArrangeLayout::Blocks(blocks) => {
                let mut children: Vec<Tree> = blocks.into_values().collect();
                match children.len() {
                    0 => None,
                    1 => children.pop(),
                    _ => Some(Tree::Container(Container {
                        axis: Axis::Row,
                        children,
                        gap: None,
                        align: Align::Center,
                        wrap: None,
                        margin: Margin::default(),
                    })),
                }
            }
        }
    }
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
            "description": "Select every symbol of this block, by its title or the region name it was placed under (case and punctuation do not matter), bench included."
        }
    });
    if engine {
        properties["intent"] =
            sch_check::place_parts_input_schema()["properties"]["intent"].clone();
        properties["layout"] = sch_check::place_parts::layout_tree_schema();
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
    // One tree for the whole payload is the common case now that blocks are made
    // afterwards: file it under the payload's own region name.
    if let Some(Value::Object(tree)) = input.get("layout")
        && (tree.contains_key("row") || tree.contains_key("col") || tree.contains_key("part"))
    {
        let region = input
            .get("block")
            .and_then(Value::as_str)
            .unwrap_or(sch_check::place_parts::DEFAULT_BLOCK)
            .to_string();
        let tree = input["layout"].take();
        input["layout"] = json!({ region: tree });
    }
    let mut warnings = sanitize_place_parts_input(&mut input);
    let mut payload: sch_check::PlacePartsInput = typed(input, "place_parts")?;
    // The exact payload is what reproduces a placement; nothing else in the log does.
    tracing::debug!(
        payload = %serde_json::to_string(&payload).unwrap_or_default(),
        "place_parts"
    );
    if payload.parts.is_empty() {
        return Ok(with_warnings(empty_payload_refusal(&payload), &warnings));
    }
    let mut edit = if ctx.sch_path().is_file() {
        Edit::open(ctx).context("opening the existing schematic")?
    } else {
        Edit::create(ctx, sch_floorplan::live::blank_sheet()?)
    };
    if payload.strict {
        let mut scope = ctx.request_scope();
        scope.no_additions = true;
        ctx.workspace()
            .set_request_scope(&scope)
            .context("recording the request's part-list constraint")?;
    }
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
    let (footprints_resolved, mut footprints_unresolved) = clear_unknown_footprints(ctx, &mut payload)?;
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
    warnings.extend(crowded_blocks(&design));
    warnings.extend(unrelated_neighbours(&design));
    let (_, diags, mut audit) = sch_check::into_design(&payload, ctx.provider(), &existing);
    audit.input_errors = diags
        .0
        .iter()
        .filter(|diagnostic| diagnostic.severity == sch_check::Severity::Error)
        .map(|diagnostic| format!("{}: {}", diagnostic.code, diagnostic.message))
        .collect();
    // Layout warnings say what the drawing did with a tree the payload got slightly
    // wrong — a region adopted, a hint dropped. They only reach the author here.
    warnings.extend(
        diags
            .0
            .iter()
            .filter(|diagnostic| diagnostic.severity == sch_check::Severity::Warning)
            .filter(|diagnostic| {
                diagnostic.code.starts_with("layout-") || diagnostic.code == "unknown-block"
            })
            .map(|diagnostic| format!("{}: {}", diagnostic.code, diagnostic.message)),
    );
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
                nothing_placed_response(non_placeable_parts(&payload, &audit), &warnings),
                &renamed,
            ));
        }
        Err(sch_floorplan::live::Error::BodyOverlap(overlaps)) => {
            return Ok(with_renamed(
                overlap_refusal(&overlaps, &warnings),
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
            // `@ref.pin` target and of a net an earlier block drew unnamed, so the
            // guard is told rather than surprised.
            Allow::nothing()
                .parts(refs)
                .joining_nets(resolved_nets.allowed.iter().cloned())
                .promoting_nets(report.promoted.clone())
                .creating(),
        )
        .context("committing placed parts")?;
    if value.get("error").is_some() {
        return Ok(value);
    }
    value["placement"] = placement;
    if let Some(blocks) = outlined_blocks(ctx) {
        value["next"] = json!(format!(
            "the sheet already has {blocks} outlined block(s); put these parts in a block with \
             create_and_update_block and call arrange_blocks again so they are tiled with the rest"
        ));
    }
    let refs = report.placed.join(" ");
    attach_connectivity(&mut value, ctx, report.placed, &format!("PLACED  {refs}"))?;
    let value = with_check(value, ctx).context("checking placed parts")?;
    let mut value = with_unresolved_footprints(value, &footprints_unresolved);
    crate::edit::attach_footprint_repairs(&mut value, &footprints_resolved, &[]);
    let value = with_unresolved_decoupling(value, &audit.decouple_unresolved);
    let value = with_resolved_nets(value, &resolved_nets.reported);
    let value = with_nc_overrides(value, &audit.nc_overridden);
    Ok(with_renamed(with_warnings(value, &warnings), &renamed))
}

/// Blocks holding more parts than one drawing can group.
///
/// A block is a section of the sheet, and a reader takes it in as one thing. Past
/// about a dozen parts it stops being a section: a 24-part H-bridge asked for as
/// ONE block comes back as a field of components joined by names, because there is
/// no arrangement of 24 parts that reads as a single idea. The typesetter draws
/// whatever tree it is handed, so this is the only place that can say so.
fn crowded_blocks(design: &sch_check::model::Design) -> Vec<String> {
    const ROOMY: usize = 12;
    design
        .blocks
        .iter()
        .filter(|(_, block)| block.components.len() > ROOMY)
        .map(|(name, block)| {
            let mut members: Vec<&str> = block.components.keys().map(String::as_str).collect();
            members.truncate(24);
            format!(
                "block `{name}` holds {} parts ({}), too many to read as one section: it will \
                 draw as a field of parts joined by labels rather than a circuit. Sections \
                 of 3-{ROOMY} are power entry, regulator, MCU core, each interface, each \
                 repeated channel — one `place_parts` call each, with its own `block` name \
                 and `layout` tree. Re-cut it now: `remove_region({{\"block\": \"{name}\"}})`, \
                 then place those parts back as two or more named sections.",
                block.components.len(),
                members.join(", ")
            )
        })
        .collect()
}

/// Neighbours in a row that share no net at all.
///
/// A row is one signal path, so the pair either side of a gap in it should be joined
/// by something. When they are not, the router has nothing to draw between them and
/// falls back to naming both ends — which is how a sheet ends up with half the wire a
/// person would draw on it and reads as a parts bin rather than a circuit.
///
/// Sharing ANY net counts, power included: a row of decoupling capacitors shares only
/// its rails and is exactly what the convention asks for.
fn unrelated_neighbours(design: &sch_check::model::Design) -> Vec<String> {
    let mut said = Vec::new();
    for (name, block) in &design.blocks {
        let Some(tree) = &block.layout else { continue };
        let nets = |leaf: &sch_model::tree::Leaf| -> BTreeSet<String> {
            block
                .components
                .get(&leaf.part)
                .into_iter()
                .flat_map(|comp| comp.pins.values())
                .filter_map(|target| match target {
                    sch_check::model::PinTarget::Net(net) => Some(net.clone()),
                    sch_check::model::PinTarget::NoConnect => None,
                })
                .collect()
        };
        for (left, right) in row_neighbours(tree) {
            let (a, b) = (nets(left), nets(right));
            if !a.is_empty() && !b.is_empty() && a.is_disjoint(&b) {
                said.push(format!(
                    "in block `{name}`, `{}` and `{}` sit side by side in a row but share \
                     no net: a row is one signal path, so put each next to the part it \
                     connects to, or give it its own row. Neighbours with nothing between \
                     them are drawn as labels at both ends instead of a wire.",
                    left.part, right.part
                ));
            }
        }
    }
    said
}

/// Every adjacent pair of LEAVES within one row of the tree.
fn row_neighbours(
    tree: &sch_model::tree::Tree,
) -> Vec<(&sch_model::tree::Leaf, &sch_model::tree::Leaf)> {
    use sch_model::tree::{Axis, Tree};
    let Tree::Container(container) = tree else {
        return Vec::new();
    };
    let mut pairs: Vec<_> = container.children.iter().flat_map(row_neighbours).collect();
    if container.axis == Axis::Row {
        for window in container.children.windows(2) {
            if let (Tree::Leaf(left), Tree::Leaf(right)) = (&window[0], &window[1]) {
                pairs.push((left, right));
            }
        }
    }
    pairs
}

/// `place_parts` with no `parts` at all.
///
/// It CREATES parts, so an empty list has nothing to do — and the way it arrives is
/// a payload that carries only a `layout` for parts already on the sheet, which is
/// what `arrange` is for. Saying "nothing could be placed" there named no part and
/// no reason, because there was no part to name.
fn empty_payload_refusal(payload: &sch_check::PlacePartsInput) -> Value {
    let wanted: Vec<&str> = payload
        .layout
        .values()
        .flat_map(sch_model::tree::Tree::leaves)
        .map(|leaf| leaf.part.as_str())
        .collect();
    let clause = if wanted.is_empty() {
        "This payload declares no parts and no layout.".to_string()
    } else {
        format!(
            "This payload declares no parts, only a layout over {}. `place_parts` \
             CREATES parts; to re-lay-out parts already on the sheet call \
             `arrange({{refs|block, layout}})` instead.",
            wanted.join(", ")
        )
    };
    json!({
        "ok": false,
        "code": "no_parts",
        "error": format!("{clause} `parts` must list at least one part to create."),
    })
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

/// Every part of a payload the typesetter drew nothing from, with the reason the
/// audit already knows where it has one.
///
/// The typesetter reports only that it produced nothing; a caller needs it per part,
/// because the repair is per part.
fn non_placeable_parts(
    payload: &sch_check::PlacePartsInput,
    audit: &sch_check::place_parts::PayloadAudit,
) -> Value {
    Value::Array(
        payload
            .parts
            .iter()
            .map(|part| {
                let refdes = part.refdes.clone().unwrap_or_else(|| part.part.clone());
                let audited = audit
                    .unplaced
                    .iter()
                    .find(|unplaced| unplaced.refdes == refdes);
                json!({
                    "ref": refdes,
                    "part": part.part,
                    "reason": match audited {
                        Some(unplaced) => unplaced.reason.clone(),
                        None if part.part.starts_with("power:") || part.part.starts_with("label:") =>
                            "connectivity furniture is generated by wiring and cannot be placed as \
                             a part: name the rail on a pin instead, or use add_power".to_string(),
                        None => "no placeable symbol geometry was produced".to_string(),
                    },
                    "did_you_mean": audited.map(|u| u.did_you_mean.clone()).unwrap_or_default(),
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

/// A footprint that does not fit its symbol is repaired to the unique compatible one
/// of its own library when there is one, and cleared otherwise; both are reported.
fn clear_unknown_footprints(
    ctx: &AgentRuntime,
    payload: &mut sch_check::PlacePartsInput,
) -> Result<(Vec<crate::edit::ResolvedFootprint>, Vec<UnresolvedFootprint>)> {
    let mut resolved = Vec::new();
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
        let refdes = part
            .refdes
            .clone()
            .unwrap_or_else(|| format!("unassigned {}", part.part));
        // A pin the payload declares no-connect needs no pad.
        let ignored: std::collections::BTreeSet<String> = part
            .pins
            .iter()
            .filter(|(_, net)| sch_check::place_parts::is_no_connect_name(net))
            .map(|(pin, _)| pin.clone())
            .collect();
        match crate::edit::footprint_repair(ctx, &refdes, &part.part, &requested, &ignored)? {
            crate::edit::FootprintRepair::Keep => {}
            crate::edit::FootprintRepair::Resolve { from, to } => {
                part.footprint = Some(to.clone());
                resolved.push(crate::edit::ResolvedFootprint { refdes, from, to });
            }
            crate::edit::FootprintRepair::Clear { requested, did_you_mean } => {
                unresolved.push(UnresolvedFootprint { refdes, requested, did_you_mean });
                part.footprint = None;
            }
        }
    }
    Ok((resolved, unresolved))
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
                "assign a compatible footprint to {} with set_fields({{footprints}})",
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

/// Whether a layout node names something to draw.
fn draws_something(node: &Value) -> bool {
    ["part", "row", "col"]
        .iter()
        .any(|key| node.get(key).is_some_and(|value| !value.is_null()))
}

/// Rewrite the layout nodes the grammar does not have into ones it does.
///
/// Two near misses recur: a node carrying BOTH `row` and `col`, which reads as the row
/// with what the `col` says stacked under it, and a bare `{gap: n}` sitting between
/// siblings, which is spacing written as a child. Both have one honest reading, and
/// refusing the payload over spelling costs a whole block; the rewrite is reported so
/// the next call is written the way the grammar reads.
fn normalize_layout_node(node: &mut Value, path: &str, warnings: &mut Vec<String>) {
    let Some(object) = node.as_object_mut() else {
        return;
    };
    split_row_and_col(object, path, warnings);
    for axis in ["row", "col"] {
        let Some(children) = object.get_mut(axis).and_then(Value::as_array_mut) else {
            continue;
        };
        for (index, child) in children.iter_mut().enumerate() {
            normalize_layout_node(child, &format!("{path}.{axis}[{index}]"), warnings);
        }
        let mut lifted_gap = None;
        children.retain(|child| {
            if draws_something(child) {
                return true;
            }
            if lifted_gap.is_none() {
                lifted_gap = child.get("gap").cloned();
            }
            warnings.push(format!(
                "{path}: dropped a `{axis}` child that names no part, row or col ({child}); \
                 spacing is the container's own `gap`, not a child"
            ));
            false
        });
        if let Some(gap) = lifted_gap
            && !object.contains_key("gap")
        {
            object.insert("gap".into(), gap);
        }
    }
}

/// A node saying both `row` and `col` becomes a col of [that row, the col's entries].
///
/// The node's own spacing and alignment were written for the row it names, so they
/// travel with it rather than governing the stacking that did not exist before.
fn split_row_and_col(
    object: &mut serde_json::Map<String, Value>,
    path: &str,
    warnings: &mut Vec<String>,
) {
    if !object.get("row").is_some_and(Value::is_array)
        || !object.get("col").is_some_and(Value::is_array)
    {
        return;
    }
    let mut inner = serde_json::Map::new();
    inner.insert("row".into(), object.remove("row").expect("just checked"));
    for attribute in ["gap", "align", "wrap"] {
        if let Some(value) = object.remove(attribute) {
            inner.insert(attribute.into(), value);
        }
    }
    let stacked = object.remove("col").expect("just checked");
    let mut children = vec![Value::Object(inner)];
    children.extend(stacked.as_array().cloned().expect("just checked"));
    object.insert("col".into(), Value::Array(children));
    warnings.push(format!(
        "{path}: a node is one of `part`, `row` or `col`; this one said both, read as \
         the row with the `col` entries stacked under it"
    ));
}

/// Drop isolated layout-hint defects without weakening the electrical part schema.
fn sanitize_place_parts_input(input: &mut Value) -> Vec<String> {
    let mut warnings = Vec::new();
    if let Some(layout) = input.get_mut("layout").and_then(Value::as_object_mut) {
        for (block, tree) in layout.iter_mut() {
            normalize_layout_node(tree, &format!("layout.{block}"), &mut warnings);
        }
    }
    if let Some(parts) = input.get_mut("parts").and_then(Value::as_array_mut) {
        for (index, part) in parts.iter_mut().enumerate() {
            let Some(fields) = part.as_object_mut() else { continue };
            if fields.remove("").is_some() {
                warnings.push(format!("dropped empty field at parts[{index}]."));
            }
            // `lib_id` is what the KiCAD file calls the part; `near`/`side` were how a
            // part used to ask for a seat, which the layout tree now decides.
            if let Some(lib_id) = fields.remove("lib_id")
                && !fields.contains_key("part")
            {
                fields.insert("part".into(), lib_id);
            }
            for key in ["near", "side"] {
                if fields.remove(key).is_some() {
                    warnings.push(format!("parts[{index}].{key} is ignored: where a part sits is the layout tree's."));
                }
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

/// The refusal a placement that would draw a symbol on a symbol comes back as.
fn overlap_refusal(overlaps: &sch_floorplan::live::Overlaps, warnings: &[String]) -> Value {
    with_warnings(
        json!({
            "error": overlaps.to_string(),
            "body_overlaps": overlaps.0,
        }),
        warnings,
    )
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
    let mut input = input;
    let mut warnings = Vec::new();
    match input.get_mut("layout") {
        Some(Value::Object(blocks))
            if !blocks.contains_key("row") && !blocks.contains_key("col") =>
        {
            for (block, tree) in blocks.iter_mut() {
                normalize_layout_node(tree, &format!("layout.{block}"), &mut warnings);
            }
        }
        Some(tree) => normalize_layout_node(tree, "layout", &mut warnings),
        None => {}
    }
    let mut input: SelectionInput = typed(input, "arrange")?;
    let layout = input.layout.take().and_then(ArrangeLayout::tree);
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
    let report = match sch_floorplan::live::arrange(
        ctx.env(),
        &mut edit.doc,
        &selection,
        input.intent.clone(),
        layout,
    ) {
        Ok(report) => report,
        Err(sch_floorplan::live::Error::BodyOverlap(overlaps)) => {
            timing.done("refused");
            return Ok(overlap_refusal(&overlaps, &warnings));
        }
        Err(error) => return Err(error.into()),
    };
    timing.done(if report.committed {
        "committed"
    } else {
        "refused"
    });
    Ok(with_warnings(
        selection_notes.finish(finish_arrangement(edit, report, ctx)?),
        &warnings,
    ))
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

fn finish_arrangement(mut edit: Edit, report: ArrangeReport, ctx: &AgentRuntime) -> Result<Value> {
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
    let refit = report.outlines_refit.clone();
    let retiled = match refit.is_empty() {
        true => None,
        false => crate::blocks::retile(&mut edit.doc, ctx, None),
    };
    let mut value = edit.commit(json!(report), allow)?;
    if value.get("error").is_some() {
        return Ok(value);
    }
    match retiled {
        Some(retiled) => value["retiled"] = retiled,
        None if !refit.is_empty() => {
            value["next"] = json!(format!(
                "the outline(s) of {} were redrawn around the moved parts; call arrange_blocks to tile the grid",
                refit.join(", ")
            ));
        }
        None => {}
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
                    at,
                } = &found
                {
                    edit.doc.add_label(
                        sch_doc::LabelKind::Local,
                        net,
                        crate::wiring::outward_pose(&edit.doc, *at),
                    );
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
    let report = match sch_floorplan::live::bench(ctx.env(), &mut edit.doc, payload, None, why)
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

/// The sheet's findings after the call. Missing-support gaps are left to
/// `check_schematic`: echoed on every placement they drove one-cap-per-call
/// churn, each cap then boxed alone.
fn with_check(mut value: Value, ctx: &AgentRuntime) -> Result<Value> {
    let mut check = crate::check::check_schematic(json!({}), ctx)?;
    if let Some(completeness) = check.get_mut("completeness").and_then(Value::as_object_mut) {
        completeness.remove("gaps");
    }
    value["gaps"] = json!([]);
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

#[cfg(test)]
mod block_size_tests {
    use sch_check::model::{Block, Component, Design};

    fn design_with(parts: usize) -> Design {
        let mut block = Block::default();
        for n in 0..parts {
            block
                .components
                .insert(format!("R{n}"), Component::default());
        }
        let mut design = Design::default();
        design.blocks.insert("everything".into(), block);
        design
    }

    #[test]
    fn a_section_of_a_dozen_parts_is_left_alone() {
        assert!(super::crowded_blocks(&design_with(12)).is_empty());
    }

    /// Two parts on one net are a signal path; two parts on none are a parts bin.
    #[test]
    fn neighbours_in_a_row_that_share_nothing_are_named() {
        use sch_check::model::PinTarget;
        use sch_model::tree::Tree;

        let mut block = Block::default();
        for (refdes, net) in [("R1", "SIG"), ("R2", "SIG"), ("R3", "OTHER")] {
            let mut part = Component::default();
            part.pins.insert("1".into(), PinTarget::Net(net.into()));
            block.components.insert(refdes.into(), part);
        }
        block.layout = Some(Tree::row_of([
            ("R1".to_string(), 1),
            ("R2".to_string(), 1),
            ("R3".to_string(), 1),
        ]));
        let mut design = Design::default();
        design.blocks.insert("chain".into(), block);

        let said = super::unrelated_neighbours(&design);
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(said[0].contains("`R2` and `R3`"), "{said:?}");
    }

    #[test]
    fn a_block_too_big_to_read_says_how_to_split_it() {
        let said = super::crowded_blocks(&design_with(24));
        assert_eq!(said.len(), 1);
        assert!(said[0].contains("holds 24 parts"), "{said:?}");
        assert!(
            said[0].contains("too many to read as one section"),
            "{said:?}"
        );
        assert!(said[0].contains("remove_region"), "{said:?}");
    }
}

/// How many block outlines the sheet draws, when it draws any.
fn outlined_blocks(ctx: &AgentRuntime) -> Option<usize> {
    let doc = sch_doc::SchDoc::read(ctx.sch_path()).ok()?;
    let count = doc
        .items()
        .iter()
        .filter(|item| matches!(item, sch_doc::Item::Rectangle(_)))
        .count();
    (count > 0).then_some(count)
}
