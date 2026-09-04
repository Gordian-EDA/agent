//! The symbol mutators: place, remove, move, retag and swap parts.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use geom::{EPS, Point2, Rect, Segment};
use gordian_runtime::AgentRuntime;
use sch_doc::{LabelKind, Pose, SchDoc, body_rect, placed_pins};
use serde::Serialize;
use serde_json::{Value, json};

use crate::place::{Occupancy, Side, snap, snap_point};
use crate::refs;
use crate::session::{Allow, Edit, symbol_source};
use crate::wiring::{spot_beside, spot_near};

#[derive(Debug)]
enum FootprintRepair {
    Keep,
    Resolve {
        from: String,
        to: String,
    },
    Clear {
        requested: String,
        did_you_mean: Vec<String>,
    },
}

#[derive(Debug, Serialize)]
struct ResolvedFootprint {
    #[serde(rename = "ref")]
    refdes: String,
    from: String,
    to: String,
}

#[derive(Debug, Serialize)]
struct UnresolvedFootprint {
    #[serde(rename = "ref")]
    refdes: String,
    requested: String,
    did_you_mean: Vec<String>,
}

/// Turn footprint lookup and pad compatibility into repairable metadata.
fn footprint_repair(
    ctx: &AgentRuntime,
    reference: &str,
    symbol: &str,
    requested: &str,
) -> Result<FootprintRepair> {
    footprint_repair_ignoring(ctx, reference, symbol, requested, &BTreeSet::new())
}

fn footprint_repair_ignoring(
    ctx: &AgentRuntime,
    reference: &str,
    symbol: &str,
    requested: &str,
    ignored_pins: &BTreeSet<String>,
) -> Result<FootprintRepair> {
    if let Some(did_you_mean) =
        gordian_runtime::footprint_compat::unresolved_footprint_suggestions(ctx, symbol, requested)?
    {
        let requested_library = requested.split_once(':').map(|(library, _)| library);
        let mut compatible = did_you_mean
            .iter()
            .filter(|candidate| {
                requested_library.is_some_and(|library| {
                    candidate
                        .split_once(':')
                        .is_some_and(|(candidate_library, _)| candidate_library == library)
                })
            })
            .filter(|candidate| {
                gordian_runtime::footprint_compat::footprint_compatibility_ignoring(
                    ctx,
                    symbol,
                    candidate,
                    ignored_pins,
                )
                .is_ok_and(|verdict| verdict.compatible)
            })
            .cloned()
            .collect::<Vec<_>>();
        compatible.sort();
        compatible.dedup();
        if compatible.len() == 1 {
            return Ok(FootprintRepair::Resolve {
                from: requested.to_owned(),
                to: compatible.remove(0),
            });
        }
        return Ok(FootprintRepair::Clear {
            requested: requested.to_owned(),
            did_you_mean,
        });
    }

    let Some(mismatch) = gordian_runtime::footprint_compat::assignment_pin_mismatch_ignoring(
        ctx,
        reference,
        symbol,
        requested,
        ignored_pins,
    )?
    else {
        return Ok(FootprintRepair::Keep);
    };
    if mismatch.suggestion_compatible
        && let Some(to) = mismatch.suggestion
    {
        return Ok(FootprintRepair::Resolve {
            from: requested.to_owned(),
            to,
        });
    }
    Ok(FootprintRepair::Clear {
        requested: requested.to_owned(),
        did_you_mean: mismatch.suggestion.into_iter().collect(),
    })
}

fn attach_footprint_repairs(
    value: &mut Value,
    resolved: &[ResolvedFootprint],
    unresolved: &[UnresolvedFootprint],
) {
    match resolved {
        [] => {}
        [resolution] => {
            value["footprint_resolved"] = json!({
                "from": resolution.from,
                "to": resolution.to,
            });
        }
        resolutions => value["footprints_resolved"] = json!(resolutions),
    }
    if unresolved.is_empty() {
        return;
    }
    value["footprints_unresolved"] = json!(unresolved);
    value["gaps"] = json!(
        unresolved
            .iter()
            .map(|issue| json!({
                "kind": "footprint_unresolved",
                "refdes": issue.refdes,
                "suggestion": format!(
                    "assign a compatible footprint to {} with assign_footprints",
                    issue.refdes
                ),
            }))
            .collect::<Vec<_>>()
    );
}

/// The next unused designator with this prefix, e.g. `R` → `R7`.
/// The lowest free designator with `prefix`, stepping over both the sheet's own
/// references and every one `reserve_refs` promised another caller.
pub(crate) fn next_refdes(edit: &Edit, prefix: &str) -> String {
    let taken: Vec<u32> = edit
        .doc
        .symbols()
        .filter_map(|s| s.refdes().strip_prefix(prefix)?.parse().ok())
        .chain(
            edit.reserved()
                .iter()
                .filter_map(|refdes| refdes.strip_prefix(prefix)?.parse().ok()),
        )
        .collect();
    let mut n = 1;
    while taken.contains(&n) {
        n += 1;
    }
    format!("{prefix}{n}")
}

fn normalized_pin_name(name: &str) -> String {
    name.chars()
        .filter(|ch| !matches!(ch, '~' | '_' | '-'))
        .flat_map(char::to_lowercase)
        .collect()
}

/// Whether two non-empty pin names match without presentation punctuation.
fn pin_names_match(left: &str, right: &str) -> bool {
    let left = normalized_pin_name(left);
    !left.is_empty() && left == normalized_pin_name(right)
}

fn valid_refdes(refdes: &str) -> bool {
    let refdes = refdes.strip_prefix('#').unwrap_or(refdes);
    let letters = refdes.chars().take_while(char::is_ascii_alphabetic).count();
    letters > 0
        && letters < refdes.len()
        && refdes[..letters].chars().all(|ch| ch.is_ascii_alphabetic())
        && refdes[letters..].chars().all(|ch| ch.is_ascii_digit())
}

fn swapped_field_collisions(doc: &SchDoc, refdes: &str) -> usize {
    sch_floorplan::visual::measure(doc)
        .text_collisions
        .iter()
        .filter(|collision| {
            collision.reference == refdes
                && matches!(collision.field.as_str(), "Reference" | "Value")
        })
        .count()
}

fn reflow_swapped_fields(doc: &mut SchDoc, refdes: &str, units: &[(u32, String)]) -> Result<()> {
    for (_, uuid) in units {
        let Some(symbol) = doc.symbol(uuid) else {
            continue;
        };
        let Some(body) = body_rect(doc, symbol) else {
            continue;
        };
        let fields = ["Reference", "Value"];
        let Some(original) = fields
            .iter()
            .map(|name| symbol.fields.get(*name)?.at)
            .collect::<Option<Vec<_>>>()
        else {
            continue;
        };
        let center = body.center();
        let candidates = [
            [
                Point2::new(center.x, body.min_y - 3.81),
                Point2::new(center.x, body.min_y - 2.03),
            ],
            [
                Point2::new(center.x, body.max_y + 2.03),
                Point2::new(center.x, body.max_y + 3.81),
            ],
            [
                Point2::new(body.min_x - 6.35, center.y - 1.27),
                Point2::new(body.min_x - 6.35, center.y + 1.27),
            ],
            [
                Point2::new(body.max_x + 6.35, center.y - 1.27),
                Point2::new(body.max_x + 6.35, center.y + 1.27),
            ],
            [
                Point2::new(center.x, body.min_y - 6.35),
                Point2::new(center.x, body.min_y - 4.57),
            ],
            [
                Point2::new(center.x, body.max_y + 4.57),
                Point2::new(center.x, body.max_y + 6.35),
            ],
        ];
        let mut best = (swapped_field_collisions(doc, refdes), original.clone());
        for candidate in candidates {
            for (name, point) in fields.iter().zip(candidate) {
                doc.set_field_pose(uuid, name, Pose::new(point.x, point.y, 0.0))?;
            }
            let collisions = swapped_field_collisions(doc, refdes);
            if collisions < best.0 {
                best = (
                    collisions,
                    candidate
                        .iter()
                        .map(|point| Pose::new(point.x, point.y, 0.0))
                        .collect(),
                );
            }
            if collisions == 0 {
                break;
            }
        }
        for ((name, _), pose) in fields.iter().zip(&original).zip(best.1) {
            doc.set_field_pose(uuid, name, pose)?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PinMatchKind {
    Explicit,
    Number,
    Name,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PinAssignment {
    old: usize,
    new: usize,
    kind: PinMatchKind,
}

#[derive(Debug)]
struct PinMappingPlan {
    assignments: Vec<PinAssignment>,
    old_without_counterpart: Vec<usize>,
    new_unassigned: Vec<usize>,
    /// `pin_map` entries whose target is not a pin of the new symbol, as
    /// `(old pin, requested target)`.
    unknown_targets: Vec<(String, String)>,
}

fn assign_pin(
    assignments: &mut Vec<PinAssignment>,
    claimed_old: &mut [bool],
    taken_new: &mut [bool],
    old: usize,
    new: usize,
    kind: PinMatchKind,
) {
    claimed_old[old] = true;
    taken_new[new] = true;
    assignments.push(PinAssignment { old, new, kind });
}

impl PinMappingPlan {
    fn mapped_by_name(
        &self,
        old_pins: &[sch_doc::PlacedPin],
        new_pins: &[sch_doc::PlacedPin],
    ) -> BTreeMap<String, String> {
        self.assignments
            .iter()
            .filter(|assignment| assignment.kind == PinMatchKind::Name)
            .map(|assignment| {
                (
                    old_pins[assignment.old].number.clone(),
                    new_pins[assignment.new].number.clone(),
                )
            })
            .collect()
    }

    fn suggested_pin_map(
        &self,
        old_pins: &[sch_doc::PlacedPin],
        new_pins: &[sch_doc::PlacedPin],
    ) -> BTreeMap<String, String> {
        self.assignments
            .iter()
            .filter(|assignment| assignment.kind != PinMatchKind::Number)
            .filter(|assignment| old_pins[assignment.old].number != new_pins[assignment.new].number)
            .map(|assignment| {
                (
                    old_pins[assignment.old].number.clone(),
                    new_pins[assignment.new].number.clone(),
                )
            })
            .collect()
    }
}

fn pin_mapping_plan(
    old_pins: &[sch_doc::PlacedPin],
    new_pins: &[sch_doc::PlacedPin],
    requested: Option<&serde_json::Map<String, Value>>,
) -> PinMappingPlan {
    let mut assignments = Vec::new();
    let mut unknown_targets = Vec::new();
    let mut claimed_old = vec![false; old_pins.len()];
    let mut taken_new = vec![false; new_pins.len()];

    for (old_index, old) in old_pins.iter().enumerate() {
        let wanted = requested
            .and_then(|map| map.get(&old.number).or_else(|| map.get(&old.name)))
            .and_then(Value::as_str);
        let Some(wanted) = wanted else {
            continue;
        };
        let target = new_pins
            .iter()
            .enumerate()
            .find(|(index, pin)| !taken_new[*index] && pin.number == wanted)
            .or_else(|| {
                new_pins
                    .iter()
                    .enumerate()
                    .find(|(index, pin)| !taken_new[*index] && pin_names_match(&pin.name, wanted))
            });
        match target {
            Some((new_index, _)) => assign_pin(
                &mut assignments,
                &mut claimed_old,
                &mut taken_new,
                old_index,
                new_index,
                PinMatchKind::Explicit,
            ),
            // Leaving the pin unclaimed lets the number and name passes still find it,
            // and records the real fault: the map named a pin the new symbol lacks.
            None => unknown_targets.push((old.number.clone(), wanted.to_string())),
        }
    }

    for (old_index, old) in old_pins.iter().enumerate() {
        if claimed_old[old_index] {
            continue;
        }
        if let Some((new_index, _)) = new_pins
            .iter()
            .enumerate()
            .find(|(index, pin)| !taken_new[*index] && pin.number == old.number)
        {
            assign_pin(
                &mut assignments,
                &mut claimed_old,
                &mut taken_new,
                old_index,
                new_index,
                PinMatchKind::Number,
            );
        }
    }

    for (old_index, old) in old_pins.iter().enumerate() {
        if claimed_old[old_index] {
            continue;
        }
        if let Some((new_index, _)) = new_pins
            .iter()
            .enumerate()
            .find(|(index, pin)| !taken_new[*index] && pin_names_match(&pin.name, &old.name))
        {
            assign_pin(
                &mut assignments,
                &mut claimed_old,
                &mut taken_new,
                old_index,
                new_index,
                PinMatchKind::Name,
            );
        }
    }

    let assigned_old = assignments
        .iter()
        .map(|assignment| assignment.old)
        .collect::<Vec<_>>();
    PinMappingPlan {
        old_without_counterpart: old_pins
            .iter()
            .enumerate()
            .filter_map(|(index, _)| (!assigned_old.contains(&index)).then_some(index))
            .collect(),
        new_unassigned: taken_new
            .iter()
            .enumerate()
            .filter_map(|(index, taken)| (!taken).then_some(index))
            .collect(),
        assignments,
        unknown_targets,
    }
}

fn pin_detail(pin: &sch_doc::PlacedPin) -> Value {
    json!({
        "number": pin.number,
        "name": pin.name,
        "type": pin.etype,
    })
}

fn swap_suggestion(
    plan: &PinMappingPlan,
    old_pins: &[sch_doc::PlacedPin],
    new_pins: &[sch_doc::PlacedPin],
) -> Value {
    json!({
        "pin_map": plan.suggested_pin_map(old_pins, new_pins),
        "old_pins_without_counterpart": plan.old_without_counterpart
            .iter().map(|index| pin_detail(&old_pins[*index])).collect::<Vec<_>>(),
        "new_symbol_unassigned_pins": plan.new_unassigned
            .iter().map(|index| pin_detail(&new_pins[*index])).collect::<Vec<_>>(),
    })
}

fn combined_extent(doc: &SchDoc, uuids: &[String]) -> Option<Rect> {
    let corners: Vec<Point2> = uuids
        .iter()
        .filter_map(|uuid| doc.symbol(uuid))
        .filter_map(|symbol| crate::place::extent(doc, symbol))
        .flat_map(|extent| {
            [
                Point2::new(extent.min_x, extent.min_y),
                Point2::new(extent.max_x, extent.max_y),
            ]
        })
        .collect();
    Rect::bounding(&corners)
}

fn stack_units(doc: &mut SchDoc, uuids: &[String]) -> Result<(), sch_doc::Error> {
    let mut previous_bottom = None;
    for uuid in uuids {
        let Some(symbol) = doc.symbol(uuid) else {
            continue;
        };
        let at = symbol.at;
        let Some(extent) = crate::place::extent(doc, symbol) else {
            continue;
        };
        if let Some(bottom) = previous_bottom {
            let y = at.y + bottom + crate::place::CLEARANCE - extent.min_y;
            doc.move_symbol(uuid, at.x, snap(y))?;
        }
        previous_bottom = doc
            .symbol(uuid)
            .and_then(|symbol| crate::place::extent(doc, symbol))
            .map(|extent| extent.max_y);
    }
    Ok(())
}

/// Where a part should end up, and which of its two anchors the answer is
/// about.
///
/// `to` names the symbol's own position — the one `read_schematic` prints, so
/// a coordinate read back and written out again lands where it started. A
/// free-space search instead yields where the part's *extent* should be
/// centred, which is the only way to reason about clearance.
enum Destination {
    Origin(Point2),
    Centre(Point2),
}

impl Destination {
    /// The symbol position this destination implies for a part whose extent is
    /// currently centred at `centre` while its origin sits at `origin`.
    fn origin_for(&self, origin: Point2, centre: Point2) -> Point2 {
        match self {
            Destination::Origin(at) => *at,
            Destination::Centre(at) => {
                Point2::new(origin.x + at.x - centre.x, origin.y + at.y - centre.y)
            }
        }
    }
}

fn destination(
    doc: &SchDoc,
    input: &Value,
    w: f64,
    h: f64,
    skip: &[String],
) -> Result<(Destination, Option<String>), String> {
    if let Some(at) = input.get("to").and_then(Value::as_array) {
        let n: Vec<f64> = at.iter().filter_map(Value::as_f64).collect();
        if n.len() != 2 {
            return Err("`to` must be [x, y] in mm".to_string());
        }
        return Ok((
            Destination::Origin(snap_point(Point2::new(n[0], n[1]))),
            None,
        ));
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
        let (at, on_side) =
            spot_beside(doc, anchor, side, w, h, skip).ok_or("no free space on the sheet")?;
        let note = (!on_side)
            .then(|| format!("nothing fits {side:?} {anchor}; used the nearest free spot"));
        return Ok((Destination::Centre(at), note));
    }
    let content = Occupancy::skipping(doc, skip).content();
    spot_near(
        doc,
        Point2::new(content.max_x + 12.7, content.center().y),
        w,
        h,
        skip,
    )
    .map(|at| (Destination::Centre(at), None))
    .ok_or_else(|| "no free space on the sheet".to_string())
}

/// Place one part and report where it landed.
///
/// The part is parked far off-sheet first: its extents are only knowable once
/// its definition is embedded, and both the orientation and the destination
/// depend on them.
fn place_one(
    edit: &mut Edit,
    spec: &Value,
    source: &sch_doc::SymbolSource,
) -> Result<Value, String> {
    let Some(lib_id) = spec.get("lib_id").and_then(Value::as_str) else {
        return Err("every part needs `lib_id` (e.g. Device:R)".to_string());
    };
    if spec.get("at").is_some() {
        return Err(
            "add_symbols chooses collision-free placement; use `near`+`side`, or use move_symbols({moves:[{ref,to}]}) only when the user explicitly requested coordinates"
                .to_string(),
        );
    }
    let value = spec.get("value").and_then(Value::as_str).unwrap_or("");
    if let Some(refdes) = spec.get("ref").and_then(Value::as_str)
        && edit.doc.symbol_by_ref(refdes).is_some()
    {
        return Err(format!("`{refdes}` is already on the sheet"));
    }
    if let Some(refdes) = spec.get("ref").and_then(Value::as_str)
        && !valid_refdes(refdes)
    {
        return Err(format!(
            "`{refdes}` is not a valid reference; use letters followed by digits, such as R12, U3, or #PWR01"
        ));
    }
    let park = Pose::new(5000.0, 5000.0, 0.0);
    let provisional = next_refdes(edit, "ZZ");
    let uuids = edit
        .doc
        .add_symbol(lib_id, &provisional, value, park, source)
        .map_err(|error| format!("could not place {lib_id}: {error}"))?;
    let refdes = match spec.get("ref").and_then(Value::as_str) {
        Some(refdes) => refdes.to_string(),
        None => {
            let prefix = edit
                .doc
                .reference_prefix(lib_id)
                .unwrap_or_else(|| "U".to_string());
            next_refdes(edit, &prefix)
        }
    };
    let fail = |error: sch_doc::Error| error.to_string();
    edit.doc.set_reference(&uuids, &refdes).map_err(fail)?;
    if let Some(footprint) = spec.get("footprint").and_then(Value::as_str) {
        for uuid in &uuids {
            edit.doc
                .set_field(uuid, "Footprint", footprint)
                .map_err(fail)?;
        }
    }
    let side = spec
        .get("side")
        .and_then(Value::as_str)
        .and_then(Side::parse);
    let rot = match spec.get("rot").and_then(Value::as_f64) {
        Some(rot) => {
            let rot = geom::snap_quadrant(rot);
            for uuid in &uuids {
                edit.doc
                    .set_symbol_orientation(uuid, rot, sch_doc::Mirror::None)
                    .map_err(fail)?;
            }
            rot
        }
        None if spec.get("near").is_some() && uuids.len() == 1 => {
            crate::place::facing_rotation(&mut edit.doc, &refdes, side.unwrap_or(Side::Right))
        }
        None => 0.0,
    };
    stack_units(&mut edit.doc, &uuids).map_err(fail)?;

    let body = combined_extent(&edit.doc, &uuids);
    let (w, h) = body.map_or((10.0, 10.0), |r| (r.width(), r.height()));
    let skip = uuids.clone();
    let (want, mut note) = destination(&edit.doc, spec, w, h, &skip)?;
    let centre = body.map_or(park.point(), |r| r.center());
    let mut at = snap_point(want.origin_for(park.point(), centre));
    // An explicit `at` is a request, not a licence to land on someone else's
    // drawing: slide clear, and say so.
    let offset = Point2::new(centre.x - park.x, centre.y - park.y);
    let landing = Point2::new(at.x + offset.x, at.y + offset.y);
    let occupancy = Occupancy::skipping(&edit.doc, &skip);
    if !occupancy.free(landing, w, h) {
        let free = occupancy
            .nearest_free(landing, w, h)
            .ok_or("no free space on the sheet")?;
        at = snap_point(Point2::new(free.x - offset.x, free.y - offset.y));
        note = Some(format!(
            "({:.2},{:.2}) was occupied; moved clear",
            landing.x, landing.y
        ));
    }
    let delta = Point2::new(at.x - park.x, at.y - park.y);
    let before_move = sch_drag::Sheet::of(&edit.doc);
    let moves = uuids
        .iter()
        .filter_map(|uuid| {
            let symbol = edit.doc.symbol(uuid)?;
            Some((
                uuid.clone(),
                sch_drag::Placement::new(
                    Point2::new(snap(symbol.at.x + delta.x), snap(symbol.at.y + delta.y)),
                    symbol.at.rot,
                    symbol.mirror,
                ),
            ))
        })
        .collect::<Vec<_>>();
    sch_drag::drag_many(&mut edit.doc, &moves, &before_move)
        .map_err(|error| format!("could not seat {refdes}: {error}"))?;

    let units: Vec<Value> = uuids
        .iter()
        .filter_map(|uuid| edit.doc.symbol(uuid))
        .map(|symbol| json!({ "unit": symbol.unit, "at": [symbol.at.x, symbol.at.y] }))
        .collect();
    let placed = units
        .first()
        .and_then(|unit| unit.get("at"))
        .cloned()
        .unwrap_or_else(|| json!([at.x, at.y]));
    let mut report = json!({
        "ref": refdes,
        "lib_id": lib_id,
        "at": placed,
        "units": units,
        "rot": rot,
    });
    if let Some(note) = note {
        report["note"] = json!(note);
    }
    Ok(report)
}

/// Place a block of new parts in one transaction, each clear of the last.
pub fn add_symbols(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let mut specs: Vec<Value> = input
        .get("parts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if specs.is_empty() {
        return Ok(json!({ "error": "add_symbols needs a non-empty `parts` list" }));
    }
    let mut footprint_repairs = Vec::new();
    for (index, spec) in specs.iter_mut().enumerate() {
        let Some(symbol) = spec.get("lib_id").and_then(Value::as_str) else {
            continue;
        };
        let Some(footprint) = spec.get("footprint").and_then(Value::as_str) else {
            continue;
        };
        let reference = spec.get("ref").and_then(Value::as_str).unwrap_or(symbol);
        let repair = footprint_repair(ctx, reference, symbol, footprint)?;
        match &repair {
            FootprintRepair::Keep => {}
            FootprintRepair::Resolve { to, .. } => {
                spec["footprint"] = json!(to);
            }
            FootprintRepair::Clear { .. } => {
                spec.as_object_mut()
                    .expect("an add_symbols part is an object")
                    .remove("footprint");
            }
        }
        footprint_repairs.push((index, repair));
    }
    let mut edit = Edit::open(ctx)?;
    let source = symbol_source(ctx);
    let mut allow = Allow::nothing().creating();
    let mut placed = Vec::new();
    for spec in &specs {
        match place_one(&mut edit, spec, &source) {
            Ok(report) => {
                if let Some(refdes) = report["ref"].as_str() {
                    allow = allow.part(refdes);
                }
                placed.push(report);
            }
            // A half-placed block is worse than none: report the first
            // failure and write nothing.
            Err(error) => return Ok(json!({ "error": error, "placed": placed })),
        }
    }
    let refs = placed
        .iter()
        .filter_map(|part| part["ref"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    let mut result = edit.commit(json!({ "placed": placed }), allow)?;
    if result.get("error").is_none() {
        crate::session::attach_connectivity(
            &mut result,
            ctx,
            refs.clone(),
            &format!("ADDED  {}", refs.join(" ")),
        )?;
        let mut resolved = Vec::new();
        let mut unresolved = Vec::new();
        for (index, repair) in footprint_repairs {
            let refdes = placed[index]["ref"]
                .as_str()
                .expect("a placed symbol has a reference")
                .to_owned();
            match repair {
                FootprintRepair::Keep => {}
                FootprintRepair::Resolve { from, to } => {
                    resolved.push(ResolvedFootprint { refdes, from, to });
                }
                FootprintRepair::Clear {
                    requested,
                    did_you_mean,
                } => unresolved.push(UnresolvedFootprint {
                    refdes,
                    requested,
                    did_you_mean,
                }),
            }
        }
        attach_footprint_repairs(&mut result, &resolved, &unresolved);
    }
    Ok(result)
}

#[derive(Debug, Default, Serialize)]
struct RemovedItems {
    symbols: usize,
    wires: usize,
    labels: usize,
    no_connects: usize,
    power: usize,
    junctions: usize,
    text: usize,
}

fn removed_items(before: &SchDoc, after: &SchDoc) -> RemovedItems {
    let symbols = before
        .symbols()
        .filter(|symbol| !symbol.refdes().starts_with('#') && after.symbol(&symbol.uuid).is_none())
        .count();
    let power = before
        .symbols()
        .filter(|symbol| symbol.refdes().starts_with('#') && after.symbol(&symbol.uuid).is_none())
        .count();
    let missing = |uuid: &str| {
        !after.items().iter().any(|item| match item {
            sch_doc::Item::Wire(item) => item.uuid == uuid,
            sch_doc::Item::Junction(item) => item.uuid == uuid,
            sch_doc::Item::NoConnect(item) => item.uuid == uuid,
            sch_doc::Item::Label(item) => item.uuid == uuid,
            sch_doc::Item::Text(item) => item.uuid == uuid,
            _ => false,
        })
    };
    let mut removed = RemovedItems {
        symbols,
        power,
        ..RemovedItems::default()
    };
    for item in before.items() {
        match item {
            sch_doc::Item::Wire(item) if missing(&item.uuid) => removed.wires += 1,
            sch_doc::Item::Label(item) if missing(&item.uuid) => removed.labels += 1,
            sch_doc::Item::NoConnect(item) if missing(&item.uuid) => removed.no_connects += 1,
            sch_doc::Item::Junction(item) if missing(&item.uuid) => removed.junctions += 1,
            sch_doc::Item::Text(item) if missing(&item.uuid) => removed.text += 1,
            _ => {}
        }
    }
    removed
}

/// What a caller's list of symbol names resolved to.
struct SymbolTargets {
    uuids: Vec<String>,
    missing: Vec<String>,
    /// Names read as a symbol other than what was written, reported so a caller sees
    /// which part it actually named.
    read_as: BTreeMap<String, String>,
}

/// Resolve caller-named symbols to uuids.
///
/// `"#PWR_GND_1.1"` is the PIN address every other tool result prints, and the model
/// reads one back and hands it here. It is read as its symbol only when the suffix is
/// really one of that symbol's pins — so `U1.VDD` names `U1` and a genuinely unknown
/// `U1.9` stays missing rather than deleting the part.
fn symbol_targets(doc: &SchDoc, targets: &[String]) -> SymbolTargets {
    let mut found = SymbolTargets {
        uuids: Vec::new(),
        missing: Vec::new(),
        read_as: BTreeMap::new(),
    };
    for target in targets {
        if let Some(symbol) = doc.symbol(target) {
            found.uuids.push(symbol.uuid.clone());
            continue;
        }
        let mut named = target.as_str();
        if !doc.symbols().any(|symbol| symbol.refdes() == named)
            && let Ok(pin) = refs::pin(doc, target)
        {
            found.read_as.insert(target.clone(), pin.refdes.clone());
            named = &target[..target.len() - pin.number.len() - 1];
        }
        let matched = doc
            .symbols()
            .filter(|symbol| symbol.refdes() == named)
            .map(|symbol| symbol.uuid.clone())
            .collect::<Vec<_>>();
        if matched.is_empty() {
            found.read_as.remove(target);
            found.missing.push(target.clone());
        } else {
            found.uuids.extend(matched);
        }
    }
    found.uuids.sort();
    found.uuids.dedup();
    found
}

/// The designators on the sheet closest to each name it does not carry, so a
/// refusal points somewhere instead of only saying no.
fn closest_refs(doc: &SchDoc, missing: &[String]) -> BTreeMap<String, Vec<String>> {
    let matcher = SkimMatcherV2::default().ignore_case();
    let on_sheet: Vec<&str> = doc
        .symbols()
        .map(sch_doc::SymbolInst::refdes)
        .filter(|refdes| !refdes.starts_with('#'))
        .collect();
    missing
        .iter()
        .filter_map(|name| {
            let mut ranked: Vec<(i64, &str)> = on_sheet
                .iter()
                .filter_map(|candidate| {
                    let score = matcher
                        .fuzzy_match(candidate, name)
                        .into_iter()
                        .chain(matcher.fuzzy_match(name, candidate))
                        .max()?;
                    Some((score, *candidate))
                })
                .collect();
            ranked.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(right.1)));
            ranked.truncate(3);
            let near: Vec<String> = ranked.into_iter().map(|(_, r)| r.to_string()).collect();
            (!near.is_empty()).then(|| (name.clone(), near))
        })
        .collect()
}

fn welded_flags(doc: &SchDoc, pins: &[Point2], removed_owners: &[String]) -> Vec<String> {
    let placed = placed_pins(doc);
    let flags = doc
        .symbols()
        .filter(|symbol| symbol.refdes().starts_with("#FLG"))
        .flat_map(|symbol| {
            placed
                .iter()
                .filter(|pin| pin.owner == symbol.uuid)
                .map(move |pin| (symbol.uuid.clone(), pin.at))
        })
        .collect::<Vec<_>>();
    let wires = doc
        .wires()
        .filter_map(|wire| {
            refs::ends(wire).map(|ends| (wire.uuid.clone(), Segment::new(ends.0, ends.1)))
        })
        .collect::<Vec<_>>();
    let junctions = doc.items().iter().filter_map(|item| match item {
        sch_doc::Item::Junction(junction) => Some(junction.at),
        _ => None,
    });
    let junctions = junctions.collect::<Vec<_>>();
    let component = |start: Point2| {
        let mut points = vec![start];
        let mut visited = BTreeSet::new();
        for _ in 0..64 {
            let mut grew = false;
            for (uuid, wire) in &wires {
                if visited.contains(uuid) {
                    continue;
                }
                let joins = points.iter().any(|point| {
                    wire.a.near_eq(*point, EPS)
                        || wire.b.near_eq(*point, EPS)
                        || (junctions.iter().any(|at| at.near_eq(*point, EPS))
                            && wire.contains_point(*point))
                });
                if !joins {
                    continue;
                }
                visited.insert(uuid.clone());
                points.extend([wire.a, wire.b]);
                points.extend(
                    junctions
                        .iter()
                        .filter(|point| wire.contains_point(**point)),
                );
                grew = true;
            }
            if !grew {
                break;
            }
        }
        points
    };
    let mut removed = Vec::new();
    for start in pins {
        let points = component(*start);
        let has_survivor = placed.iter().any(|pin| {
            !removed_owners.contains(&pin.owner)
                && !pin.refdes.starts_with("#")
                && points.iter().any(|point| point.near_eq(pin.at, EPS))
        });
        if has_survivor {
            continue;
        }
        removed.extend(
            flags
                .iter()
                .filter(|(_, flag_pin)| points.iter().any(|point| point.near_eq(*flag_pin, EPS)))
                .map(|(uuid, _)| uuid.clone()),
        );
    }
    removed.sort();
    removed.dedup();
    removed
}

/// Remove parts, together with the drawing that only served them.
pub fn remove_symbols(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let targets: Vec<String> = input
        .get("refs")
        .and_then(Value::as_array)
        .map(|v| {
            v.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if targets.is_empty() {
        return Ok(json!({ "error": "remove_symbols needs `refs`" }));
    }
    let mut edit = Edit::open(ctx)?;
    let original = edit.doc.clone();
    let SymbolTargets {
        mut uuids,
        missing,
        read_as,
    } = symbol_targets(&edit.doc, &targets);
    if uuids.is_empty() {
        return Ok(json!({
            "error": format!("not on the sheet: {}", missing.join(", ")),
            "missing": missing,
            "did_you_mean": closest_refs(&edit.doc, &missing),
        }));
    }

    let orphaned: Vec<Point2> = placed_pins(&edit.doc)
        .into_iter()
        .filter(|pin| uuids.contains(&pin.owner))
        .map(|p| p.at)
        .collect();
    uuids.extend(welded_flags(&edit.doc, &orphaned, &uuids));
    uuids.sort();
    uuids.dedup();
    let removed_refs = uuids
        .iter()
        .filter_map(|uuid| {
            edit.doc
                .symbol(uuid)
                .map(|symbol| symbol.refdes().to_string())
        })
        .collect::<Vec<_>>();
    let mut nets = refs::nets_touching(edit.before(), &removed_refs);
    let affected_pins = edit
        .before()
        .nets
        .iter()
        .filter(|net| nets.contains(&net.name))
        .flat_map(|net| net.pins.iter().cloned())
        .collect::<std::collections::BTreeSet<_>>();

    for uuid in &uuids {
        edit.doc.remove_symbol(uuid)?;
    }
    retract_stubs(&mut edit.doc, &orphaned);
    let floating = crate::wiring::floating_wires(&edit.doc);
    edit.doc.remove_drawing(&floating);
    let after = sch_doc::connect::extract(&edit.doc);
    nets.extend(
        after
            .nets
            .iter()
            .filter(|net| net.pins.iter().any(|pin| affected_pins.contains(pin)))
            .map(|net| net.name.clone()),
    );
    nets.sort();
    nets.dedup();
    let affected_refs = affected_pins
        .iter()
        .map(|pin| pin.refdes.clone())
        .collect::<Vec<_>>();
    let allow = Allow::nothing()
        .nets(nets)
        .parts(removed_refs)
        .parts(affected_refs);
    let loose = refs::newly_loose(edit.before(), &after);
    let removed = removed_items(&original, &edit.doc);
    edit.commit(
        json!({
            "removed": {
                "symbols": removed.symbols,
                "wires": removed.wires,
                "labels": removed.labels,
                "no_connects": removed.no_connects,
                "power": removed.power,
            },
            "now_loose": loose,
            "missing": missing,
            "read_as": read_as,
        }),
        allow,
    )
}

struct RegionSelection {
    bounds: Rect,
    block_uuids: Vec<String>,
    missing_block: Option<String>,
    blocks: Vec<String>,
}

fn region_bbox(input: &Value) -> std::result::Result<Option<Rect>, String> {
    let Some(values) = input.get("bbox") else {
        return Ok(None);
    };
    let coordinates = values
        .as_array()
        .map(|values| values.iter().filter_map(Value::as_f64).collect::<Vec<_>>())
        .unwrap_or_default();
    if coordinates.len() != 4 {
        return Err("`bbox` must be [x1, y1, x2, y2] in mm".to_string());
    }
    Ok(Some(Rect::from_points(
        Point2::new(coordinates[0], coordinates[1]),
        Point2::new(coordinates[2], coordinates[3]),
    )))
}

fn block_names(doc: &SchDoc) -> Vec<String> {
    let mut blocks = doc
        .symbols()
        .filter_map(|symbol| {
            symbol
                .fields
                .get(sch_model::result::AP_BLOCK)
                .map(|field| field.value.clone())
                .filter(|value| !value.is_empty())
        })
        .collect::<Vec<_>>();
    blocks.sort();
    blocks.dedup();
    blocks
}

fn region_bounds(input: &Value, doc: &SchDoc) -> std::result::Result<RegionSelection, Value> {
    let bbox = region_bbox(input).map_err(|error| json!({"error": error}))?;
    let block = input.get("block").and_then(Value::as_str);
    match (bbox, block) {
        (None, None) => Err(json!({
            "error": "remove_region needs `bbox`, `block`, or both",
        })),
        (Some(bounds), None) => Ok(RegionSelection {
            bounds,
            block_uuids: Vec::new(),
            missing_block: None,
            blocks: Vec::new(),
        }),
        (fallback, Some(block)) => {
            let blocks = block_names(doc);
            let uuids = doc
                .symbols()
                .filter(|symbol| {
                    symbol
                        .fields
                        .get(sch_model::result::AP_BLOCK)
                        .is_some_and(|field| field.value == block)
                })
                .map(|symbol| symbol.uuid.clone())
                .collect::<Vec<_>>();
            if uuids.is_empty() {
                return match fallback {
                    Some(bounds) => Ok(RegionSelection {
                        bounds,
                        block_uuids: Vec::new(),
                        missing_block: Some(block.to_string()),
                        blocks,
                    }),
                    None => Err(json!({
                        "error": format!("no symbols belong to block `{block}`"),
                        "blocks": blocks,
                    })),
                };
            }
            let bounds = combined_extent(doc, &uuids).or_else(|| {
                Rect::bounding(
                    &uuids
                        .iter()
                        .filter_map(|uuid| doc.symbol(uuid).map(|symbol| symbol.at.point()))
                        .collect::<Vec<_>>(),
                )
            });
            bounds
                .map(|bounds| RegionSelection {
                    bounds,
                    block_uuids: uuids,
                    missing_block: None,
                    blocks,
                })
                .ok_or_else(
                    || json!({"error": format!("block `{block}` has no measurable region")}),
                )
        }
    }
}

fn region_drawing(doc: &SchDoc, bounds: Rect) -> Vec<String> {
    doc.items()
        .iter()
        .filter_map(|item| match item {
            sch_doc::Item::Junction(item) if bounds.contains(item.at) => Some(item.uuid.clone()),
            sch_doc::Item::NoConnect(item) if bounds.contains(item.at) => Some(item.uuid.clone()),
            sch_doc::Item::Label(item) if bounds.contains(item.at.point()) => {
                Some(item.uuid.clone())
            }
            sch_doc::Item::Text(item) if bounds.contains(item.at.point()) => {
                Some(item.uuid.clone())
            }
            _ => None,
        })
        .collect()
}

fn merge_refusal(delta: &sch_doc::NetDelta) -> Option<String> {
    let (sources, target) = delta.merged.first()?;
    let mut nets = sources.clone();
    nets.push(target.clone());
    nets.sort();
    nets.dedup();
    Some(format!(
        "refused: removing that region would silently merge nets {}; nothing was written",
        nets.join(" and ")
    ))
}

/// Remove a rectangular or named functional region, cutting crossing wires at
/// its boundary and reporting the surviving ends.
pub fn remove_region(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let mut edit = Edit::open(ctx)?;
    let selection = match region_bounds(&input, &edit.doc) {
        Ok(selection) => selection,
        Err(error) => return Ok(error),
    };
    let bounds = selection.bounds;
    let block_uuids = selection.block_uuids;
    let original = edit.doc.clone();
    let old_scene = sch_doc::connect::scene(&edit.doc);
    let mut symbol_uuids = edit
        .doc
        .symbols()
        .filter(|symbol| bounds.contains(symbol.at.point()) || block_uuids.contains(&symbol.uuid))
        .map(|symbol| symbol.uuid.clone())
        .collect::<Vec<_>>();
    let removed_pins = placed_pins(&edit.doc)
        .into_iter()
        .filter(|pin| symbol_uuids.contains(&pin.owner))
        .collect::<Vec<_>>();
    let orphaned = removed_pins.iter().map(|pin| pin.at).collect::<Vec<_>>();
    symbol_uuids.extend(welded_flags(&edit.doc, &orphaned, &symbol_uuids));
    symbol_uuids.sort();
    symbol_uuids.dedup();
    let removed_refs = symbol_uuids
        .iter()
        .filter_map(|uuid| {
            edit.doc
                .symbol(uuid)
                .map(|symbol| symbol.refdes().to_string())
        })
        .collect::<Vec<_>>();
    let drawing = region_drawing(&edit.doc, bounds);
    edit.doc.remove_drawing(&drawing);
    for uuid in &symbol_uuids {
        edit.doc.remove_symbol(uuid)?;
    }
    let clipped = edit.doc.clip_wires_outside(bounds);
    retract_stubs(&mut edit.doc, &orphaned);

    let after = sch_doc::connect::extract(&edit.doc);
    let delta = sch_doc::Netlist::diff(edit.before(), &after);
    if let Some(error) = merge_refusal(&delta) {
        return Ok(json!({ "error": error }));
    }
    let surviving_cut = clipped
        .cut_points
        .iter()
        .filter(|point| {
            edit.doc.wires().any(|wire| {
                refs::ends(wire)
                    .is_some_and(|(a, b)| a.near_eq(**point, EPS) || b.near_eq(**point, EPS))
            })
        })
        .flat_map(|point| {
            old_scene
                .segments
                .iter()
                .filter(move |(a, b, _)| Segment::new(*a, *b).contains_point(*point))
                .map(move |(_, _, net)| (*point, net.clone()))
        })
        .collect::<Vec<_>>();
    let mut now_loose = Vec::new();
    for (point, net) in surviving_cut {
        if !now_loose.iter().any(|entry: &Value| {
            entry["net"] == net
                && entry["at"][0]
                    .as_f64()
                    .is_some_and(|x| (x - point.x).abs() <= EPS)
                && entry["at"][1]
                    .as_f64()
                    .is_some_and(|y| (y - point.y).abs() <= EPS)
        }) {
            now_loose.push(json!({"at": [point.x, point.y], "net": net}));
        }
    }
    let mut nets_lost_members = edit
        .before()
        .nets
        .iter()
        .filter(|net| {
            net.pins.iter().any(|member| {
                removed_pins.iter().any(|pin| {
                    pin.refdes == member.refdes
                        && pin.unit == member.unit
                        && pin.number == member.pin
                })
            })
        })
        .map(|net| net.name.clone())
        .chain(delta.removed.iter().cloned())
        .collect::<Vec<_>>();
    nets_lost_members.sort();
    nets_lost_members.dedup();
    let counts = removed_items(&original, &edit.doc);
    let all_nets = edit
        .before()
        .nets
        .iter()
        .chain(&after.nets)
        .map(|net| net.name.clone())
        .collect::<Vec<_>>();
    let all_refs = original
        .symbols()
        .map(|symbol| symbol.refdes().to_string())
        .chain(edit.doc.symbols().map(|symbol| symbol.refdes().to_string()))
        .chain(removed_refs)
        .collect::<BTreeSet<_>>();
    let mut changed = json!({
        "bbox": [bounds.min_x, bounds.min_y, bounds.max_x, bounds.max_y],
        "removed": counts,
        "now_loose": now_loose,
        "nets_lost_members": nets_lost_members,
    });
    if let Some(block) = selection.missing_block {
        changed["block_not_found"] = json!(block);
        changed["blocks"] = json!(selection.blocks);
    }
    edit.commit(
        changed,
        Allow::nothing()
            .joining_nets(all_nets)
            .parts(all_refs)
            .creating(),
    )
}

/// Retract the drawing that only served pins which no longer exist.
///
/// A removed pin leaves a wire run hanging in the air: KiCAD calls the free end
/// an `unconnected_wire_endpoint` error, and a run that reached only that pin
/// carries no connection any more. So the run is peeled back from the orphaned
/// endpoint up to the first thing that holds it — a live pin, a junction, or a
/// branch where another wire carries on — and the labels that sat on nothing but
/// the peeled run go with it. A label at the far end of a stub is *not* an
/// anchor: it is the other half of the same stub.
///
/// A no-connect marker on the orphaned pin goes too; with its pin gone it is
/// KiCAD's `no_connect_dangling`.
///
/// Returns how many items went.
pub(crate) fn retract_stubs(doc: &mut SchDoc, orphaned: &[Point2]) -> usize {
    // A power symbol is placed straight onto the pin it feeds, so a removed pin's
    // coordinate may still carry a pin that is very much there — and everything
    // hanging off it, its marker included, still belongs to that pin.
    let mut orphaned: Vec<Point2> = orphaned.to_vec();
    orphaned.retain(|p| !anchor_points(doc).iter().any(|q| q.near_eq(*p, EPS)));
    let markers: Vec<String> = doc
        .items()
        .iter()
        .filter_map(|item| match item {
            sch_doc::Item::NoConnect(marker)
                if orphaned.iter().any(|p| p.near_eq(marker.at, EPS)) =>
            {
                Some(marker.uuid.clone())
            }
            _ => None,
        })
        .collect();
    let mut removed = doc.remove_drawing(&markers);
    removed += remove_unheld_components(doc, &orphaned);
    let mut frontier: Vec<Point2> = orphaned.clone();
    let mut peeled: Vec<Point2> = orphaned;
    for _ in 0..64 {
        let live = anchor_points(doc);
        frontier.retain(|p| !live.iter().any(|q| q.near_eq(*p, EPS)));
        let runs: Vec<(String, Point2, Point2)> = doc
            .wires()
            .filter_map(|wire| refs::ends(wire).map(|(a, b)| (wire.uuid.clone(), a, b)))
            .collect();
        let degree = |p: Point2| {
            runs.iter()
                .filter(|(_, a, b)| a.near_eq(p, EPS) || b.near_eq(p, EPS))
                .count()
        };
        let is_junction = |p: Point2| {
            doc.items()
                .iter()
                .any(|item| matches!(item, sch_doc::Item::Junction(j) if j.at.near_eq(p, EPS)))
        };
        let mut doomed = Vec::new();
        let mut next = Vec::new();
        for &p in &frontier {
            if is_junction(p) || degree(p) > 1 {
                continue;
            }
            for (uuid, a, b) in &runs {
                let far = match (a.near_eq(p, EPS), b.near_eq(p, EPS)) {
                    (true, _) => *b,
                    (_, true) => *a,
                    _ => continue,
                };
                doomed.push(uuid.clone());
                next.push(far);
            }
        }
        if doomed.is_empty() {
            break;
        }
        // Runs that converge on one point would otherwise re-walk it once per
        // arrival, inflating the count and multiplying the frontier each round.
        next.retain(|p| !peeled.iter().any(|q| q.near_eq(*p, EPS)));
        next.dedup_by(|a, b| a.near_eq(*b, EPS));
        removed += doc.remove_drawing(&doomed);
        peeled.extend(&next);
        frontier = next;
    }
    removed + doc.remove_drawing(&stranded_labels(doc, &peeled))
}

/// Drawing components reached by removed pins and held by no surviving pin.
fn remove_unheld_components(doc: &mut SchDoc, orphaned: &[Point2]) -> usize {
    let runs = doc
        .wires()
        .filter_map(|wire| refs::ends(wire).map(|(a, b)| (wire.uuid.clone(), Segment::new(a, b))))
        .collect::<Vec<_>>();
    let mut groups = geom::UnionFind::new(runs.len());
    for (left, (_, one)) in runs.iter().enumerate() {
        for (right, (_, other)) in runs.iter().enumerate().skip(left + 1) {
            let junction_joins = doc.items().iter().any(|item| {
                matches!(item, sch_doc::Item::Junction(junction)
                    if one.contains_point(junction.at) && other.contains_point(junction.at))
            });
            if one.axis_aligned_connects(*other) || junction_joins {
                groups.union(left, right);
            }
        }
    }
    let roots = (0..runs.len())
        .map(|index| groups.find(index))
        .collect::<Vec<_>>();
    let pin_anchors = placed_pins(doc)
        .into_iter()
        .map(|pin| pin.at)
        .collect::<Vec<_>>();
    let sheet_anchors = doc
        .items()
        .iter()
        .filter_map(|item| match item {
            sch_doc::Item::Sheet(sheet) => Some(sheet.pins.iter().map(|pin| pin.at.point())),
            _ => None,
        })
        .flatten()
        .collect::<Vec<_>>();
    let held = runs
        .iter()
        .enumerate()
        .filter(|(_, (_, run))| {
            pin_anchors
                .iter()
                .any(|point| point.near_eq(run.a, EPS) || point.near_eq(run.b, EPS))
                || sheet_anchors.iter().any(|point| run.contains_point(*point))
        })
        .map(|(index, _)| roots[index])
        .collect::<BTreeSet<_>>();
    let touched = runs
        .iter()
        .enumerate()
        .filter(|(_, (_, run))| orphaned.iter().any(|point| run.contains_point(*point)))
        .map(|(index, _)| roots[index])
        .filter(|root| !held.contains(root))
        .collect::<BTreeSet<_>>();
    if touched.is_empty() {
        return 0;
    }
    let doomed_runs = runs
        .iter()
        .zip(&roots)
        .filter(|(_, root)| touched.contains(root))
        .map(|((uuid, _), _)| uuid.clone())
        .collect::<Vec<_>>();
    let on_doomed_run = |point: Point2| {
        runs.iter()
            .any(|(uuid, run)| doomed_runs.contains(uuid) && run.contains_point(point))
    };
    let drawing = doc
        .items()
        .iter()
        .filter_map(|item| match item {
            sch_doc::Item::Label(label) if on_doomed_run(label.at.point()) => {
                Some(label.uuid.clone())
            }
            sch_doc::Item::Junction(junction) if on_doomed_run(junction.at) => {
                Some(junction.uuid.clone())
            }
            sch_doc::Item::NoConnect(marker) if on_doomed_run(marker.at) => {
                Some(marker.uuid.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut doomed = doomed_runs;
    doomed.extend(drawing);
    doc.remove_drawing(&doomed)
}

/// Every point the sheet holds a connection at: symbol pins and the pins of a
/// hierarchical sheet symbol.
///
/// A sheet pin is a connection point that no symbol owns, so a retraction that
/// only knew about `placed_pins` would peel a run straight through one and cut
/// the child sheet loose.
fn anchor_points(doc: &SchDoc) -> Vec<Point2> {
    let sheet_pins = doc.items().iter().filter_map(|item| match item {
        sch_doc::Item::Sheet(sheet) => Some(sheet.pins.iter().map(|pin| pin.at.point())),
        _ => None,
    });
    placed_pins(doc)
        .into_iter()
        .map(|pin| pin.at)
        .chain(sheet_pins.flatten())
        .collect()
}

/// The labels at `points` that no longer sit on any wire or pin.
///
/// A stub's label lives at the far end of its run, so it only becomes stranded
/// once the run is gone — which is why this is a sweep over everything the
/// retraction peeled, run after the peeling rather than during it.
fn stranded_labels(doc: &SchDoc, points: &[Point2]) -> Vec<String> {
    let pins = anchor_points(doc);
    let held = |p: Point2| {
        pins.iter().any(|q| q.near_eq(p, EPS))
            || doc
                .wires()
                .filter_map(refs::ends)
                .any(|(a, b)| geom::Segment::new(a, b).contains_point(p))
    };
    doc.labels()
        .filter(|label| {
            let at = label.at.point();
            points.iter().any(|q| q.near_eq(at, EPS)) && !held(at)
        })
        .map(|label| label.uuid.clone())
        .collect()
}

/// How far a move may slide to clear an obstacle and still be the move that
/// was asked for: 20 grid steps, about 25 mm.
const NUDGE_RINGS: i32 = 20;

/// Apply a `rot`/`mirror` to a symbol, returning the rotation it now carries.
///
/// A drag may turn a part to meet its connections without replacing it.
fn orient(doc: &mut SchDoc, uuid: &str, step: &Value) -> Result<Option<f64>> {
    let rot = step.get("rot").and_then(Value::as_f64);
    let mirror = step.get("mirror").and_then(Value::as_str);
    if rot.is_none() && mirror.is_none() {
        return Ok(None);
    }
    let current = doc
        .symbol(uuid)
        .map_or((0.0, sch_doc::Mirror::None), |s| (s.at.rot, s.mirror));
    let mirror = match mirror {
        Some("x") => sch_doc::Mirror::X,
        Some("y") => sch_doc::Mirror::Y,
        Some(_) => sch_doc::Mirror::None,
        None => current.1,
    };
    let rot = rot.map_or(current.0, geom::snap_quadrant);
    doc.set_symbol_orientation(uuid, rot, mirror)?;
    Ok(Some(rot))
}

/// `1, 2` — the unit numbers of a multi-unit part, for an error message.
fn list(units: &[(u32, String)]) -> String {
    units
        .iter()
        .map(|(unit, _)| unit.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Move parts, sliding clear of anything already in the way.
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
    let mut planning = edit.doc.clone();
    let all: Vec<String> = moves
        .iter()
        .filter_map(|m| m.get("ref").and_then(Value::as_str).map(str::to_string))
        .collect();
    if all.len() != moves.len() {
        return Ok(json!({ "error": "every move needs a `ref`" }));
    }
    let mut placed = Vec::new();
    let mut drag_moves = Vec::new();
    let mut turn_moves = Vec::new();
    let mut post_drag_turns = Vec::new();
    // A part still waiting its turn is not an obstacle to the one being placed;
    // one already placed in this batch is.
    let mut pending: Vec<String> = moves
        .iter()
        .filter_map(|step| {
            let refdes = step.get("ref")?.as_str()?;
            let units = refs::units(&planning, refdes);
            let wanted = step.get("unit").and_then(Value::as_u64);
            match (units.as_slice(), wanted) {
                ([(_, uuid)], _) => Some(uuid.clone()),
                (many, Some(unit)) => many
                    .iter()
                    .find(|(candidate, _)| u64::from(*candidate) == unit)
                    .map(|(_, uuid)| uuid.clone()),
                _ => None,
            }
        })
        .collect();
    for step in &moves {
        let refdes = step["ref"].as_str().unwrap_or_default().to_string();
        if ["to", "by", "near", "rot", "mirror"]
            .iter()
            .all(|key| step.get(key).is_none())
        {
            return Ok(json!({
                "error": format!("the move of {refdes} does nothing — give `to`, `by`, `near`+`side`, `rot` or `mirror`"),
            }));
        }
        // The units of one part sit in different places, so a move — unlike a
        // value or a swap — has to say which one it means.
        let units = refs::units(&planning, &refdes);
        let wanted = step.get("unit").and_then(Value::as_u64);
        let uuid = match (units.as_slice(), wanted) {
            ([], _) => {
                return Ok(json!({ "error": format!("no symbol `{refdes}` on the sheet") }));
            }
            ([(_, uuid)], _) => uuid.clone(),
            (many, Some(want)) => match many.iter().find(|(unit, _)| u64::from(*unit) == want) {
                Some((_, uuid)) => uuid.clone(),
                None => {
                    return Ok(json!({
                        "error": format!("{refdes} has no unit {want}; it has {}", list(many)),
                    }));
                }
            },
            (many, None) => {
                return Ok(json!({
                    "error": format!(
                        "{refdes} is a {}-unit part whose units sit apart; add `unit` to say \
                         which one to move — it has {}",
                        many.len(), list(many)
                    ),
                }));
            }
        };
        if planning.symbol(&uuid).is_none() {
            return Ok(json!({ "error": format!("no symbol `{refdes}` on the sheet") }));
        }
        let staying = ["to", "by", "near"]
            .iter()
            .all(|key| step.get(key).is_none());
        let explicit_turn = step.get("turn_in_place").and_then(Value::as_bool) == Some(true);
        let before_pose = planning
            .symbol(&uuid)
            .map(|symbol| sch_drag::Placement::new(symbol.at.point(), symbol.at.rot, symbol.mirror))
            .expect("the symbol was just resolved");
        let turn_target = if staying {
            let mut trial = planning.clone();
            let turned = orient(&mut trial, &uuid, step)?;
            trial.symbol(&uuid).map(|symbol| {
                (
                    turned,
                    sch_drag::Placement::new(symbol.at.point(), symbol.at.rot, symbol.mirror),
                )
            })
        } else {
            None
        };
        let (turned, turn_in_place) = match turn_target {
            Some((turned, target)) => match sch_drag::turn_in_place(&mut planning, &uuid, target) {
                Ok(_) => (turned, Some(target)),
                Err(error) if explicit_turn => {
                    return Ok(json!({
                        "error": format!(
                            "turn_in_place for {refdes} was refused ({error}); nothing was moved"
                        ),
                    }));
                }
                Err(_) => (orient(&mut planning, &uuid, step)?, None),
            },
            None => (orient(&mut planning, &uuid, step)?, None),
        };
        let symbol = match planning.symbol(&uuid) {
            Some(symbol) => symbol,
            None => return Ok(json!({ "error": format!("no symbol `{refdes}` on the sheet") })),
        };
        let origin = symbol.at.point();
        let body = crate::place::extent(&planning, symbol);
        let (w, h) = body.map_or((10.0, 10.0), |r| (r.width(), r.height()));
        let centre = body.map_or(origin, |r| r.center());
        let want = if staying {
            Destination::Origin(origin)
        } else if let Some(by) = step.get("by").and_then(Value::as_array) {
            let n: Vec<f64> = by.iter().filter_map(Value::as_f64).collect();
            if n.len() != 2 {
                return Ok(json!({ "error": "`by` must be [dx, dy] in mm" }));
            }
            Destination::Centre(Point2::new(centre.x + n[0], centre.y + n[1]))
        } else {
            match destination(&planning, step, w, h, &pending) {
                Ok((want, _)) => want,
                Err(error) => return Ok(json!({ "error": error })),
            }
        };
        let mut at = if turn_in_place.is_some() {
            origin
        } else {
            snap_point(want.origin_for(origin, centre))
        };
        // Clearance is about the extent, which sits `centre - origin` away.
        let offset = Point2::new(centre.x - origin.x, centre.y - origin.y);
        let landing = Point2::new(at.x + offset.x, at.y + offset.y);
        let occupancy = Occupancy::skipping(&planning, &pending);
        let mut nudge = None;
        if turn_in_place.is_none() && !occupancy.free(landing, w, h) {
            // The spot the caller picked is taken, but the intent — put this
            // part about here — still holds: slide to the nearest grid spot
            // that fits and say where it went.
            let Some(free) = occupancy.nearest_free_within(landing, w, h, NUDGE_RINGS) else {
                return Ok(json!({
                    "error": format!(
                        "nothing within {:.0} mm of ({:.2},{:.2}) has room for {refdes}; \
                         nothing was moved",
                        NUDGE_RINGS as f64 * 1.27, at.x, at.y
                    ),
                }));
            };
            at = snap_point(Point2::new(free.x - offset.x, free.y - offset.y));
            nudge = Some([at.x, at.y]);
        }
        planning.move_symbol(&uuid, at.x, at.y)?;
        let symbol = planning
            .symbol(&uuid)
            .expect("a planned move keeps its symbol");
        let target = sch_drag::Placement::new(symbol.at.point(), symbol.at.rot, symbol.mirror);
        if turn_in_place.is_some() {
            turn_moves.push((placed.len(), uuid.clone(), refdes.clone(), target));
        } else if explicit_turn {
            drag_moves.push((
                uuid.clone(),
                sch_drag::Placement::new(target.at, before_pose.rot, before_pose.mirror),
            ));
            post_drag_turns.push((placed.len(), uuid.clone(), refdes.clone(), target));
        } else {
            drag_moves.push((uuid.clone(), target));
        }
        pending.retain(|pending_uuid| *pending_uuid != uuid);
        let mut report = json!({ "ref": refdes, "at": [at.x, at.y] });
        if let Some(to) = nudge {
            report["nudged_to"] = json!(to);
        }
        if let Some(rot) = turned {
            report["rot"] = json!(rot);
        }
        placed.push(report);
    }
    let moved: Vec<String> = placed
        .iter()
        .filter_map(|entry| entry.get("ref").and_then(Value::as_str))
        .map(str::to_owned)
        .collect();
    let touched = refs::nets_touching(edit.before(), &moved);
    let mut allow = Allow::nothing().nets(touched);
    for (index, uuid, refdes, target) in turn_moves {
        let turn = sch_drag::turn_in_place(&mut edit.doc, &uuid, target).map_err(|error| {
            anyhow::anyhow!("planned turn_in_place for {refdes} failed: {error}")
        })?;
        let nets: Vec<String> = turn
            .pins_swapped
            .iter()
            .map(|(_, net)| net.clone())
            .collect();
        allow = allow.joining_nets(nets).part(refdes);
        placed[index]["turned_in_place"] = json!(true);
        placed[index]["pins_swapped"] = json!(turn.pins_swapped);
    }
    let before = sch_drag::Sheet::of(&edit.doc);
    let drag = match sch_drag::drag_many(&mut edit.doc, &drag_moves, &before) {
        Ok((report, _)) => report,
        Err(error) => {
            let symbols = all.join(", ");
            let detail = match error {
                sch_drag::DragError::Truthfulness(nets) => format!(
                    "dragging {symbols} would change net{} {}; try a small 1.27 mm nudge away from other pins or wires",
                    if nets.len() == 1 { "" } else { "s" },
                    nets.join(", ")
                ),
                other => format!(
                    "dragging {symbols} was refused ({other}); try a small 1.27 mm nudge away from other pins or wires"
                ),
            };
            return Ok(json!({ "error": format!("refused: {detail}; nothing was moved") }));
        }
    };
    for (index, uuid, refdes, target) in post_drag_turns {
        let turn = sch_drag::turn_in_place(&mut edit.doc, &uuid, target).map_err(|error| {
            anyhow::anyhow!("planned turn_in_place for {refdes} failed after its drag: {error}")
        })?;
        let nets = turn
            .pins_swapped
            .iter()
            .map(|(_, net)| net.clone())
            .collect::<Vec<_>>();
        allow = allow.joining_nets(nets).part(refdes);
        placed[index]["dragged"] = json!(true);
        placed[index]["turned_in_place"] = json!(true);
        placed[index]["pins_swapped"] = json!(turn.pins_swapped);
    }
    let placement = if drag.labels_added == 0 && drag.crossings_added == 0 {
        "connections preserved as clean wire routes; any nudged_to coordinate is final"
    } else {
        "connections preserved; review labels_added/crossings_added and batch-nudge the moved parts if either is nonzero"
    };
    edit.commit(
        json!({
            "moved": placed,
            "redrawn_segments": drag.redrawn_segments,
            "labels_added": drag.labels_added,
            "crossings_added": drag.crossings_added,
            "placement": placement,
        }),
        allow,
    )
}

/// Set or clear a part's properties.
pub fn set_fields(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let (Some(refdes), Some(fields)) = (
        input.get("ref").and_then(Value::as_str),
        input.get("fields").and_then(Value::as_object),
    ) else {
        return Ok(json!({ "error": "set_fields needs `ref` and `fields`" }));
    };
    if fields
        .keys()
        .any(|name| name.eq_ignore_ascii_case("Footprint"))
    {
        return Ok(json!({
            "error": "set_fields does not set Footprint; use assign_footprints so symbol compatibility is validated",
        }));
    }
    let mut edit = Edit::open(ctx)?;
    // Address the symbol by UUID: setting `Reference` renames it, and every
    // later field in the same call would then be looking for a part that is
    // no longer there. Every unit gets the value — KiCAD shares one property
    // table across the halves of a part.
    let units = refs::units(&edit.doc, refdes);
    if units.is_empty() {
        return Ok(json!({ "error": format!("no symbol `{refdes}` on the sheet") }));
    }
    let renamed_to = fields.get("Reference").and_then(Value::as_str);
    if let Some(new_refdes) = renamed_to
        && new_refdes != refdes
    {
        let clashes = refs::units(&edit.doc, new_refdes);
        if !clashes.is_empty() {
            let units = clashes
                .iter()
                .map(|(unit, _)| unit.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            return Ok(json!({
                "error": format!(
                    "reference clash: cannot rename {refdes} to {new_refdes}; {new_refdes} is already used by unit(s) {units}; nothing was written"
                ),
            }));
        }
    }
    let was = refs::nets_touching(edit.before(), &[refdes.to_string()]);
    let mut allow = Allow::nothing().nets(was).part(refdes);
    if let Some(new) = fields.get("Reference").and_then(Value::as_str) {
        allow = allow.part(new);
    }
    let mut applied = serde_json::Map::new();
    for (name, value) in fields {
        let text = match value {
            Value::Null => "",
            other => other.as_str().unwrap_or_default(),
        };
        if name == "Reference" {
            let uuids: Vec<String> = units.iter().map(|(_, uuid)| uuid.clone()).collect();
            edit.doc.set_reference(&uuids, text)?;
        } else {
            for (_, uuid) in &units {
                edit.doc.set_field(uuid, name, text)?;
            }
        }
        applied.insert(name.clone(), json!(text));
    }
    edit.commit(
        json!({ "ref": refdes, "units": units.len(), "fields": applied }),
        allow,
    )
}

/// Set a validated batch of footprint fields without changing connectivity.
pub fn assign_footprints(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let Some(assignments) = input.get("assignments").and_then(Value::as_array) else {
        return Ok(json!({ "error": "assign_footprints needs a non-empty `assignments` array" }));
    };
    if assignments.is_empty() {
        return Ok(json!({ "error": "assign_footprints needs a non-empty `assignments` array" }));
    }
    let mut requested = Vec::with_capacity(assignments.len());
    for assignment in assignments {
        let (Some(reference), Some(footprint)) = (
            assignment.get("reference").and_then(Value::as_str),
            assignment.get("footprint").and_then(Value::as_str),
        ) else {
            return Ok(json!({ "error": "every assignment needs `reference` and `footprint`" }));
        };
        requested.push((reference.to_string(), footprint.to_string()));
    }

    let mut edit = Edit::open(ctx)?;
    for (reference, _) in &requested {
        let units = refs::units(&edit.doc, reference);
        if units.is_empty() {
            return Ok(json!({ "error": format!("no symbol `{reference}` on the sheet") }));
        }
        if units.iter().any(|(_, uuid)| {
            edit.doc
                .symbols()
                .find(|symbol| symbol.uuid == *uuid)
                .is_some_and(|symbol| symbol.dnp || symbol.refdes().starts_with('#'))
        }) {
            return Ok(json!({
                "error": format!("{reference} is virtual or DNP and cannot receive a footprint")
            }));
        }
    }
    let mut assigned = Vec::new();
    let mut resolved = Vec::new();
    let mut unresolved = Vec::new();
    for (reference, footprint) in &requested {
        let units = refs::units(&edit.doc, reference);
        let symbol = units
            .first()
            .and_then(|(_, uuid)| edit.doc.symbol(uuid))
            .map(|symbol| symbol.lib_id.clone())
            .expect("validated schematic reference has a symbol");
        let ignored_pins: std::collections::BTreeSet<String> = edit
            .before()
            .no_connect
            .iter()
            .filter(|pin| pin.refdes == *reference)
            .map(|pin| pin.pin.clone())
            .collect();
        let installed = ctx
            .provider()
            .symbol(&symbol)
            .or_else(|| ctx.index().ok()?.symbol(&symbol))
            .is_some();
        let repair = if installed {
            footprint_repair_ignoring(ctx, reference, &symbol, footprint, &ignored_pins)?
        } else {
            let pin_numbers = placed_pins(&edit.doc)
                .into_iter()
                .filter(|pin| pin.refdes == *reference)
                .map(|pin| pin.number)
                .collect::<Vec<_>>();
            if pin_numbers.is_empty() {
                return Ok(json!({
                    "error": format!(
                        "unknown symbol `{symbol}` and its embedded schematic definition has no pins"
                    ),
                }));
            }
            match gordian_runtime::footprint_compat::footprint_compatibility_for_pins(
                ctx,
                &symbol,
                pin_numbers.iter().map(String::as_str),
                footprint,
            ) {
                Ok(verdict) if verdict.compatible => FootprintRepair::Keep,
                Ok(_) | Err(_) => FootprintRepair::Clear {
                    requested: footprint.clone(),
                    did_you_mean: Vec::new(),
                },
            }
        };
        let selected = match repair {
            FootprintRepair::Keep => footprint.clone(),
            FootprintRepair::Resolve { from, to } => {
                resolved.push(ResolvedFootprint {
                    refdes: reference.clone(),
                    from,
                    to: to.clone(),
                });
                to
            }
            FootprintRepair::Clear {
                requested,
                did_you_mean,
            } => {
                unresolved.push(UnresolvedFootprint {
                    refdes: reference.clone(),
                    requested,
                    did_you_mean,
                });
                String::new()
            }
        };
        for (_, uuid) in refs::units(&edit.doc, reference) {
            edit.doc.set_field(&uuid, "Footprint", &selected)?;
        }
        if !selected.is_empty() {
            assigned.push(json!({ "reference": reference, "footprint": selected }));
        }
    }
    let mut result = edit.commit(json!({ "assigned": assigned }), Allow::nothing())?;
    if result.get("error").is_none() {
        attach_footprint_repairs(&mut result, &resolved, &unresolved);
    }
    Ok(result)
}

/// Set a part's build attributes.
pub fn set_flags(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let Some(refdes) = input.get("ref").and_then(Value::as_str) else {
        return Ok(json!({ "error": "set_flags needs `ref`" }));
    };
    let mut edit = Edit::open(ctx)?;
    let units = refs::units(&edit.doc, refdes);
    if units.is_empty() {
        return Ok(json!({ "error": format!("no symbol `{refdes}` on the sheet") }));
    }
    let dnp = input.get("dnp").and_then(Value::as_bool);
    let in_bom = input.get("in_bom").and_then(Value::as_bool);
    if dnp.is_none() && in_bom.is_none() {
        return Ok(json!({ "error": "set_flags needs `dnp` and/or `in_bom`" }));
    }
    for (_, uuid) in &units {
        edit.doc.set_flags(uuid, dnp, in_bom)?;
    }
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
/// A label on `pin`, turned so its text reads AWAY from the symbol body.
///
/// KiCAD draws label text along the label's angle, so a label left at angle 0
/// on a west-facing pin runs straight back over the part's own pin names.
fn outward_label(pin: &sch_doc::PlacedPin) -> Pose {
    let angle = if pin.out.x.abs() > pin.out.y.abs() {
        if pin.out.x > 0.0 { 0.0 } else { 180.0 }
    } else if pin.out.y < 0.0 {
        90.0
    } else {
        270.0
    };
    Pose::new(pin.at.x, pin.at.y, angle)
}

pub fn swap_symbol(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let (Some(refdes), Some(lib_id)) = (
        input.get("ref").and_then(Value::as_str),
        input.get("lib_id").and_then(Value::as_str),
    ) else {
        return Ok(json!({ "error": "swap_symbol needs `ref` and `lib_id`" }));
    };
    let mut edit = Edit::open(ctx)?;
    // Every unit of the part moves together: half an ECC83 and half an ECC82
    // is not a part.
    let units = refs::units(&edit.doc, refdes);
    if units.is_empty() {
        return Ok(json!({ "error": format!("no symbol `{refdes}` on the sheet") }));
    }
    let old_pins: Vec<sch_doc::PlacedPin> = placed_pins(&edit.doc)
        .into_iter()
        .filter(|p| p.refdes == refdes)
        .collect();
    let before_sheet = sch_drag::Sheet::of(&edit.doc);
    // Where each connected pin sat, so whatever met it can follow it across.
    let before: Vec<(String, String, Point2)> = old_pins
        .iter()
        .filter_map(|p| {
            let net = refs::net_of(edit.before(), refdes, &p.number)?;
            Some((p.number.clone(), net.to_string(), p.at))
        })
        .collect();
    let was = refs::nets_touching(edit.before(), &[refdes.to_string()]);

    let source = symbol_source(ctx);
    let mut dropped: Vec<String> = Vec::new();
    for (_, uuid) in &units {
        match edit.doc.set_lib_id(uuid, lib_id, &source) {
            Ok(lost) => dropped.extend(lost),
            Err(_) => {
                return Ok(json!({
                    "error": format!(
                        "no symbol `{lib_id}` exists in any library; if the replacement is electrically the same part, use `set_fields({{ref, fields:{{Value:…}}}})` instead of swapping to an unrelated same-numbered symbol"
                    ),
                }));
            }
        }
    }
    dropped.sort_unstable();
    dropped.dedup();
    // A definition with fewer units than the part uses would leave a unit with
    // no pins at all — an invisible amputation, so refuse it outright.
    let now = placed_pins(&edit.doc);
    let missing: Vec<String> = units
        .iter()
        .filter(|(_, uuid)| !now.iter().any(|p| p.owner == *uuid))
        .map(|(unit, _)| unit.to_string())
        .collect();
    if !missing.is_empty() {
        return Ok(json!({
            "error": format!(
                "{lib_id} has no unit {}; {refdes} is a {}-unit part and every unit must swap \
                 together — pick a symbol with at least {} units",
                missing.join(", "), units.len(), units.len()
            ),
        }));
    }
    let new_pins: Vec<sch_doc::PlacedPin> = now
        .iter()
        .filter(|pin| pin.refdes == refdes)
        .cloned()
        .collect();
    let plan = pin_mapping_plan(
        &old_pins,
        &new_pins,
        input.get("pin_map").and_then(Value::as_object),
    );
    let suggestion = swap_suggestion(&plan, &old_pins, &new_pins);
    if !plan.unknown_targets.is_empty() {
        let named = plan
            .unknown_targets
            .iter()
            .map(|(from, to)| format!("`{to}` (for {refdes}.{from})"))
            .collect::<Vec<_>>();
        return Ok(json!({
            "error": format!(
                "refused: pin_map names {} , which {lib_id} does not have; nothing was written",
                named.join(", ")
            ),
            "new_pins": new_pins
                .iter()
                .map(|pin| pin.number.clone())
                .collect::<Vec<_>>(),
        }));
    }
    // Only a pin carrying a net can be lost by a swap. Refusing over unwired pins
    // made every narrowing swap impossible — the case that had an agent cycle
    // through five connector symbols and never find the one that fits.
    let wired: std::collections::HashSet<&str> = before
        .iter()
        .map(|(number, _, _)| number.as_str())
        .collect();
    let orphaned: Vec<usize> = plan
        .old_without_counterpart
        .iter()
        .copied()
        .filter(|index| wired.contains(old_pins[*index].number.as_str()))
        .collect();
    if !orphaned.is_empty() {
        let unmatched = orphaned
            .iter()
            .map(|index| match old_pins[*index].name.as_str() {
                "" | "~" => old_pins[*index].number.clone(),
                name => format!("{} ({name})", old_pins[*index].number),
            })
            .collect::<Vec<_>>();
        return Ok(json!({
            "error": format!(
                "refused: {lib_id} has no counterpart for {refdes} pin(s) {}, which carry nets; \
                 nothing was written. Unwired pins would have been dropped silently; name a \
                 target for these in `pin_map`, or disconnect them first",
                unmatched.join(", ")
            ),
            "suggestion": suggestion,
        }));
    }
    // A definition that brings supply pins the old part did not have is not the
    // pin-compatible replacement a swap claims to be: nothing on the sheet drives them
    // and KiCAD calls every one of them an error. A pin that keeps its number but
    // turns into a supply pin is the same defect wearing the connectivity guard's
    // clothes — the net partition is untouched while KiCAD gains an ERC error.
    let added_supplies: Vec<String> = plan
        .new_unassigned
        .iter()
        .map(|index| &new_pins[*index])
        .filter(|pin| pin.etype == "power_in")
        .chain(
            plan.assignments
                .iter()
                .filter(|a| {
                    new_pins[a.new].etype == "power_in" && old_pins[a.old].etype != "power_in"
                })
                .map(|a| &new_pins[a.new]),
        )
        .map(|p| match p.name.as_str() {
            "" | "~" => p.number.clone(),
            name => format!("{} ({name})", p.number),
        })
        .collect();
    if !added_supplies.is_empty() {
        return Ok(json!({
            "error": format!(
                "{lib_id} makes {refdes} pin(s) {} supply pins the old symbol did not have, so \
                 nothing on the sheet drives them; it is not a pin-compatible replacement. Use \
                 `set_fields({{ref, fields:{{Value:…}}}})` if the part is electrically the same, \
                 or pick a symbol with the same supply pins.",
                added_supplies.join(", ")
            ),
            "suggestion": suggestion,
        }));
    }
    let selected_footprint = input
        .get("footprint")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            units.first().and_then(|(_, uuid)| {
                edit.doc
                    .symbol(uuid)?
                    .fields
                    .get("Footprint")
                    .map(|field| field.value.clone())
                    .filter(|footprint| !footprint.is_empty())
            })
        });
    let footprint_repair = selected_footprint
        .as_deref()
        .map(|footprint| footprint_repair(ctx, refdes, lib_id, footprint))
        .transpose()?;
    if let Some(text) = input.get("value").and_then(Value::as_str) {
        for (_, uuid) in &units {
            edit.doc.set_field(uuid, "Value", text)?;
        }
    }
    if let Some(repair) = &footprint_repair {
        let footprint = match repair {
            FootprintRepair::Keep => selected_footprint.as_deref().unwrap_or_default(),
            FootprintRepair::Resolve { to, .. } => to,
            FootprintRepair::Clear { .. } => "",
        };
        for (_, uuid) in &units {
            edit.doc.set_field(uuid, "Footprint", footprint)?;
        }
    }

    let seats = plan
        .assignments
        .iter()
        .map(|assignment| {
            let old = &old_pins[assignment.old];
            let new = &new_pins[assignment.new];
            sch_drag::PinReSeat::new(&old.owner, &old.number, &new.owner, &new.number)
        })
        .collect::<Vec<_>>();
    let retired = plan
        .old_without_counterpart
        .iter()
        .map(|index| {
            let old = &old_pins[*index];
            sch_drag::RetiredPin::new(&old.owner, &old.number)
        })
        .collect::<Vec<_>>();
    let mut redraw = match sch_drag::reseat_many(&mut edit.doc, &before_sheet, &seats, &retired) {
        Ok((report, _)) => report,
        Err(error) => {
            return Ok(json!({
                "error": format!(
                    "refused: the replacement pins could not be re-seated cleanly: {error}; nothing was written"
                ),
                "suggestion": suggestion,
            }));
        }
    };
    let mut named = Vec::new();
    for (old_number, net, _) in &before {
        let Some(assignment) = plan
            .assignments
            .iter()
            .find(|assignment| old_pins[assignment.old].number == *old_number)
        else {
            continue;
        };
        let pin = &new_pins[assignment.new];
        let after = sch_doc::connect::extract(&edit.doc);
        if refs::net_of(&after, refdes, &pin.number) == Some(net.as_str()) {
            continue;
        }
        let kind = crate::wiring::sheet_scope(&edit.doc, net).unwrap_or(LabelKind::Local);
        edit.doc.add_label(kind, net, outward_label(pin));
        named.push(format!("{refdes}.{}={net}", pin.number));
    }
    let after = sch_doc::connect::extract(&edit.doc);
    for (old_number, net, _) in &before {
        let Some(assignment) = plan
            .assignments
            .iter()
            .find(|assignment| old_pins[assignment.old].number == *old_number)
        else {
            continue;
        };
        let pin = &new_pins[assignment.new];
        if refs::net_of(&after, refdes, &pin.number) == Some(net.as_str())
            || edit.doc.labels().any(|label| {
                label.at.point().near_eq(pin.at, geom::EPS)
                    && sch_doc::unescape(&label.text) == *net
            })
        {
            continue;
        }
        let kind = crate::wiring::sheet_scope(&edit.doc, net).unwrap_or(LabelKind::Local);
        edit.doc.add_label(kind, net, outward_label(pin));
        named.push(format!("{refdes}.{}={net}", pin.number));
    }
    redraw.labels_added += named.len();
    if redraw.labels_added > 0 {
        edit.warn(format!(
            "pin re-seat debit: {} labels added where a clean orthogonal route could not preserve the connection and its name{}",
            redraw.labels_added,
            if named.is_empty() {
                String::new()
            } else {
                format!(" ({})", named.join(" "))
            }
        ));
    }
    // A wider replacement arrives with pins nothing drives. Left bare they are ERC
    // errors the swap itself manufactured, so mark them no-connect the way
    // `place_parts` marks any signal pin it was given no net for.
    let mut no_connected = Vec::new();
    for index in &plan.new_unassigned {
        let pin = &new_pins[*index];
        if pin.etype == "power_in" {
            continue;
        }
        edit.doc.add_no_connect(pin.at);
        no_connected.push(pin.number.clone());
    }
    if !no_connected.is_empty() {
        edit.warn(format!(
            "{lib_id} has pins {refdes} did not: {} marked no-connect. Wire any that the \
             design needs",
            no_connected.join(", ")
        ));
    }
    reflow_swapped_fields(&mut edit.doc, refdes, &units)?;
    dropped.retain(|number| {
        plan.assignments
            .iter()
            .all(|assignment| old_pins[assignment.old].number != *number)
    });
    let mapped_by_name = plan.mapped_by_name(&old_pins, &new_pins);
    let mut result = edit.commit(
        json!({
            "ref": refdes,
            "lib_id": lib_id,
            "dropped_pins": dropped,
            "mapped_by_name": mapped_by_name,
            "redrawn_segments": redraw.redrawn_segments,
            "labels_added": redraw.labels_added,
        }),
        Allow::nothing().nets(was.clone()).part(refdes).creating(),
    )?;
    if result.get("error").is_some() {
        result["suggestion"] = suggestion;
    } else {
        crate::session::attach_connectivity(
            &mut result,
            ctx,
            [refdes],
            &format!("SWAPPED  {refdes} → {lib_id}"),
        )?;
        let mut resolved = Vec::new();
        let mut unresolved = Vec::new();
        match footprint_repair {
            Some(FootprintRepair::Resolve { from, to }) => {
                resolved.push(ResolvedFootprint {
                    refdes: refdes.to_owned(),
                    from,
                    to,
                });
            }
            Some(FootprintRepair::Clear {
                requested,
                did_you_mean,
            }) => unresolved.push(UnresolvedFootprint {
                refdes: refdes.to_owned(),
                requested,
                did_you_mean,
            }),
            Some(FootprintRepair::Keep) | None => {}
        }
        attach_footprint_repairs(&mut result, &resolved, &unresolved);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::{pin_names_match, valid_refdes};

    /// Tool-created references use KiCad's letter-prefix and numeric-suffix form.
    #[test]
    fn reference_validation_rejects_descriptive_names() {
        for valid in ["R12", "U3", "#PWR01"] {
            assert!(valid_refdes(valid), "{valid}");
        }
        for invalid in ["D_NEW2", "R", "12", ""] {
            assert!(!valid_refdes(invalid), "{invalid}");
        }
    }

    #[test]
    fn pin_name_matching_ignores_case_and_presentation_punctuation() {
        assert!(pin_names_match("~RESET", "r_e-s-e_t"));
        assert!(pin_names_match("CC-1", "cc_1"));
        assert!(!pin_names_match("~", "~"));
        assert!(!pin_names_match("", ""));
        assert!(!pin_names_match("CC1", "CC2"));
    }
}
