//! `place::emit` — IR → millimetre orchestration: `gather` the parts, seed the
//! coarse grid into mm (`assign_cells`/`apply_cells`), drive the placement engine
//! (`emit_strategy`/`prepare_writer`), and assemble the routed `SchematicWriter`
//! (`build_writer`).

#![allow(clippy::items_after_test_module)]

use std::collections::{BTreeMap, BTreeSet};
use std::io;

use kicad::KicadInstallation;
use kicad_symbol::SymbolTable;
use kicad_symbol::geometry::SymbolGeometry;
use sch_check::model::{Component, Design, PinTarget};
use sch_check::{PinType, SymbolMeta, find_pin};

use crate::write::SchematicWriter;
use geom::Dir;
use sch_model::result::EmitOutput;

use super::*;
use sch_model::item::{Incidence, Item};

// The disjoint-set forest (over a caller-owned `parent` slice) lives in
// `geom::union_find`, shared with the desugar pin reconciler.
use sch_model::ir::LayoutIr;

// ---------------------------------------------------------------------------
// Compiler internal model.
// ---------------------------------------------------------------------------

/// Resolve a component's pins to (number, name, net) using the canonical
/// symbol-wide number-first resolver.
pub(crate) fn resolve_pins(
    comp: &Component,
    geom: &SymbolGeometry,
    meta: &SymbolMeta,
) -> Vec<(String, String, Option<String>)> {
    let targets = resolve_pin_targets(comp, meta);
    geom.pins
        .iter()
        .map(|pg| {
            let net = match targets.get(&pg.number) {
                Some(PinTarget::Net(n)) => Some(n.clone()),
                _ => None,
            };
            (pg.number.clone(), pg.name.clone(), net)
        })
        .collect()
}

fn resolve_pin_targets<'a>(
    comp: &'a Component,
    meta: &SymbolMeta,
) -> BTreeMap<String, &'a PinTarget> {
    let mut targets = BTreeMap::new();
    for (key, target) in &comp.pins {
        for pin in sch_check::pins::resolve(meta, key) {
            targets.insert(pin.number.clone(), target);
        }
    }
    for (unit, pins) in &comp.units {
        let Some(unit) = authored_unit_number(unit) else {
            continue;
        };
        for (key, target) in pins {
            for pin in sch_check::pins::resolve(meta, key)
                .into_iter()
                .filter(|pin| pin.unit == unit)
            {
                targets.entry(pin.number.clone()).or_insert(target);
            }
        }
    }
    targets
}

/// Circuit YAML names symbol units `A`, `B`, ... while KiCad geometry numbers
/// them 1, 2, ... . Numeric unit keys are accepted too for generated designs.
fn authored_unit_number(name: &str) -> Option<u8> {
    let name = name.trim();
    if let Ok(unit) = name.parse::<u8>() {
        return (unit > 0).then_some(unit);
    }
    let mut chars = name.chars();
    let unit = chars.next()?.to_ascii_uppercase();
    if chars.next().is_none() && unit.is_ascii_uppercase() {
        Some(unit as u8 - b'A' + 1)
    } else {
        None
    }
}

fn used_symbol_units(comp: &Component, geom: &SymbolGeometry, meta: &SymbolMeta) -> Vec<u8> {
    let targets = resolve_pin_targets(comp, meta);
    let mut units: Vec<u8> = geom
        .pins
        .iter()
        .filter(|pin| targets.contains_key(&pin.number))
        .map(|pin| pin.unit.max(1))
        .collect();
    units.sort_unstable();
    units.dedup();
    if units.is_empty() {
        units.push(1);
    }
    units
}

#[cfg(test)]
mod resolve_pin_tests {
    use super::*;
    use kicad_symbol::geometry::PinGeom;

    fn pin(number: &str, name: &str, unit: u8) -> PinGeom {
        PinGeom {
            number: number.to_owned(),
            name: name.to_owned(),
            at: [0.0, 0.0].into(),
            angle: 0.0,
            length: 2.54,
            unit,
        }
    }

    #[test]
    fn resolves_authored_multi_unit_pins_on_their_kicad_units() {
        let mut comp = Component {
            part: "Amplifier_Operational:LM324".to_owned(),
            ..Component::default()
        };
        comp.pins
            .insert("4".to_owned(), PinTarget::Net("VCC".to_owned()));
        comp.units
            .entry("A".to_owned())
            .or_default()
            .insert("OUT".to_owned(), PinTarget::Net("OUT_A".to_owned()));
        comp.units
            .entry("B".to_owned())
            .or_default()
            .insert("OUT".to_owned(), PinTarget::Net("OUT_B".to_owned()));
        comp.units
            .entry("B".to_owned())
            .or_default()
            .insert("7".to_owned(), PinTarget::NoConnect);
        let geom = SymbolGeometry {
            lib_id: comp.part.clone(),
            pins: vec![
                pin("1", "OUT", 1),
                pin("5", "OUT", 2),
                pin("7", "-", 2),
                pin("4", "V+", 5),
            ],
            raw_definition: String::new(),
        };
        let mut provider = SymbolTable::mock();
        provider.mock_add(
            &comp.part,
            vec![
                ("1", "OUT", PinType::Other, 1),
                ("5", "OUT", PinType::Other, 2),
                ("7", "-", PinType::Other, 2),
                ("4", "V+", PinType::Other, 5),
            ],
        );
        let meta = provider.symbol(&comp.part).unwrap();

        assert_eq!(
            resolve_pins(&comp, &geom, &meta),
            vec![
                ("1".to_owned(), "OUT".to_owned(), Some("OUT_A".to_owned())),
                ("5".to_owned(), "OUT".to_owned(), Some("OUT_B".to_owned())),
                ("7".to_owned(), "-".to_owned(), None),
                ("4".to_owned(), "V+".to_owned(), Some("VCC".to_owned())),
            ]
        );
        assert_eq!(
            used_symbol_units(&comp, &geom, &meta),
            [1, 2, 5],
            "an explicitly no-connected unit must still be placed"
        );
    }

    #[test]
    fn authored_unit_letters_and_numbers_map_to_kicad_units() {
        assert_eq!(authored_unit_number("A"), Some(1));
        assert_eq!(authored_unit_number("d"), Some(4));
        assert_eq!(authored_unit_number("5"), Some(5));
        assert_eq!(authored_unit_number("0"), None);
        assert_eq!(authored_unit_number("Unit A"), None);
    }
}

// ---------------------------------------------------------------------------
// Public entry.
// ---------------------------------------------------------------------------

/// A design lowered to the geometry the typesetter and the realiser share: the items to
/// draw, the net incidence over them, and the sheet intent.
pub struct Scene {
    pub items: Vec<Item>,
    pub inc: Incidence,
    pub ir: LayoutIr,
}

/// Lower `design` into a [`Scene`], inferring the sheet intent when the caller has none.
pub fn place_problem(
    env: &KicadInstallation,
    design: &Design,
    ir: Option<LayoutIr>,
) -> io::Result<Scene> {
    let items = gather(env, design)?;
    let inc = incidence(&items);
    let ir = ir.unwrap_or_else(|| super::super::infer::infer_ir(env, design));
    Ok(Scene { items, inc, ir })
}

/// Emit a complete `.kicad_sch`. Pass `ir: None` for connectivity inference (production);
/// `Some(ir)` for a hand-tuned / sidecar frame (validation fixtures).
pub fn emit_strategy(
    env: &KicadInstallation,
    design: &Design,
    ir: Option<LayoutIr>,
) -> io::Result<EmitOutput> {
    let (w, mut out) = prepare_writer(env, design, ir)?;
    out.sch = w.finish();
    // The OPEN half of truthfulness, read off the finished document with the same
    // extractor the live-edit gate uses — so a whole-sheet emit can never ship a rail
    // in islands that only `live::verify` would have caught. `net_shorts` above is the
    // other half.
    out.net_opens = match sch_doc::SchDoc::parse(&out.sch) {
        Ok(doc) => crate::live::verify(&doc, design).scattered,
        Err(e) => vec![format!("could not re-read the emitted sheet: {e}")],
    };
    for net in &out.net_opens {
        tracing::warn!(
            "{}: realised sheet leaves net {net} in islands",
            design.name.as_deref().unwrap_or("<unnamed>")
        );
    }
    Ok(out)
}

/// The hidden `ap_block` identity tag, so a later call can say `arrange{block}` about
/// parts it did not place itself, and a re-typeset can redraw the block's frame.
fn block_prop(it: &Item) -> Vec<(String, String)> {
    if it.block.is_empty() || sch_model::result::synthesized_block(&it.block) {
        return Vec::new();
    }
    vec![(sch_model::result::AP_BLOCK.to_string(), it.block.clone())]
}

/// Typeset `design` and build its FINALIZED writer (placed, routed, text-solved,
/// reframed) WITHOUT rendering it. Returns the prepared writer plus the readability
/// metadata; `EmitOutput.sch` and `net_opens` are left empty because both need the
/// finished document — [`emit_strategy`] fills them in.
#[tracing::instrument(
    skip_all,
    fields(design = design.name.as_deref().unwrap_or("<unnamed>"))
)]
pub(crate) fn prepare_writer(
    env: &KicadInstallation,
    design: &Design,
    ir: Option<LayoutIr>,
) -> io::Result<(SchematicWriter, EmitOutput)> {
    let mut problem = place_problem(env, design, ir)?;
    sch_flex::typeset(&mut problem.items, &problem.ir.trees);
    let ir = problem.ir.clone();

    let realizer = RoutedSheetRealizer::new(env, &problem.inc, &ir);
    let evaluator = RoutedEvaluator::new(realizer);
    let mut w = realizer.realize_writer(design.name.as_deref(), &problem.items)?;
    add_orphan_label_columns(&mut w, design, &problem.inc);
    w.set_frame(true);
    w.prepare();
    let warnings = w.layout_warnings();
    let crossings = evaluator.crossings(&problem.items);
    // The truthfulness invariant of the finished geometry, read back off the writer:
    // no point may carry two nets. Cheap next to the search, and it names the pair.
    let net_shorts: Vec<String> = super::net_conflicts(env, &w, &problem.items, &problem.inc)
        .iter()
        .map(ToString::to_string)
        .collect();
    for short in &net_shorts {
        tracing::warn!(
            "{}: realised sheet shorts nets — {short}",
            design.name.as_deref().unwrap_or("<unnamed>")
        );
    }
    Ok((
        w,
        EmitOutput {
            sch: String::new(),
            layout_warnings: warnings,
            crossings,
            net_shorts,
            net_opens: Vec::new(),
        },
    ))
}

/// Build the complete schematic writer for a placed `items`: symbols (+mirror),
/// no-connects on unconnected pins, all wiring (rails + routed signals), and ERC flags.
#[allow(clippy::too_many_arguments)]
pub fn build_writer(
    env: &KicadInstallation,
    title: Option<&str>,
    items: &[Item],
    inc: &Incidence,
    ir: &LayoutIr,
    needs_flag: &BTreeSet<String>,
    beside: sch_model::route::RouteScene,
) -> io::Result<SchematicWriter> {
    let mut w = SchematicWriter::new();
    w.set_beside(beside);
    if let Some(name) = title {
        w.set_title(name);
    }
    for it in items {
        // The item already carries its geometry — including for a part whose library
        // only this sheet has — so the writer never needs to look one up.
        w.register(&it.geom);
        w.add_symbol_full(
            env,
            &it.part,
            &it.refdes,
            &it.value,
            it.at,
            it.angle,
            it.footprint.as_deref(),
            &block_prop(it),
            None,
        )?;
        if it.unit != 1 {
            w.set_unit_last(it.unit);
        }
        if it.mirror {
            w.set_mirror_last();
        }
    }
    // Only a net with a SECOND pin gets wiring drawn to it, so only those endpoints are
    // ones a no-connect marker could sever. A lone pin on a net of its own is exactly
    // what the singleton no-connect in `route::route_signal` is for.
    let wired = inc
        .values()
        .filter(|pins| pins.len() >= 2)
        .flatten()
        .map(|(i, num)| (items[*i].refdes.as_str(), num.as_str()));
    w.declare_connected(env, wired)?;
    for it in items {
        for (num, _name, net) in &it.pins {
            if net.is_none() {
                w.add_no_connect(env, &it.refdes, num)?;
            }
        }
    }
    let mut flag_points: BTreeMap<String, ([f64; 2], f64)> = BTreeMap::new();
    wire(
        &crate::wire::ElbowRouter,
        env,
        &mut w,
        items,
        inc,
        ir,
        needs_flag,
        &mut flag_points,
    )?;
    for net in needs_flag {
        if let Some((at, angle)) = flag_points.get(net) {
            w.add_power_flag_at(env, &format!("#FLG_{net}"), *at, *angle)?;
        }
    }
    Ok(w)
}

/// Power nets needing a PWR_FLAG: every power-input pin's net and every declared
/// rail, minus any net already driven by a power-output pin (a regulator output,
/// say). KiCAD flags an undriven power-input pin as an error, so each such net
/// gets exactly one flag.
pub(crate) fn compute_needs_flag(
    env: &KicadInstallation,
    items: &[Item],
    ir: &LayoutIr,
) -> BTreeSet<String> {
    let provider = SymbolTable::from_symbol_dir(env.symbol_dir().to_path_buf());
    let (mut driven, mut power_input) = (BTreeSet::new(), BTreeSet::new());
    for it in items {
        let Some(meta) = provider.symbol(&it.part) else {
            continue;
        };
        for (num, _name, net) in &it.pins {
            if let (Some(net), Some(pm)) = (net, find_pin(&meta.pins, num)) {
                match pm.etype {
                    PinType::PowerOutput => {
                        driven.insert(net.clone());
                    }
                    PinType::PowerInput => {
                        power_input.insert(net.clone());
                    }
                    _ => {}
                }
            }
        }
    }
    let mut needs_flag: BTreeSet<String> = power_input;
    needs_flag.extend(ir.rails.keys().cloned());
    for net in &driven {
        needs_flag.remove(net);
    }
    needs_flag
}

// ---------------------------------------------------------------------------
// Gather + incidence.
// ---------------------------------------------------------------------------

pub(crate) fn gather(env: &KicadInstallation, design: &Design) -> io::Result<Vec<Item>> {
    let mut items = Vec::new();
    let provider = SymbolTable::from_symbol_dir(env.symbol_dir().to_path_buf());
    for (block_name, block) in &design.blocks {
        for (refdes, comp) in &block.components {
            if comp.dnp {
                continue;
            }
            // A power-symbol component (`power:GND`, `power:+5V`, …) is a power-net
            // DECLARATION, not a placed part: it tells the engine its net is a power
            // rail (via `NetAttrs.power`), and the rail/terminal drawing is emitted by
            // the power path (`emit_rail`), not as a gathered symbol. Skip it here.
            // A `label:*` component is likewise a net-LABEL declaration (marks a net a
            // port / draws its name), not a placed symbol — and has no geometry. Skip.
            if comp.part.starts_with("power:") || comp.part.starts_with("label:") {
                continue;
            }
            let geom = SymbolGeometry::load(env.symbol_dir(), &comp.part)?;
            let meta = provider.symbol(&comp.part).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("symbol metadata unavailable for {}", comp.part),
                )
            })?;
            let pins = resolve_pins(comp, &geom, &meta); // same order/len as geom.pins
            // An IC/connector (>=3 pins) with no authored value shows its part
            // name (the MPN) so the part is identifiable on the sheet — the
            // reference's "MCP1703A-3302" etc. Passives keep their authored value.
            let value = match comp.value.clone() {
                Some(v) if !v.is_empty() => v,
                _ if geom.pins.len() >= 3 => comp
                    .part
                    .rsplit(':')
                    .next()
                    .unwrap_or(&comp.part)
                    .to_string(),
                _ => String::new(),
            };
            // MULTI-UNIT SPLIT. A part's pins are spread across symbol units
            // (op-amp: A=1/2/3, B=5/6/7, power=4/8). KiCAD draws one unit per
            // placed instance, so we emit one Item per USED unit (a unit carrying
            // at least one assigned pin), each holding only that unit's pins. A
            // single-unit part collapses to exactly one Item (unit 1) — identical
            // to the old behaviour. Without this, only unit 1's pins ever reach the
            // netlist (the power pins and unit B silently vanish).
            let pin_unit: Vec<u8> = geom.pins.iter().map(|p| p.unit.max(1)).collect();
            let units = used_symbol_units(comp, &geom, &meta);
            for (k, &u) in units.iter().enumerate() {
                let unit_pins: Vec<(String, String, Option<String>)> = pins
                    .iter()
                    .zip(pin_unit.iter())
                    .filter(|pair| *pair.1 == u)
                    .map(|pair| pair.0.clone())
                    .collect();
                items.push(Item {
                    refdes: refdes.clone(),
                    block: block_name.clone(),
                    part: comp.part.clone(),
                    // Show the MPN/value on the FIRST placed unit only — N copies
                    // of "MCP6002" across the units would just be clutter.
                    value: if k == 0 { value.clone() } else { String::new() },
                    footprint: if k == 0 { comp.footprint.clone() } else { None },
                    geom: geom.clone(),
                    pins: unit_pins,
                    at: [0.0, 0.0].into(),
                    angle: 0.0,
                    unit: u,
                    mirror: false,
                    preseeded: false,
                });
            }
        }
    }
    Ok(items)
}

/// Draw every ORPHANED `label:global` net — one carried by label parts but by no
/// placed symbol on this sheet (so it never reaches `inc`) — as a clean, evenly
/// spaced vertical COLUMN of global labels. This rescues a pure/mostly-label
/// block (an external-I/O / pinout breakout sheet) from rendering blank: each
/// label part is geometry-free and skipped by `gather`, and the engine's port
/// pennants attach only to real pins, so without this such a sheet emits nothing.
///
/// The pennant text is the NET name — cross-sheet connectivity binds by net, so
/// the same name's global labels on this net's target sheets join to it, exactly
/// as a wired pin's port pennant would. (The label part's `value`, e.g. `CH1P`,
/// is a shorter human alias of the same net and carries no extra connectivity, so
/// the pennant's own net text is the clearer, complete annotation.) Labels are
/// ordered by the design's component order and deduplicated by net, so the
/// emission is deterministic. Coordinates are nominal: `reframe` shifts the whole
/// column to the page margin. No-op (byte-identical) when no net is orphaned.
pub(crate) fn add_orphan_label_columns(w: &mut SchematicWriter, design: &Design, inc: &Incidence) {
    // Orphaned nets in component order, deduplicated by net (first occurrence wins).
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut orphans: Vec<String> = Vec::new();
    for block in design.blocks.values() {
        for comp in block.components.values() {
            if comp.dnp || comp.part != "label:global" {
                continue;
            }
            for target in comp.pins.values() {
                if let PinTarget::Net(net) = target {
                    if inc.contains_key(net) || !seen.insert(net.clone()) {
                        continue; // already drawn (real pin) or already in the column
                    }
                    orphans.push(net.clone());
                }
            }
        }
    }
    if orphans.is_empty() {
        return;
    }
    // A readable single column: even vertical pitch, each pennant pointing right
    // (text reads outward). PITCH leaves a clear gap between the 1.27 mm-tall rows.
    //
    // The column's coordinates are nominal: on a whole sheet the drawing is translated
    // around them, so the column is always in clear air and is emitted as-is.
    //
    // A block placed INTO a sheet that already has content is the exception — it is
    // never reframed, so the column lands wherever those constants fall, on top of
    // whatever is there. An orphan net has no pin to reach for, so its label simply
    // BECOMES whatever it lands on: step each row down until its anchor is on nobody
    // else's net.
    const X: f64 = 25.4;
    const Y0: f64 = 25.4;
    const PITCH: f64 = 7.62;
    let mut scene = w.joins_existing_content().then(|| w.route_scene());
    for (i, net) in orphans.iter().enumerate() {
        let mut at = [X, Y0 + i as f64 * PITCH];
        if let Some(scene) = scene.as_mut() {
            for _ in 0..orphans.len().max(16) {
                if !crate::floorplan::place::route::anchor_merges(scene, at.into(), net) {
                    break;
                }
                at[1] += PITCH;
            }
            scene.points.push((at.into(), net.clone()));
        }
        w.add_cluster_label(net, at, Dir::East, true);
    }
}

/// net -> list of (item index, pin number).
pub(crate) fn incidence(items: &[Item]) -> Incidence {
    let mut inc: Incidence = BTreeMap::new();
    for (i, it) in items.iter().enumerate() {
        for (num, _name, net) in &it.pins {
            if let Some(net) = net {
                inc.entry(net.clone()).or_default().push((i, num.clone()));
            }
        }
    }
    inc
}

// ---------------------------------------------------------------------------
// Placement — the coarse (col,row,orient) grid rendered as a table.
// ---------------------------------------------------------------------------
