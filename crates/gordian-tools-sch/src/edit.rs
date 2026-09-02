//! The symbol mutators: place, remove, move, retag and swap parts.

use std::collections::BTreeMap;

use anyhow::Result;
use geom::{EPS, Point2, Rect};
use gordian_runtime::AgentRuntime;
use sch_doc::{LabelKind, Pose, SchDoc, body_rect, placed_pins};
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
    fn new_pin_number<'a>(
        &self,
        old_number: &str,
        old_pins: &[sch_doc::PlacedPin],
        new_pins: &'a [sch_doc::PlacedPin],
    ) -> Option<&'a str> {
        self.assignments
            .iter()
            .find(|assignment| old_pins[assignment.old].number == old_number)
            .map(|assignment| new_pins[assignment.new].number.as_str())
    }

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
    let provisional = next_refdes(&edit.doc, "ZZ");
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
            next_refdes(&edit.doc, &prefix)
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
    for uuid in &uuids {
        let unit_at = edit
            .doc
            .symbol(uuid)
            .map(|symbol| symbol.at)
            .unwrap_or(park);
        edit.doc
            .move_symbol(uuid, snap(unit_at.x + delta.x), snap(unit_at.y + delta.y))
            .map_err(fail)?;
    }

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
    let specs: Vec<Value> = input
        .get("parts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if specs.is_empty() {
        return Ok(json!({ "error": "add_symbols needs a non-empty `parts` list" }));
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
    let mut result = edit.commit(
        ctx,
        "add_symbols",
        "Add schematic symbols",
        json!({ "placed": placed }),
        allow,
    )?;
    if result.get("error").is_none() {
        crate::session::attach_connectivity(
            &mut result,
            ctx,
            refs.clone(),
            &format!("ADDED  {}", refs.join(" ")),
        )?;
    }
    Ok(result)
}

/// Remove parts, together with the stubs and labels that only served them.
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
    let allow = Allow::nothing().nets(nets).parts(targets.clone());

    let orphaned: Vec<Point2> = placed_pins(&edit.doc)
        .into_iter()
        .filter(|p| targets.contains(&p.refdes))
        .map(|p| p.at)
        .collect();
    for refdes in &targets {
        edit.doc.remove_symbol(refdes)?;
    }
    let mut retracted = retract_stubs(&mut edit.doc, &orphaned);
    let floating = crate::wiring::floating_wires(&edit.doc);
    retracted += edit.doc.remove_drawing(&floating);
    let loose = refs::newly_loose(edit.before(), &sch_doc::connect::extract(&edit.doc));
    edit.commit(
        ctx,
        "remove_symbols",
        "Remove schematic symbols",
        json!({
            "removed": targets,
            "retracted_drawing": retracted,
            "now_loose": loose,
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
    for _ in 0..64 {
        let live: Vec<Point2> = placed_pins(doc).into_iter().map(|p| p.at).collect();
        // A power symbol is placed straight onto the pin it feeds, so a
        // removed pin's coordinate may still carry a pin that is very much
        // there; nothing hanging off it is a stub.
        let orphaned: Vec<Point2> = orphaned
            .iter()
            .copied()
            .filter(|p| !live.iter().any(|q| q.near_eq(*p, EPS)))
            .collect();
        let ends: Vec<Point2> = doc
            .wires()
            .filter_map(refs::ends)
            .flat_map(|(a, b)| [a, b])
            .collect();
        let anchored = |p: Point2| {
            live.iter().any(|q| q.near_eq(p, EPS))
                || doc.labels().any(|l| l.at.point().near_eq(p, EPS))
                || ends.iter().filter(|q| q.near_eq(p, EPS)).count() > 1
        };
        let doomed: Vec<String> = doc
            .wires()
            .filter(|wire| {
                let Some((a, b)) = refs::ends(wire) else {
                    return false;
                };
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

/// How far a move may slide to clear an obstacle and still be the move that
/// was asked for: 20 grid steps, about 25 mm.
const NUDGE_RINGS: i32 = 20;

/// Drag the symbols glued to a pin along with it.
///
/// A power symbol is placed straight onto the pin it feeds — that contact
/// *is* the connection — so a move that left it behind would silently take the
/// pin off its rail. Only symbols whose every pin sits on the moved one
/// travel; anything with a pin elsewhere is wired, not glued.
fn carry_glued_symbols(
    doc: &mut SchDoc,
    moved: &str,
    from: Point2,
    to: Point2,
) -> anyhow::Result<()> {
    if from.near_eq(to, EPS) {
        return Ok(());
    }
    let pins = placed_pins(doc);
    let glued: Vec<String> = doc
        .symbols()
        .filter(|s| s.uuid != moved)
        .filter(|s| {
            let own: Vec<&sch_doc::PlacedPin> = pins.iter().filter(|p| p.owner == s.uuid).collect();
            !own.is_empty() && own.iter().all(|p| p.at.near_eq(from, EPS))
        })
        .map(|s| s.uuid.clone())
        .collect();
    for uuid in glued {
        let at = match doc.symbol(&uuid) {
            Some(symbol) => symbol.at,
            None => continue,
        };
        doc.move_symbol(&uuid, at.x + to.x - from.x, at.y + to.y - from.y)?;
    }
    Ok(())
}

/// Apply a `rot`/`mirror` to a symbol, returning the rotation it now carries.
///
/// Rotating in place is how a diode is reversed or a part turned to meet a
/// wire; without it the only way to change an orientation is to delete the
/// part and place it again, losing its connections.
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
    let all: Vec<String> = moves
        .iter()
        .filter_map(|m| m.get("ref").and_then(Value::as_str).map(str::to_string))
        .collect();
    if all.len() != moves.len() {
        return Ok(json!({ "error": "every move needs a `ref`" }));
    }
    let mut placed = Vec::new();
    // A part still waiting its turn is not an obstacle to the one being placed;
    // one already placed in this batch is.
    let mut pending: Vec<String> = moves
        .iter()
        .filter_map(|step| {
            let refdes = step.get("ref")?.as_str()?;
            let units = refs::units(&edit.doc, refdes);
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
        let units = refs::units(&edit.doc, &refdes);
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
        if edit.doc.symbol(&uuid).is_none() {
            return Ok(json!({ "error": format!("no symbol `{refdes}` on the sheet") }));
        }
        // Where the pins are *now*, before any turn: their wires follow them
        // through both the rotation and the move.
        let was: Vec<Point2> = placed_pins(&edit.doc)
            .into_iter()
            .filter(|p| p.owner == uuid)
            .map(|p| p.at)
            .collect();
        // Turning a part is how a diode is reversed, and it changes the shape
        // that has to fit, so it happens before the destination is chosen.
        let turned = orient(&mut edit.doc, &uuid, step)?;
        let symbol = match edit.doc.symbol(&uuid) {
            Some(symbol) => symbol,
            None => return Ok(json!({ "error": format!("no symbol `{refdes}` on the sheet") })),
        };
        let origin = symbol.at.point();
        let body = crate::place::extent(&edit.doc, symbol);
        let (w, h) = body.map_or((10.0, 10.0), |r| (r.width(), r.height()));
        let centre = body.map_or(origin, |r| r.center());
        let staying = ["to", "by", "near"]
            .iter()
            .all(|key| step.get(key).is_none());
        let want = if staying {
            Destination::Origin(origin)
        } else if let Some(by) = step.get("by").and_then(Value::as_array) {
            let n: Vec<f64> = by.iter().filter_map(Value::as_f64).collect();
            if n.len() != 2 {
                return Ok(json!({ "error": "`by` must be [dx, dy] in mm" }));
            }
            Destination::Centre(Point2::new(centre.x + n[0], centre.y + n[1]))
        } else {
            match destination(&edit.doc, step, w, h, &pending) {
                Ok((want, _)) => want,
                Err(error) => return Ok(json!({ "error": error })),
            }
        };
        let mut at = snap_point(want.origin_for(origin, centre));
        // Clearance is about the extent, which sits `centre - origin` away.
        let offset = Point2::new(centre.x - origin.x, centre.y - origin.y);
        let landing = Point2::new(at.x + offset.x, at.y + offset.y);
        let occupancy = Occupancy::skipping(&edit.doc, &pending);
        let mut nudge = None;
        if !occupancy.free(landing, w, h) {
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
        // Whatever met this part's pins comes with it, the way KiCAD drags a
        // symbol: leaving the wires behind would silently unwire the board.
        edit.doc.move_symbol(&uuid, at.x, at.y)?;
        let now: Vec<Point2> = placed_pins(&edit.doc)
            .into_iter()
            .filter(|p| p.owner == uuid)
            .map(|p| p.at)
            .collect();
        let mut landed = Vec::new();
        for (from, to) in was.into_iter().zip(now) {
            edit.doc.move_attached(from, to);
            carry_glued_symbols(&mut edit.doc, &uuid, from, to)?;
            landed.push(to);
        }
        let straightened = crate::wiring::straighten(&mut edit.doc, &landed);
        pending.retain(|pending_uuid| *pending_uuid != uuid);
        let mut report = json!({ "ref": refdes, "at": [at.x, at.y] });
        if let Some(to) = nudge {
            report["nudged_to"] = json!(to);
        }
        if let Some(rot) = turned {
            report["rot"] = json!(rot);
        }
        if straightened > 0 {
            report["rerouted_wires"] = json!(straightened);
        }
        placed.push(report);
    }
    edit.commit(
        ctx,
        "move_symbols",
        "Move schematic symbols",
        json!({
            "moved": placed,
            "placement": "final and clean; any nudged_to coordinate is the collision-free final position, so do not move it again",
        }),
        Allow::nothing(),
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
        ctx,
        "set_fields",
        "Set schematic fields",
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
    let catalog = ctx.footprint_catalog()?;
    let mut requested = Vec::with_capacity(assignments.len());
    for assignment in assignments {
        let (Some(reference), Some(footprint)) = (
            assignment.get("reference").and_then(Value::as_str),
            assignment.get("footprint").and_then(Value::as_str),
        ) else {
            return Ok(json!({ "error": "every assignment needs `reference` and `footprint`" }));
        };
        let id = match kicad_footprint::FootprintId::parse(footprint) {
            Ok(id) => id,
            Err(_) => {
                let suggestions = catalog.suggest(footprint);
                return Ok(json!({
                    "error": format!("{reference}: {}", kicad_footprint::unknown_footprint_message(footprint, &suggestions)),
                    "suggestions": suggestions,
                }));
            }
        };
        if let Err(error) = catalog.footprint(&id) {
            if !error.is_not_found() {
                return Ok(json!({
                    "error": format!("{reference}: footprint `{footprint}` could not be used: {error}"),
                }));
            }
            let suggestions = catalog.suggest(footprint);
            return Ok(json!({
                "error": format!("{reference}: {}", kicad_footprint::unknown_footprint_message(footprint, &suggestions)),
                "suggestions": suggestions,
            }));
        }
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
    for (reference, footprint) in &requested {
        for (_, uuid) in refs::units(&edit.doc, reference) {
            edit.doc.set_field(&uuid, "Footprint", footprint)?;
        }
    }
    edit.commit(
        ctx,
        "assign_footprints",
        "Assign schematic footprints",
        json!({
            "assigned": requested.iter().map(|(reference, footprint)| {
                json!({ "reference": reference, "footprint": footprint })
            }).collect::<Vec<_>>()
        }),
        Allow::nothing(),
    )
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
        ctx,
        "set_flags",
        "Set schematic part flags",
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
    for (key, field) in [("value", "Value"), ("footprint", "Footprint")] {
        if let Some(text) = input.get(key).and_then(Value::as_str) {
            for (_, uuid) in &units {
                edit.doc.set_field(uuid, field, text)?;
            }
        }
    }

    // The new part's pins sit where its own body puts them, which need not be
    // where the old ones were. Drag each pin's wires across to it; only a pin
    // that still cannot reach its net gets named in place.
    let mut mapped = Vec::new();
    for (number, net, was_at) in &before {
        let pin_number = plan
            .new_pin_number(number, &old_pins, &new_pins)
            .expect("every old pin has an assignment");
        let pin = new_pins
            .iter()
            .find(|pin| pin.number == pin_number)
            .expect("assigned new pin exists");
        let landed = pin.at;
        mapped.push((pin.number.clone(), net.clone(), *was_at, landed));
    }
    let moves: Vec<(Point2, Point2)> = mapped
        .iter()
        .map(|(_, _, was_at, landed)| (*was_at, *landed))
        .collect();
    edit.doc.move_attached_many(&moves);

    let mut restored = Vec::new();
    for (number, net, _, landed) in mapped {
        let after = sch_doc::connect::extract(&edit.doc);
        if refs::net_of(&after, refdes, &number) == Some(net.as_str()) {
            continue;
        }
        edit.doc
            .add_label(LabelKind::Local, &net, Pose::new(landed.x, landed.y, 0.0));
        restored.push(format!("{refdes}.{number}={net}"));
    }
    if !restored.is_empty() {
        edit.warn(format!(
            "these pins could not reach their old net through wires, so it was named at \
             them instead: {}",
            restored.join(" ")
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
        ctx,
        "swap_symbol",
        "Swap a schematic symbol",
        json!({
            "ref": refdes,
            "lib_id": lib_id,
            "dropped_pins": dropped,
            "mapped_by_name": mapped_by_name,
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
