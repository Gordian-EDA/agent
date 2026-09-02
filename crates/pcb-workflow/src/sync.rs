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
//! Every run that writes goes through [`crate::board::guard`] like every other
//! board mutator: capture a revision, edit, re-check, then write or roll back.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use serde_json::{Value, json};

use geom::Rect;
use gordian_runtime::AgentRuntime;
use gordian_runtime::revisions::RevisionId;
use kicad_board::{BoardDoc, BoardFootprint};
use kicad_footprint::FootprintCatalog;
use pcb_model::Point2;
use pcb_place::PlacementHints;

use crate::board::guard::Guard;
use crate::seed::{PourPadConnection, PourSpec};

use crate::create::{
    BoardSeedSpec, SeedPart, SeedRules, add_default_power_pours,
    apply_complexity_default_layer_count, emit_board_footprint, merge, parse_seed_bounds,
    parse_seed_rules_over, plan_seed_board, write_seed_plan,
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
    if let Err(error) = crate::intent::parse(&input) {
        return Ok(json!({ "error": error }));
    }
    if !ctx.pcb_path().exists() {
        return Ok(create_board(&parts, &input, ctx));
    }
    // Intent is the shape of a board being built. On a board that already
    // exists sync has nothing to apply it to — pours are a `rules` change and
    // layout is placement's — so say where each half belongs rather than drop
    // it silently.
    if input.get("intent").is_some() {
        return Ok(json!({
            "error": "sync_board takes `intent` only when it creates the board. On an existing \
                      board pass the layout half to place_board({intent}) and any zones as \
                      rules {\"pours\": [{\"net\": …, \"layer\": …}]}.",
            "code": "intent_after_creation",
        }));
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

/// Create the board a project does not have yet. Every part is "added", and the
/// outline is sized from the parts' own courtyards unless the caller names one.
fn create_board(parts: &[SchematicPart], input: &Value, ctx: &AgentRuntime) -> Value {
    seed_board(parts, input, None, None, ctx)
}

/// Synthesize the board file. `base` is the rule set the caller's `rules`
/// overlay — `None` on a fresh board (the defaults), the board's own rules when
/// one is being rebuilt.
fn seed_board(
    parts: &[SchematicPart],
    input: &Value,
    base: Option<SeedRules>,
    revision: Option<RevisionId>,
    ctx: &AgentRuntime,
) -> Value {
    let catalog = match ctx.footprint_catalog() {
        Ok(catalog) => catalog,
        Err(e) => return json!({ "error": format!("footprint catalog unavailable: {e}") }),
    };
    let bounds = match parse_seed_bounds(input.get("bounds")) {
        Ok(bounds) => bounds,
        Err(e) => return json!({ "error": e }),
    };
    let rebuilding = base.is_some();
    let mut rules = match parse_seed_rules_over(base.unwrap_or_default(), input.get("rules")) {
        Ok(rules) => rules,
        Err(e) => return json!({ "error": e }),
    };
    let seed = seed_parts(parts);
    if !rebuilding {
        apply_complexity_default_layer_count(&mut rules, input.get("rules"), seed.len());
        if let Err(e) = add_intent_zones(&mut rules, input) {
            return json!({ "error": e });
        }
        add_default_power_pours(&mut rules, &seed);
    }

    let spec = BoardSeedSpec {
        bounds,
        rules,
        parts: seed,
        outline: None,
    };
    let plan = match plan_seed_board(&spec, catalog) {
        Ok(plan) => plan,
        Err(e) => return json!({ "error": e }),
    };
    let sizing = plan.sizing.clone();
    let width = plan.bounds.max_x - plan.bounds.min_x;
    let height = plan.bounds.max_y - plan.bounds.min_y;
    let sizes = json!({
        "required_bounds": { "width": sizing.required_w, "height": sizing.required_h },
        "recommended_bounds": { "width": sizing.recommended_w, "height": sizing.recommended_h },
        "applied_bounds": { "width": width, "height": height },
        "parts_courtyard_area_mm2": sizing.courtyard_area_mm2,
    });
    if plan.bounds_were_explicit && !sizing.fits(width, height) {
        let mut out = json!({
            "ok": false,
            "code": "bounds_below_required",
            "error": format!(
                "the requested {width} x {height} mm board is smaller than its own parts require: \
                 {} mm² of courtyards need at least {} x {} mm to pack legally. Call sync_board \
                 with bounds {{\"min_x\":0,\"min_y\":0,\"max_x\":{},\"max_y\":{}}} \
                 (recommended, with routing room), or omit bounds to size the board \
                 automatically. No board was written.",
                sizing.courtyard_area_mm2,
                sizing.required_w,
                sizing.required_h,
                sizing.recommended_w,
                sizing.recommended_h,
            ),
        });
        merge(&mut out, sizes);
        return out;
    }
    let outline = plan.bounds;
    let revision = match revision.map_or_else(
        || {
            ctx.revisions().capture(
                "sync_board",
                if rebuilding {
                    "Rebuild the project board"
                } else {
                    "Create the project board"
                },
                &[ctx.pcb_path()],
            )
        },
        Ok,
    ) {
        Ok(revision) => revision,
        Err(error) => {
            return json!({ "error": format!("could not capture the board before sync: {error}") });
        }
    };
    let seeded = match write_seed_plan(plan, ctx) {
        Ok(seeded) => seeded,
        Err(e) => return json!({ "error": e, "revision": revision }),
    };
    let mut out = json!({
        "ok": true,
        "created": true,
        "changed": true,
        "delta": BoardDelta {
            added: parts.iter().map(|p| p.reference.clone()).collect(),
            ..BoardDelta::default()
        }.to_json(),
        "part_count": parts.len(),
        "layer_count": seeded.rules.layer_count,
        "outline": bounds_json(&outline),
        "design_rules": {
            "clearance": seeded.rules.clearance,
            "min_trace_width": seeded.rules.min_trace_width,
            "via_diameter": seeded.rules.via_diameter,
            "via_drill": seeded.rules.via_drill,
        },
        "rules_from_footprints": seeded.rule_notes,
        "path": ctx.pcb_path().display().to_string(),
        "revision": revision,
        // Seeded is not placed: every part sits in the board's seed row until
        // place_board lays it out, and naming them is what makes that obvious.
        "unplaced": parts.iter().map(|part| part.reference.clone()).collect::<Vec<_>>(),
        "next_tool": "place_board",
        "note": "board created from the schematic with every part still unplaced — run \
                 place_board, then route_board, then check_board",
    });
    merge(&mut out, sizes);
    // The layout half of the intent is placement's to honour, not the seed's.
    // Hand it straight back so the next call carries it instead of losing it.
    if let Some(layout) = layout_intent(input) {
        merge(
            &mut out,
            json!({
                "next": format!(
                    "call place_board({{\"intent\": {layout}}}), then route_board, then check_board"
                ),
                "place_board_intent": layout,
            }),
        );
    }
    out
}

/// `intent.zones` names the nets that get a copper pour. A board with inner
/// layers pours on the last inner one, where a plane belongs; a two-layer board
/// pours on the bottom.
fn add_intent_zones(rules: &mut SeedRules, input: &Value) -> std::result::Result<(), String> {
    for net in crate::intent::parse(input)?.zones {
        if rules.pours.iter().any(|pour| pour.net == net) {
            continue;
        }
        let layer = if rules.layer_count >= 4 {
            format!("inner{}", rules.layer_count - 2)
        } else {
            "bottom".to_owned()
        };
        rules.pours.push(PourSpec {
            net,
            layer,
            pad_connection: PourPadConnection::Thermal,
        });
    }
    Ok(())
}

/// The placement half of an `intent`, if it has one.
fn layout_intent(input: &Value) -> Option<Value> {
    let mut intent = input.get("intent")?.as_object()?.clone();
    intent.remove("zones");
    (!intent.is_empty()).then_some(Value::Object(intent))
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
    if input.get("rules").is_some() || input.get("bounds").is_some() {
        return reseed_board(parts, input, ctx);
    }
    let catalog = match ctx.footprint_catalog() {
        Ok(catalog) => catalog,
        Err(e) => return json!({ "error": format!("footprint catalog unavailable: {e}") }),
    };
    if let Err(e) = ctx.kicad().save_if_open() {
        return json!({
            "error": format!("could not save the open KiCAD board before syncing: {e}"),
        });
    }
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

    let duplicates = doc.duplicate_references();
    if !duplicates.is_empty() {
        return json!({
            "error": format!(
                "the board has more than one footprint for {}; a part-by-part sync cannot tell \
                 them apart. Delete the duplicates in pcbnew first.",
                duplicates.join(", ")
            ),
        });
    }
    let delta = diff(parts, &doc.footprints());
    if delta.is_empty() {
        let result = json!({
            "ok": true,
            "delta": delta.to_json(),
            "changed": false,
            "note": "the board already matches the schematic; nothing was written.",
        });
        return result;
    }

    let gate = match Guard::open(
        ctx,
        "sync_board",
        "Synchronize the project board",
        &[ctx.pcb_path()],
    ) {
        Ok(gate) => gate,
        Err(refusal) => return refusal,
    };

    let existing: BTreeMap<String, BoardFootprint> = doc
        .footprints()
        .into_iter()
        .map(|fp| (fp.reference.clone(), fp))
        .collect();
    let by_reference: BTreeMap<&str, &SchematicPart> = parts
        .iter()
        .map(|part| (part.reference.as_str(), part))
        .collect();
    let seed_origin = Point2::new(
        before.imported.bounds.min_x + 2.0,
        before.imported.bounds.min_y + 2.0,
    );
    if let Err(e) = apply(
        &mut doc,
        &delta,
        &by_reference,
        &existing,
        seed_origin,
        catalog,
    ) {
        return gate.rollback(ctx, json!({ "error": e }));
    }

    if let Err(e) = write_board(ctx, &doc.into_text()) {
        return gate.rollback(ctx, json!({ "error": e }));
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
        return gate.rollback(
            ctx,
            json!({ "error": format!("the board was synced but its copper was not retracted: {e}") }),
        );
    }

    let placed = match place_added(&delta.added, ctx) {
        Ok(placed) => placed,
        Err(e) => return gate.rollback(ctx, json!({ "error": e })),
    };

    let mut nets_to_reroute: BTreeSet<String> = retract.nets.clone();
    nets_to_reroute.extend(delta.nets_changed.iter().cloned());

    let result = json!({
        "ok": true,
        "changed": true,
        "delta": delta.to_json(),
        "placed": placed,
        "retracted_tracks": retract.count,
        "nets_to_reroute": nets_to_reroute,
        "next_tool": "route_board",
        "next": "call route_board({nets: nets_to_reroute}), then check_board",
        "note": "only the delta was applied; every part the delta did not name kept its position \
                 and its copper. place_board() lays out anything still unplaced and leaves the \
                 rest alone.",
    });
    gate.commit(ctx, result)
}

/// Rebuild the board under new rules or a new outline, keeping every part where
/// it sits.
///
/// Clearance, trace width, via size and layer count are the board's fabric, not
/// its netlist: they cannot be patched into an existing document one node at a
/// So a rules change re-synthesizes the board and restores the placement — the
/// layout survives, the copper does not, and the model re-routes.
fn reseed_board(parts: &[SchematicPart], input: &Value, ctx: &AgentRuntime) -> Value {
    let gate = match Guard::open(
        ctx,
        "sync_board",
        "Rebuild the project board",
        &[ctx.pcb_path()],
    ) {
        Ok(gate) => gate,
        Err(refusal) => return refusal,
    };
    let revision = gate.revision();
    if let Err(e) = ctx.kicad().save_if_open() {
        return json!({
            "error": format!("could not save the open KiCAD board before rebuilding it: {e}"),
            "revision": revision,
        });
    }
    let before = match crate::active_board(ctx) {
        Ok(board) => board,
        Err(e) => return json!({ "error": e }),
    };
    let path = ctx.pcb_path();
    let original = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) => return json!({ "error": format!("could not read the board: {e}") }),
    };
    let doc = match BoardDoc::parse(original.clone()) {
        Ok(doc) => doc,
        Err(e) => return json!({ "error": format!("could not read the board document: {e}") }),
    };
    if !doc.outline_is_rectangular() {
        return json!({
            "error": "this board has a drawn outline, which a rules rebuild cannot reproduce. \
                      Change the rules in pcbnew, or square the outline with update_board_outline \
                      first.",
        });
    }
    let delta = diff(parts, &doc.footprints());
    let poses: Vec<kicad_ipc::FootprintMove> = doc
        .footprints()
        .into_iter()
        .filter(|fp| parts.iter().any(|part| part.reference == fp.reference))
        .map(|fp| kicad_ipc::FootprintMove {
            reference: fp.reference,
            x_nm: kicad_ipc::units::mm_to_nm(fp.at.x),
            y_nm: kicad_ipc::units::mm_to_nm(fp.at.y),
            rotation_deg: Some(fp.rotation),
        })
        .collect();
    // Keep the outline the board already has unless the caller asked for another.
    let mut seed_input = input.clone();
    if seed_input.get("bounds").is_none()
        && let Some((min_x, min_y, max_x, max_y)) = kicad_board::board_outline_bbox(&original)
    {
        seed_input["bounds"] = bounds_json(&Rect {
            min_x,
            min_y,
            max_x,
            max_y,
        });
    }
    let mut result = seed_board(
        parts,
        &seed_input,
        Some(board_rules(&before)),
        Some(revision),
        ctx,
    );
    if result.get("ok").and_then(Value::as_bool) != Some(true) {
        return result;
    }
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) => return json!({ "error": format!("could not read the reseeded board: {e}") }),
    };
    match kicad_board::patch_placements(&text, &poses).and_then(|placed| write_board(ctx, &placed))
    {
        Ok(()) => {}
        Err(e) => {
            return gate.rollback(
                ctx,
                json!({
                    "error": format!(
                        "the board was reseeded but its placement was not restored: {e}"
                    ),
                }),
            );
        }
    }
    merge(
        &mut result,
        json!({
            "created": false,
            "reseeded": true,
            // The rebuild restored every part where it sat, so nothing is
            // waiting on placement — only the copper is.
            "unplaced": Vec::<String>::new(),
            "delta": delta.to_json(),
            "retracted_tracks": original.matches("(segment").count(),
            "nets_to_reroute": delta_nets(parts),
            "next_tool": "route_board",
            "next": "call route_board(), then check_board",
            "note": "the board was rebuilt under the new rules with every part kept at its \
                     position; all copper was dropped because the old route is not honest under \
                     the new rules — run route_board, then check_board",
        }),
    );
    gate.commit(ctx, result)
}

/// The rules the board is already built with, as the starting point a rules
/// change overlays. Everything here is what the board file itself declares.
fn board_rules(board: &kicad_board::IpcBoardSnapshot) -> SeedRules {
    let layer_count = board.problem.layer_count;
    let pours = board
        .problem
        .plane_nets
        .iter()
        .map(|(net, layer)| PourSpec {
            net: net.clone(),
            layer: match *layer {
                0 => "top".to_owned(),
                n if n + 1 == layer_count => "bottom".to_owned(),
                n => format!("inner{n}"),
            },
            pad_connection: PourPadConnection::Thermal,
        })
        .collect();
    SeedRules {
        clearance: board.problem.clearance,
        min_trace_width: board.problem.min_trace_width,
        via_diameter: board.problem.via_diameter,
        via_drill: board.problem.via_drill,
        layer_count,
        net_widths: board.problem.net_widths.clone(),
        pours,
    }
}

/// Every net the schematic gives more than one pad — what a full re-route covers.
fn delta_nets(parts: &[SchematicPart]) -> Vec<String> {
    let mut pads: BTreeMap<&str, usize> = BTreeMap::new();
    for part in parts {
        for net in part.pad_nets.values() {
            *pads.entry(net.as_str()).or_default() += 1;
        }
    }
    pads.into_iter()
        .filter(|(_, count)| *count > 1)
        .map(|(net, _)| net.to_owned())
        .collect()
}

/// Replace the board document on disk.
///
/// The write is atomic — an interrupted sync must not leave half a board — and
/// the live session is dropped first: a cached pcbnew otherwise keeps serving
/// the old in-memory document for the same pathname and could save it back.
fn write_board(ctx: &AgentRuntime, text: &str) -> std::result::Result<(), String> {
    ctx.close_kicad_session();
    let path = ctx.pcb_path();
    crate::route::write_board_atomically(&path, text.as_bytes())
        .map_err(|e| format!("could not write {}: {e}", path.display()))
}

/// Write the delta into the board document.
fn apply(
    doc: &mut BoardDoc,
    delta: &BoardDelta,
    schematic: &BTreeMap<&str, &SchematicPart>,
    existing: &BTreeMap<String, BoardFootprint>,
    seed_origin: Point2,
    catalog: &FootprintCatalog,
) -> std::result::Result<(), String> {
    let wanted: BTreeSet<&str> = schematic
        .values()
        .flat_map(|part| part.pad_nets.values().map(String::as_str))
        .collect();
    let codes = doc.ensure_nets(wanted)?;

    // A swapped part is re-emitted from the new library footprint, which the
    // emitter only knows how to place on the front. Refuse rather than quietly
    // flip a back-side part to the front with front-side pads.
    if let Some(change) = delta.footprint_changed.iter().find(|c| {
        existing
            .get(&c.reference)
            .is_some_and(BoardFootprint::on_back)
    }) {
        return Err(format!(
            "{} sits on the back of the board, and sync_board can only re-emit a swapped \
             footprint on the front. Move it to the front, or change the package in pcbnew.",
            change.reference
        ));
    }

    for reference in delta
        .removed
        .iter()
        .chain(delta.footprint_changed.iter().map(|c| &c.reference))
    {
        if !doc.remove_footprint(reference)? {
            return Err(format!(
                "{reference} is not on the board; nothing was written"
            ));
        }
    }

    // A swap keeps the part where it sat, lock and all; a new part starts in the
    // board's own seed row and is placed properly below.
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
        let was = existing.get(reference);
        let (at, rotation) = was.map_or(
            (
                Point2::new(seed_origin.x + 2.54 * index as f64, seed_origin.y),
                0.0,
            ),
            |fp| (fp.at, fp.rotation),
        );
        let seed = SeedPart {
            reference: part.reference.clone(),
            value: Some(part.value.clone()),
            footprint: part.footprint.clone(),
            pad_nets: part.pad_nets.clone(),
            locked: None,
        };
        let locked = was.is_some_and(|fp| fp.locked);
        let block = emit_board_footprint(&seed, at, rotation, locked, catalog, &codes)?;
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
        if !doc.set_pad_net(&retarget.reference, &retarget.pad, net)? {
            return Err(format!(
                "{}.{} is a schematic pin with no pad on the board footprint — assign a package \
                 whose pads match the symbol's pins, then sync again",
                retarget.reference, retarget.pad
            ));
        }
    }

    for change in &delta.value_changed {
        if !doc.set_value(&change.reference, &change.to)? {
            return Err(format!(
                "{}'s board footprint has no Value field to update",
                change.reference
            ));
        }
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
    crate::place::restrict_to_refs(&mut problem, &board, &new);

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
            layer: "F.Cu".into(),
            locked: false,
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
