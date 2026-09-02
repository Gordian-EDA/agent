//! `sync_board` — KiCAD's "Update PCB from Schematic", as a guarded incremental
//! edit.
//!
//! The schematic is the netlist; the board is the geometry. Sync makes the board
//! agree with the schematic by changing only what disagrees: parts appear and
//! disappear, footprints swap in place, pads follow their nets, values follow
//! their fields. Placement, copper, outline, zones and rules are not the
//! schematic's business, so sync leaves them exactly as they were — and retracts
//! only the copper its own edit invalidated.
//!
//! On a project with no `.kicad_pcb` there is nothing to preserve, so sync
//! creates the board: every part is "added", and the outline is sized from the
//! footprints unless the caller names `bounds`.
//!
//! Every run that writes snapshots the board first and re-checks connectivity
//! afterwards. The invariant is one-directional: copper may connect *less* than
//! the schematic asks (an unrouted net is a to-do), never *more* (a short is a
//! defect). A violation restores the snapshot and refuses.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{Value, json};

use geom::Rect;
use gordian_runtime::AgentRuntime;
use kicad_board::{BoardDoc, BoardFootprint};
use kicad_footprint::{FootprintCatalog, FootprintId};
use pcb_drc::connectivity::Violation;
use pcb_model::Point2;
use pcb_place::{LockedAt, PlacementHints};

use crate::create::{
    BoardSeedSpec, SeedPart, add_default_power_pours, apply_complexity_default_layer_count,
    emit_board_footprint, emit_seed_board, parse_bounds, parse_seed_rules,
};

/// One schematic part as the exported netlist has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SchematicPart {
    pub(crate) reference: String,
    pub(crate) value: String,
    pub(crate) footprint: String,
    /// Pad number → net name.
    pub(crate) pad_nets: BTreeMap<String, String>,
}

/// One part whose identity or field changed, and what it changed from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Change {
    pub(crate) reference: String,
    pub(crate) from: String,
    pub(crate) to: String,
}

/// One pad that must point at a different net. `None` is "no net".
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PadRetarget {
    pub(crate) reference: String,
    pub(crate) pad: String,
    pub(crate) from: Option<String>,
    pub(crate) to: Option<String>,
}

/// Everything the schematic and the board disagree about.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct BoardDelta {
    pub(crate) added: Vec<String>,
    pub(crate) removed: Vec<String>,
    pub(crate) footprint_changed: Vec<Change>,
    pub(crate) value_changed: Vec<Change>,
    /// Nets whose pad membership differs between schematic and board.
    pub(crate) nets_changed: Vec<String>,
    pub(crate) pads_retargeted: Vec<PadRetarget>,
}

impl BoardDelta {
    pub(crate) fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.removed.is_empty()
            && self.footprint_changed.is_empty()
            && self.value_changed.is_empty()
            && self.pads_retargeted.is_empty()
    }

    /// References whose pads move, vanish or change identity — the copper on
    /// them can no longer be trusted.
    fn copper_invalidating(&self) -> BTreeSet<&str> {
        self.removed
            .iter()
            .map(String::as_str)
            .chain(self.footprint_changed.iter().map(|c| c.reference.as_str()))
            .chain(self.pads_retargeted.iter().map(|p| p.reference.as_str()))
            .collect()
    }

    fn to_json(&self) -> Value {
        let changes = |changes: &[Change]| -> Vec<Value> {
            changes
                .iter()
                .map(|c| json!({ "reference": c.reference, "from": c.from, "to": c.to }))
                .collect()
        };
        json!({
            "added": self.added,
            "removed": self.removed,
            "footprint_changed": changes(&self.footprint_changed),
            "value_changed": changes(&self.value_changed),
            "nets_changed": self.nets_changed,
            "pads_retargeted": self.pads_retargeted.iter().map(|p| json!({
                "pad": format!("{}.{}", p.reference, p.pad),
                "from": p.from,
                "to": p.to,
            })).collect::<Vec<_>>(),
        })
    }
}

/// What the schematic asks for, against what the board has.
///
/// A part the two share is compared field by field; a footprint swap subsumes
/// its own pad retargeting, because the swap re-emits every pad from the new
/// library part with the schematic's nets already on it.
pub(crate) fn diff(schematic: &[SchematicPart], board: &[BoardFootprint]) -> BoardDelta {
    let by_board: BTreeMap<&str, &BoardFootprint> =
        board.iter().map(|fp| (fp.reference.as_str(), fp)).collect();
    let by_schematic: BTreeMap<&str, &SchematicPart> = schematic
        .iter()
        .map(|part| (part.reference.as_str(), part))
        .collect();

    let mut delta = BoardDelta {
        added: by_schematic
            .keys()
            .filter(|reference| !by_board.contains_key(*reference))
            .map(|reference| (*reference).to_owned())
            .collect(),
        removed: by_board
            .keys()
            .filter(|reference| !by_schematic.contains_key(*reference))
            .map(|reference| (*reference).to_owned())
            .collect(),
        ..BoardDelta::default()
    };

    for (reference, part) in &by_schematic {
        let Some(existing) = by_board.get(reference) else {
            continue;
        };
        if existing.lib_id != part.footprint {
            delta.footprint_changed.push(Change {
                reference: (*reference).to_owned(),
                from: existing.lib_id.clone(),
                to: part.footprint.clone(),
            });
            continue;
        }
        if existing.value != part.value {
            delta.value_changed.push(Change {
                reference: (*reference).to_owned(),
                from: existing.value.clone(),
                to: part.value.clone(),
            });
        }
        for pad in part
            .pad_nets
            .keys()
            .chain(existing.pad_nets.keys())
            .collect::<BTreeSet<_>>()
        {
            let (want, have) = (part.pad_nets.get(pad), existing.pad_nets.get(pad));
            if want != have {
                delta.pads_retargeted.push(PadRetarget {
                    reference: (*reference).to_owned(),
                    pad: pad.clone(),
                    from: have.cloned(),
                    to: want.cloned(),
                });
            }
        }
    }

    delta.nets_changed = changed_nets(&by_schematic, &by_board);
    delta
}

/// Nets whose set of `REF.PAD` members is not the same on both sides.
fn changed_nets(
    schematic: &BTreeMap<&str, &SchematicPart>,
    board: &BTreeMap<&str, &BoardFootprint>,
) -> Vec<String> {
    fn members<'a>(
        pads: impl Iterator<Item = (&'a str, &'a BTreeMap<String, String>)>,
    ) -> BTreeMap<String, BTreeSet<String>> {
        let mut nets: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (reference, pad_nets) in pads {
            for (pad, net) in pad_nets {
                nets.entry(net.clone())
                    .or_default()
                    .insert(format!("{reference}.{pad}"));
            }
        }
        nets
    }
    let want = members(
        schematic
            .iter()
            .map(|(reference, part)| (*reference, &part.pad_nets)),
    );
    let have = members(
        board
            .iter()
            .map(|(reference, fp)| (*reference, &fp.pad_nets)),
    );
    want.keys()
        .chain(have.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|net| want.get(*net) != have.get(*net))
        .cloned()
        .collect()
}

// ── the tool ────────────────────────────────────────────────────────────────

/// Bring the board into agreement with the live schematic.
pub fn sync_board(input: Value, ctx: &AgentRuntime) -> Result<Value> {
    let parts = match schematic_parts(ctx) {
        Ok(parts) => parts,
        Err(refusal) => return Ok(refusal),
    };
    if !ctx.pcb_path().exists() {
        return Ok(create_board(&parts, &input, ctx));
    }
    Ok(update_board(&parts, &input, ctx))
}

/// The schematic's parts and nets, or the refusal that says why the board
/// cannot be synced yet. The gate is exactly `check_schematic`'s: ERC errors,
/// unassigned footprints and symbol/footprint pad mismatches block; warnings do
/// not.
fn schematic_parts(ctx: &AgentRuntime) -> std::result::Result<Vec<SchematicPart>, Value> {
    if !ctx.sch_path().exists() {
        return Err(json!({
            "error": "no .kicad_sch yet — create the schematic with place_parts first"
        }));
    }
    let netlist = ctx
        .env()
        .netlist(ctx.sch_path())
        .map_err(|e| json!({ "error": format!("could not export the schematic netlist: {e}") }))?;
    let erc = ctx.env().erc(ctx.sch_path()).map_err(
        |e| json!({ "error": format!("could not run ERC before syncing the board: {e}") }),
    )?;
    if erc.error_count() > 0 {
        let violations: Vec<Value> = erc
            .violations
            .iter()
            .filter(|v| v.severity == "error")
            .map(|v| json!({ "type": v.kind, "description": v.description }))
            .collect();
        return Err(json!({
            "ok": false,
            "error": format!(
                "schematic ERC has {} error(s); fix the live schematic before sync_board",
                erc.error_count()
            ),
            "erc": {
                "errors": erc.error_count(),
                "warnings": erc.warning_count(),
                "violations": violations,
            },
        }));
    }

    let mut pad_nets_by_ref: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for net in &netlist.nets {
        if net.name.is_empty() {
            continue;
        }
        for (reference, pin) in &net.nodes {
            if reference.is_empty() || pin.is_empty() {
                continue;
            }
            pad_nets_by_ref
                .entry(reference.clone())
                .or_default()
                .insert(pin.clone(), net.name.clone());
        }
    }

    let mut parts = Vec::with_capacity(netlist.components.len());
    let mut missing = Vec::new();
    for component in &netlist.components {
        let footprint = component
            .properties
            .get("Footprint")
            .cloned()
            .unwrap_or_default();
        if footprint.is_empty() {
            missing.push(component.reference.clone());
        }
        parts.push(SchematicPart {
            reference: component.reference.clone(),
            value: component.value.clone(),
            footprint,
            pad_nets: pad_nets_by_ref
                .remove(&component.reference)
                .unwrap_or_default(),
        });
    }
    if !missing.is_empty() {
        return Err(json!({
            "ok": false,
            "part_count": parts.len(),
            "missing_footprints": missing,
            "next_tool": "assign_footprints",
            "next": "call assign_footprints({assignments:[{reference, footprint}, ...]}), then sync_board again",
            "note": "some live schematic symbols have no footprint field — do not retry sync_board until footprints are assigned",
        }));
    }

    let mismatches = gordian_runtime::footprint_compat::netlist_pin_mismatches(ctx, &netlist)
        .map_err(|e| json!({ "error": format!("could not compare symbol pins to pads: {e}") }))?;
    if !mismatches.is_empty() {
        return Err(json!({
            "ok": false,
            "error": "schematic symbol and assigned footprint have incompatible numbered pins/pads",
            "footprint_pin_mismatches": mismatches,
            "next_tool": "swap_symbol",
            "next": "make the two agree: swap_symbol to a part whose pin numbers are the footprint's pad numbers, or assign_footprints a package whose pads match the pins — then sync_board again",
            "note": "Every named electrical pad must match a symbol pin and every symbol pin must have a physical pad. Unnumbered mechanical pads and repeated pads with a valid shared number are allowed.",
        }));
    }
    Ok(parts)
}

fn seed_parts(parts: &[SchematicPart]) -> Vec<SeedPart> {
    parts
        .iter()
        .map(|part| SeedPart {
            reference: part.reference.clone(),
            value: Some(part.value.clone()),
            footprint: part.footprint.clone(),
            pad_nets: part.pad_nets.clone(),
            locked: None,
        })
        .collect()
}

// ── the empty board ─────────────────────────────────────────────────────────

/// Create the board a project does not have yet. Every part is "added".
fn create_board(parts: &[SchematicPart], input: &Value, ctx: &AgentRuntime) -> Value {
    let catalog = match ctx.footprint_catalog() {
        Ok(catalog) => catalog,
        Err(e) => return json!({ "error": format!("footprint catalog unavailable: {e}") }),
    };
    let seed = seed_parts(parts);
    let bounds = match input.get("bounds") {
        Some(_) => match parse_bounds(input.get("bounds")) {
            Ok(bounds) => bounds,
            Err(e) => return json!({ "error": e }),
        },
        None => auto_bounds(&seed, catalog),
    };
    let mut rules = match parse_seed_rules(input.get("rules")) {
        Ok(rules) => rules,
        Err(e) => return json!({ "error": e }),
    };
    apply_complexity_default_layer_count(&mut rules, input.get("rules"), seed.len());
    add_default_power_pours(&mut rules, &seed);

    let spec = BoardSeedSpec {
        bounds,
        rules,
        parts: seed,
        outline: None,
    };
    let text = match emit_seed_board(&spec, catalog) {
        Ok(text) => text,
        Err(e) => return json!({ "error": e }),
    };
    ctx.close_kicad_session();
    if let Err(e) = std::fs::write(ctx.pcb_path(), text) {
        return json!({ "error": format!("could not write {}: {e}", ctx.pcb_path().display()) });
    }
    json!({
        "ok": true,
        "created": true,
        "delta": BoardDelta {
            added: parts.iter().map(|p| p.reference.clone()).collect(),
            ..BoardDelta::default()
        }.to_json(),
        "part_count": parts.len(),
        "layer_count": spec.rules.layer_count,
        "outline": bounds_json(&bounds),
        "path": ctx.pcb_path().display().to_string(),
        "note": "board created from the schematic — run place_board, then route_board, then check_board",
    })
}

/// Size a fresh outline from the parts: roughly twice the total courtyard area
/// (packing plus routing channels), square, never smaller than the largest part
/// with a margin.
fn auto_bounds(parts: &[SeedPart], catalog: &FootprintCatalog) -> Rect {
    const MARGIN: f64 = 2.0;
    let (mut area, mut max_w, mut max_h) = (0.0f64, 0.0f64, 0.0f64);
    for part in parts {
        let Some(courtyard) = FootprintId::parse(&part.footprint)
            .ok()
            .and_then(|id| catalog.footprint(&id).ok())
            .map(|fp| fp.courtyard)
        else {
            continue;
        };
        let (w, h) = (
            courtyard.max_x - courtyard.min_x,
            courtyard.max_y - courtyard.min_y,
        );
        area += w * h;
        max_w = max_w.max(w);
        max_h = max_h.max(h);
    }
    let side = (area * 2.0).sqrt();
    let width = side.max(max_w + 2.0 * MARGIN).max(20.0).ceil();
    let height = side.max(max_h + 2.0 * MARGIN).max(16.0).ceil();
    Rect {
        min_x: 0.0,
        min_y: 0.0,
        max_x: width,
        max_y: height,
    }
}

fn bounds_json(bounds: &Rect) -> Value {
    json!({
        "min_x": bounds.min_x,
        "min_y": bounds.min_y,
        "max_x": bounds.max_x,
        "max_y": bounds.max_y,
    })
}

// ── the incremental edit ────────────────────────────────────────────────────

fn update_board(parts: &[SchematicPart], input: &Value, ctx: &AgentRuntime) -> Value {
    if input.get("bounds").is_some() || input.get("rules").is_some() {
        return json!({
            "error": "sync_board takes `bounds`/`rules` only when it creates the board; \
                      change an existing board's outline with update_board_outline and its \
                      widths with set_net_width",
        });
    }
    let catalog = match ctx.footprint_catalog() {
        Ok(catalog) => catalog,
        Err(e) => return json!({ "error": format!("footprint catalog unavailable: {e}") }),
    };
    let _ = ctx.kicad().save_if_open();
    let before = match crate::active_board(ctx) {
        Ok(board) => board,
        Err(e) => return json!({ "error": e }),
    };
    let path = ctx.pcb_path();
    let original = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) => return json!({ "error": format!("could not read the board: {e}") }),
    };
    let mut doc = match BoardDoc::parse(original.clone()) {
        Ok(doc) => doc,
        Err(e) => return json!({ "error": format!("could not read the board document: {e}") }),
    };

    let delta = diff(parts, &doc.footprints());
    if delta.is_empty() {
        return json!({
            "ok": true,
            "delta": delta.to_json(),
            "changed": false,
            "note": "the board already matches the schematic; nothing was written",
        });
    }

    let revision = match snapshot(ctx, &original) {
        Ok(revision) => revision,
        Err(e) => return json!({ "error": format!("could not snapshot the board: {e}") }),
    };

    let poses: BTreeMap<String, (Point2, f64)> = doc
        .footprints()
        .into_iter()
        .map(|fp| (fp.reference, (fp.at, fp.rotation)))
        .collect();
    let by_reference: BTreeMap<&str, &SchematicPart> = parts
        .iter()
        .map(|part| (part.reference.as_str(), part))
        .collect();
    if let Err(e) = apply(&mut doc, &delta, &by_reference, &poses, catalog) {
        return json!({ "error": e, "revision": revision });
    }

    ctx.close_kicad_session();
    if let Err(e) = std::fs::write(&path, doc.into_text()) {
        return json!({ "error": format!("could not write the board: {e}") });
    }

    // Only copper the edit invalidated comes out: traces touching a pad that
    // vanished, changed package, or changed net. Pad extents are per part, so a
    // single retargeted pad retracts its part's traces — conservative, and the
    // named nets are the ones the agent re-routes.
    let pads = crate::copper::pad_extents(&before.problem, delta.copper_invalidating());
    let retract = crate::copper::retract(&before.copper, &pads, &BTreeSet::new());
    if let Err(e) = crate::copper::write_retained(
        ctx,
        before.problem.layer_count,
        &before.layer_names,
        &retract,
    ) {
        return json!({ "error": format!("the board was synced but its copper was not retracted: {e}"), "revision": revision });
    }

    let placed = match place_added(&delta.added, ctx) {
        Ok(placed) => placed,
        Err(e) => return json!({ "error": e, "revision": revision }),
    };

    let mut nets_to_reroute: BTreeSet<String> = retract.nets.clone();
    nets_to_reroute.extend(delta.nets_changed.iter().cloned());

    let mut result = json!({
        "ok": true,
        "changed": true,
        "delta": delta.to_json(),
        "placed": placed,
        "retracted_tracks": retract.count,
        "nets_to_reroute": nets_to_reroute,
        "revision": revision,
        "note": "only the delta was applied; every other part kept its position and copper — run route_board on nets_to_reroute, then check_board",
    });
    if let Some(refusal) = guard(ctx, &path, &original, &revision) {
        result = refusal;
    }
    result
}

/// Write the delta into the board document.
fn apply(
    doc: &mut BoardDoc,
    delta: &BoardDelta,
    schematic: &BTreeMap<&str, &SchematicPart>,
    poses: &BTreeMap<String, (Point2, f64)>,
    catalog: &FootprintCatalog,
) -> std::result::Result<(), String> {
    let wanted: BTreeSet<&str> = schematic
        .values()
        .flat_map(|part| part.pad_nets.values().map(String::as_str))
        .collect();
    let codes = doc.ensure_nets(wanted)?;

    for reference in delta
        .removed
        .iter()
        .chain(delta.footprint_changed.iter().map(|c| &c.reference))
    {
        doc.remove_footprint(reference)?;
    }

    // A swap keeps the part where it sat; a new part starts just inside the
    // board and is placed properly below.
    let origin = doc
        .footprints()
        .first()
        .map(|fp| fp.at)
        .unwrap_or(Point2::new(2.0, 2.0));
    for (index, reference) in delta
        .footprint_changed
        .iter()
        .map(|c| c.reference.as_str())
        .chain(delta.added.iter().map(String::as_str))
        .enumerate()
    {
        let part = schematic
            .get(reference)
            .ok_or_else(|| format!("part {reference} vanished from the schematic mid-sync"))?;
        let (at, rotation) = poses
            .get(reference)
            .copied()
            .unwrap_or((Point2::new(origin.x + 2.54 * index as f64, origin.y), 0.0));
        let seed = SeedPart {
            reference: part.reference.clone(),
            value: Some(part.value.clone()),
            footprint: part.footprint.clone(),
            pad_nets: part.pad_nets.clone(),
            locked: None,
        };
        let block = emit_board_footprint(&seed, at, rotation, catalog, &codes)?;
        doc.insert_footprint(&block)?;
    }

    for retarget in &delta.pads_retargeted {
        let net = retarget
            .to
            .as_ref()
            .map(|name| {
                codes
                    .get(name)
                    .map(|code| (name.as_str(), *code))
                    .ok_or_else(|| format!("net {name} is missing from the board net table"))
            })
            .transpose()?;
        doc.set_pad_net(&retarget.reference, &retarget.pad, net)?;
    }

    for change in &delta.value_changed {
        doc.set_value(&change.reference, &change.to)?;
    }
    Ok(())
}

/// Place the parts the sync added, with every part already on the board locked.
fn place_added(added: &[String], ctx: &AgentRuntime) -> std::result::Result<Vec<Value>, String> {
    if added.is_empty() {
        return Ok(Vec::new());
    }
    let board = crate::active_board(ctx)?;
    let mut problem = crate::place::place_problem_from_snapshot(&board, ctx)?;
    let new: BTreeSet<&str> = added.iter().map(String::as_str).collect();
    let existing: BTreeMap<&str, &kicad_board::ImportedPart> = board
        .imported
        .parts
        .iter()
        .map(|part| (part.reference.as_str(), part))
        .collect();
    for part in &mut problem.parts {
        if new.contains(part.reference.as_str()) {
            part.locked = None;
        } else if let Some(imported) = existing.get(part.reference.as_str()) {
            part.locked = Some(LockedAt {
                at: imported.at,
                rotation: imported.rotation as f64,
            });
        }
    }

    let result = pcb_engine::place_tuned(&problem, &PlacementHints::default());
    if !result.legal {
        return Ok(vec![json!({
            "note": "the added parts did not fit around the locked board; they sit at provisional \
                     positions — grow the outline with update_board_outline or move them with move_parts",
        })]);
    }
    let moves: Vec<kicad_ipc::FootprintMove> = result
        .placements
        .iter()
        .filter(|placement| new.contains(placement.reference.as_str()))
        .map(|placement| kicad_ipc::FootprintMove {
            reference: placement.reference.clone(),
            x_nm: kicad_ipc::units::mm_to_nm(placement.at.x),
            y_nm: kicad_ipc::units::mm_to_nm(placement.at.y),
            rotation_deg: Some(placement.rotation),
        })
        .collect();
    crate::place::write_placement(ctx, &moves)?;
    Ok(result
        .placements
        .iter()
        .filter(|placement| new.contains(placement.reference.as_str()))
        .map(|placement| {
            json!({
                "reference": placement.reference,
                "x": placement.at.x,
                "y": placement.at.y,
                "rotation": placement.rotation,
            })
        })
        .collect())
}

// ── the guard ───────────────────────────────────────────────────────────────

/// Where a board sync's pre-edit copies live, one per committed sync.
fn undo_dir(ctx: &AgentRuntime) -> PathBuf {
    ctx.project_dir().join(".gordian").join("pcb-undo")
}

fn snapshot(ctx: &AgentRuntime, original: &str) -> std::io::Result<String> {
    let dir = undo_dir(ctx);
    std::fs::create_dir_all(&dir)?;
    let next = 1 + std::fs::read_dir(&dir)?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            entry
                .path()
                .file_stem()?
                .to_str()?
                .strip_prefix("pcb-")?
                .parse::<u32>()
                .ok()
        })
        .max()
        .unwrap_or(0);
    let id = format!("pcb-{next}");
    std::fs::write(dir.join(format!("{id}.kicad_pcb")), original)?;
    Ok(id)
}

/// The one board invariant: copper may connect less than the schematic asks,
/// never more. Returns the refusal when the synced board shorts two nets, after
/// restoring the snapshot; `None` when the board is honest or the check could
/// not run.
fn guard(ctx: &AgentRuntime, path: &Path, original: &str, revision: &str) -> Option<Value> {
    let board = crate::active_board(ctx).ok()?;
    let shorts: Vec<Value> = pcb_drc::connectivity::check(&board.problem, &board.copper)
        .into_iter()
        .filter_map(|violation| match violation {
            Violation::CrossNetMerge { a, b } => Some(json!({ "a": a, "b": b })),
            Violation::Unconnected { .. } => None,
        })
        .collect();
    if shorts.is_empty() {
        return None;
    }
    ctx.close_kicad_session();
    let restored = std::fs::write(path, original).is_ok();
    Some(json!({
        "ok": false,
        "error": "sync_board refused: the synced board's copper would short nets the schematic keeps apart",
        "shorts": shorts,
        "restored": restored,
        "revision": revision,
        "note": "the board is back to its pre-sync state — delete the offending copper with delete_copper, then sync_board again",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schematic(
        reference: &str,
        value: &str,
        footprint: &str,
        pads: &[(&str, &str)],
    ) -> SchematicPart {
        SchematicPart {
            reference: reference.into(),
            value: value.into(),
            footprint: footprint.into(),
            pad_nets: pads
                .iter()
                .map(|(pad, net)| ((*pad).to_owned(), (*net).to_owned()))
                .collect(),
        }
    }

    fn board(reference: &str, value: &str, lib_id: &str, pads: &[(&str, &str)]) -> BoardFootprint {
        BoardFootprint {
            reference: reference.into(),
            lib_id: lib_id.into(),
            value: value.into(),
            at: Point2::new(10.0, 10.0),
            rotation: 0.0,
            pad_nets: pads
                .iter()
                .map(|(pad, net)| ((*pad).to_owned(), (*net).to_owned()))
                .collect(),
        }
    }

    const R0805: &str = "Resistor_SMD:R_0805_2012Metric";
    const R0603: &str = "Resistor_SMD:R_0603_1608Metric";

    fn divider_schematic() -> Vec<SchematicPart> {
        vec![
            schematic("R1", "10k", R0805, &[("1", "VIN"), ("2", "SENSE")]),
            schematic("R2", "10k", R0805, &[("1", "SENSE"), ("2", "GND")]),
        ]
    }

    fn divider_board() -> Vec<BoardFootprint> {
        vec![
            board("R1", "10k", R0805, &[("1", "VIN"), ("2", "SENSE")]),
            board("R2", "10k", R0805, &[("1", "SENSE"), ("2", "GND")]),
        ]
    }

    #[test]
    fn an_agreeing_board_has_no_delta() {
        let delta = diff(&divider_schematic(), &divider_board());
        assert!(delta.is_empty());
        assert!(delta.nets_changed.is_empty());
    }

    #[test]
    fn a_new_part_is_added_and_names_its_nets() {
        let mut sch = divider_schematic();
        sch.push(schematic(
            "C1",
            "100n",
            "Capacitor_SMD:C_0603_1608Metric",
            &[("1", "VIN"), ("2", "GND")],
        ));
        let delta = diff(&sch, &divider_board());
        assert_eq!(delta.added, ["C1"]);
        assert!(delta.removed.is_empty());
        assert_eq!(delta.nets_changed, ["GND", "VIN"]);
        assert_eq!(delta.copper_invalidating(), BTreeSet::new());
    }

    #[test]
    fn a_deleted_part_is_removed_and_invalidates_its_copper() {
        let delta = diff(&divider_schematic()[..1], &divider_board());
        assert_eq!(delta.removed, ["R2"]);
        assert!(delta.added.is_empty());
        assert_eq!(delta.nets_changed, ["GND", "SENSE"]);
        assert_eq!(delta.copper_invalidating(), BTreeSet::from(["R2"]));
    }

    #[test]
    fn a_footprint_swap_subsumes_its_own_pads() {
        let mut sch = divider_schematic();
        sch[0].footprint = R0603.into();
        sch[0].value = "4.7k".into();
        let delta = diff(&sch, &divider_board());
        assert_eq!(
            delta.footprint_changed,
            [Change {
                reference: "R1".into(),
                from: R0805.into(),
                to: R0603.into(),
            }]
        );
        // The swap re-emits every pad with its net; it is not also a retarget,
        // and the value rides along on the re-emitted footprint.
        assert!(delta.pads_retargeted.is_empty());
        assert!(delta.value_changed.is_empty());
        assert!(delta.nets_changed.is_empty());
        assert_eq!(delta.copper_invalidating(), BTreeSet::from(["R1"]));
    }

    #[test]
    fn a_rewired_pad_is_retargeted_and_names_both_nets() {
        let mut sch = divider_schematic();
        sch[0].pad_nets.insert("2".into(), "MID".into());
        sch[1].pad_nets.insert("1".into(), "MID".into());
        let delta = diff(&sch, &divider_board());
        assert_eq!(
            delta.pads_retargeted,
            [
                PadRetarget {
                    reference: "R1".into(),
                    pad: "2".into(),
                    from: Some("SENSE".into()),
                    to: Some("MID".into()),
                },
                PadRetarget {
                    reference: "R2".into(),
                    pad: "1".into(),
                    from: Some("SENSE".into()),
                    to: Some("MID".into()),
                },
            ]
        );
        assert_eq!(delta.nets_changed, ["MID", "SENSE"]);
        assert_eq!(delta.copper_invalidating(), BTreeSet::from(["R1", "R2"]));
    }

    #[test]
    fn a_value_only_change_touches_no_geometry() {
        let mut sch = divider_schematic();
        sch[0].value = "4.7k".into();
        let delta = diff(&sch, &divider_board());
        assert_eq!(
            delta.value_changed,
            [Change {
                reference: "R1".into(),
                from: "10k".into(),
                to: "4.7k".into(),
            }]
        );
        assert!(delta.added.is_empty() && delta.removed.is_empty());
        assert!(delta.footprint_changed.is_empty());
        assert!(delta.pads_retargeted.is_empty());
        assert!(delta.nets_changed.is_empty());
        // Nothing about a value invalidates copper.
        assert_eq!(delta.copper_invalidating(), BTreeSet::new());
    }

    #[test]
    fn a_pad_that_loses_its_net_retargets_to_none() {
        let mut sch = divider_schematic();
        sch[0].pad_nets.remove("2");
        let delta = diff(&sch, &divider_board());
        assert_eq!(
            delta.pads_retargeted,
            [PadRetarget {
                reference: "R1".into(),
                pad: "2".into(),
                from: Some("SENSE".into()),
                to: None,
            }]
        );
    }
}
