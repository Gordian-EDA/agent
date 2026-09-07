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

use crate::refs;
use crate::session::{Allow, Edit, symbol_source};

#[derive(Debug)]
pub(crate) enum FootprintRepair {
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
pub(crate) struct ResolvedFootprint {
    #[serde(rename = "ref")]
    pub(crate) refdes: String,
    pub(crate) from: String,
    pub(crate) to: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct UnresolvedFootprint {
    #[serde(rename = "ref")]
    refdes: String,
    requested: String,
    did_you_mean: Vec<String>,
}

/// Turn footprint lookup and pad compatibility into repairable metadata.
/// The repair a PLACEMENT applies unasked: it stays within the package family that
/// was named, and a footprint it cannot repair is cleared with in-family suggestions.
pub(crate) fn footprint_repair(
    ctx: &AgentRuntime,
    reference: &str,
    symbol: &str,
    requested: &str,
    ignored_pins: &BTreeSet<String>,
) -> Result<FootprintRepair> {
    footprint_repair_ignoring(ctx, reference, symbol, requested, ignored_pins, true)
}

fn footprint_repair_ignoring(
    ctx: &AgentRuntime,
    reference: &str,
    symbol: &str,
    requested: &str,
    ignored_pins: &BTreeSet<String>,
    within_family: bool,
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
    // A repair is applied unasked only within the package family that was named: a
    // 3.5 mm jack may become another 3.5 mm jack, never a 6.35 mm one. An explicit
    // assignment takes any compatible repair.
    if mismatch.suggestion_compatible
        && let Some(to) = mismatch.suggestion.clone()
        && (!within_family || same_footprint_family(requested, &to))
    {
        return Ok(FootprintRepair::Resolve {
            from: requested.to_owned(),
            to,
        });
    }
    let mut did_you_mean: Vec<String> = mismatch.suggestion.into_iter().collect();
    if within_family {
        let family = requested.split_once(':').map_or(requested, |(_, name)| name);
        let family: String = family.split('_').take(2).collect::<Vec<_>>().join("_");
        let library = requested.split_once(':').map_or("", |(library, _)| library);
        let query = format!("{library}:{family}");
        // The family's own footprints, compatible ones first: when none of them fits
        // the symbol, the nearest name is still what a person reaches for next.
        let mut hits = gordian_runtime::footprint_compat::search_compatible_footprints(ctx, symbol, Some(&query), 8)?;
        hits.sort_by_key(|hit| !hit.compatible);
        for hit in hits {
            if same_footprint_family(requested, &hit.lib_id) && hit.lib_id != requested && !did_you_mean.contains(&hit.lib_id) {
                did_you_mean.push(hit.lib_id);
            }
        }
        did_you_mean.truncate(4);
    }
    Ok(FootprintRepair::Clear {
        requested: requested.to_owned(),
        did_you_mean,
    })
}

/// Same library, same leading name token, and the same physical size when both
/// names state one in millimetres: `Jack_3.5mm_CUI…` and `Jack_3.5mm_Switronic…` are
/// one family, `Jack_6.35mm_…` is another, while `R_Array_…_2x0603` may become
/// `R_0603_…` since neither states a length.
fn same_footprint_family(a: &str, b: &str) -> bool {
    let split = |id: &str| {
        let (library, name) = id.split_once(':').unwrap_or(("", id));
        let mut tokens = name.split('_');
        let head = tokens.next().unwrap_or_default().to_string();
        let size = name
            .split('_')
            .find(|t| t.ends_with("mm") && t.trim_end_matches("mm").chars().all(|c| c.is_ascii_digit() || c == '.'))
            .map(str::to_string);
        (library.to_string(), head, size)
    };
    let (la, ha, sa) = split(a);
    let (lb, hb, sb) = split(b);
    la == lb && ha == hb && (sa.is_none() || sb.is_none() || sa == sb)
}

pub(crate) fn attach_footprint_repairs(
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
                    "assign a compatible footprint to {} with set_fields({{footprints}})",
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
        if input.get("block").is_some() || input.get("bbox").is_some() {
            return remove_region(input, ctx);
        }
        return Ok(json!({ "error": "remove_symbols needs `refs`, `block` or `bbox`" }));
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
            "error": "remove_symbols needs `refs`, `bbox` or `block`",
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
fn remove_region(input: Value, ctx: &AgentRuntime) -> Result<Value> {
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


/// Set or clear a part's properties.
pub fn set_fields(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    // `footprints` is the many-part form; a `Footprint` field on one part goes the
    // same validated way; `dnp` / `in_bom` are the part's flags.
    if let Some(footprints) = input.get("footprints").and_then(Value::as_object) {
        let assignments: Vec<Value> = footprints
            .iter()
            .map(|(reference, footprint)| json!({"reference": reference, "footprint": footprint}))
            .collect();
        return assign_footprints(json!({"assignments": assignments}), ctx);
    }
    let Some(refdes) = input.get("ref").and_then(Value::as_str) else {
        return Ok(json!({ "error": "set_fields needs `ref` with `fields`, `dnp` or `in_bom`, or `footprints`" }));
    };
    let mut fields = input.get("fields").and_then(Value::as_object).cloned().unwrap_or_default();
    let mut out = serde_json::Map::new();
    if let Some(key) = fields.keys().find(|k| k.eq_ignore_ascii_case("Footprint")).cloned() {
        let footprint = fields.remove(&key);
        let assigned = match footprint.as_ref().and_then(Value::as_str) {
            Some(footprint) => assign_footprints(json!({"assignments": [{"reference": refdes, "footprint": footprint}]}), ctx)?,
            None => return Ok(json!({ "error": "a Footprint is set to a KiCAD Lib:Name, not cleared here" })),
        };
        if assigned.get("error").is_some() {
            return Ok(assigned);
        }
        out.insert("footprint".into(), assigned);
    }
    if input.get("dnp").is_some() || input.get("in_bom").is_some() {
        let flags = set_flags(json!({"ref": refdes, "dnp": input.get("dnp"), "in_bom": input.get("in_bom")}), ctx)?;
        if flags.get("error").is_some() {
            return Ok(flags);
        }
        out.insert("flags".into(), flags);
    }
    if fields.is_empty() {
        return Ok(match out.len() {
            0 => json!({ "error": "set_fields needs `fields`, `dnp` or `in_bom` for the part" }),
            _ => Value::Object(out),
        });
    }
    let fields = &fields;
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
fn assign_footprints(input: Value, ctx: &AgentRuntime) -> Result<Value> {
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
            footprint_repair_ignoring(ctx, reference, &symbol, footprint, &ignored_pins, false)?
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
fn set_flags(input: Value, ctx: &AgentRuntime) -> Result<Value> {
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
    // Rail glyphs and flags seated on the part's pins go where the pins go: a swap
    // that transposes two pins carries the glyph on each to the other's net.
    let welded: Vec<String> = placed_pins(&edit.doc)
        .into_iter()
        .filter(|pin| pin.refdes.starts_with('#'))
        .filter(|pin| old_pins.iter().any(|old| old.at.near_eq(pin.at, geom::EPS)))
        .map(|pin| pin.refdes)
        .collect();

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
        .map(|footprint| footprint_repair(ctx, refdes, lib_id, footprint, &BTreeSet::new()))
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
        Allow::nothing().joining_nets(was.clone()).part(refdes).parts(welded).creating(),
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
    use super::pin_names_match;

    /// Tool-created references use KiCad's letter-prefix and numeric-suffix form.
    #[test]
    fn pin_name_matching_ignores_case_and_presentation_punctuation() {
        assert!(pin_names_match("~RESET", "r_e-s-e_t"));
        assert!(pin_names_match("CC-1", "cc_1"));
        assert!(!pin_names_match("~", "~"));
        assert!(!pin_names_match("", ""));
        assert!(!pin_names_match("CC1", "CC2"));
    }
}
