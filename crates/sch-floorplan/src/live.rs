//! `live` — editing a schematic that already exists.
//!
//! Three operations, all over an open [`SchDoc`] and all shaped for a tool call:
//! [`place_parts`] adds a connectivity-only block of parts, [`arrange`] re-places a
//! subset among its neighbours, and [`rewire`] redraws a subset's wiring where it
//! stands. The LLM states parts, nets and relative intent; every coordinate is the
//! solver's.
//!
//! ## The gate
//!
//! A placement is only correct if the sheet it drew means what the design said. Each
//! operation snapshots the document, edits it, and then re-extracts the net partition
//! with [`sch_doc::connect::extract`]:
//!
//! - the design's intended nets must land one-to-one on extracted nets — no net
//!   scattered across two, no two nets fused into one;
//! - nothing that was already on the sheet may be split, dropped or shorted.
//!
//! A failure restores the snapshot and comes back as [`Mismatch`], so a caller can
//! never commit a sheet that silently mis-wires. `arrange` and `rewire` promise more:
//! the partition must be *identical*, since moving a part is not supposed to mean
//! anything.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use geom::{Point2, Rect};
use kicad::KicadInstallation;
use kicad_symbol::SymbolTable;
use kicad_symbol::geometry::SymbolGeometry;
use sch_check::model::{Block, Component, Design, PinTarget};
use sch_check::{Diagnostics, PlacePartsInput};
use sch_doc::{Netlist, SchDoc, connect};
use sch_place::ir::LayoutIr;
use sch_place::item::Item;
use sch_place::result::IdiomReport;
use serde::{Deserialize, Serialize};

use crate::contract::{PlacementEngine, SchematicPlaceProblem};
use crate::floorplan::place::{incidence, item_rect};
use crate::region::{RegionProblem, arrange as region_arrange};

/// Block name the parts already on the sheet are lifted into. Prefixed so it cannot
/// collide with a block an author named.
const SHEET_BLOCK: &str = "$sheet";

/// Clearance added around a selected part when deciding which wires belong to it.
const TOUCH_MARGIN: f64 = 1.27;

/// Everything that can stop a live edit.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Doc(#[from] sch_doc::Error),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("the input describes no parts that can be placed")]
    Nothing,
    #[error("selection matched no symbol")]
    EmptySelection,
    #[error("input is not buildable: {0}")]
    Input(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Why an edit was rolled back: the connectivity it drew is not the connectivity it
/// promised. Empty means the edit stands.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mismatch {
    /// Intended nets whose pins did not all land on one extracted net.
    pub scattered: Vec<String>,
    /// `(a, b)` intended nets that landed on the same extracted net — a short.
    pub shorted: Vec<(String, String)>,
    /// Nets already on the sheet that the edit split, dropped or fused.
    pub disturbed: Vec<String>,
}

impl Mismatch {
    pub fn is_empty(&self) -> bool {
        self.scattered.is_empty() && self.shorted.is_empty() && self.disturbed.is_empty()
    }
}

/// What [`place_parts`] did.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlaceReport {
    /// Reference designators placed, in refdes order.
    pub placed: Vec<String>,
    /// Nets the new parts connect to, in name order.
    pub nets: Vec<String>,
    /// Readability warnings of the drawn sheet, plus any input diagnostics.
    pub warnings: Vec<String>,
    /// Circuit idioms the engine recognized and co-placed.
    pub idioms: Vec<IdiomReport>,
    /// Empty when the edit stands; otherwise the document was restored.
    pub mismatch: Mismatch,
    pub committed: bool,
}

/// What [`arrange`] or [`rewire`] did.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ArrangeReport {
    /// Reference designators the operation covered, in refdes order.
    pub moved: Vec<String>,
    /// Wires, junctions, labels and markers removed and redrawn.
    pub redrawn: usize,
    pub warnings: Vec<String>,
    /// Empty when the edit stands; otherwise the document was restored.
    pub mismatch: Mismatch,
    pub committed: bool,
}

/// Which symbols an operation applies to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Selection {
    /// By reference designator.
    Refs(Vec<String>),
    /// Every symbol whose origin falls inside `[x1, y1, x2, y2]` millimetres.
    Bbox([f64; 4]),
}

impl Selection {
    fn resolve(&self, doc: &SchDoc) -> BTreeSet<String> {
        match self {
            Selection::Refs(refs) => refs.iter().cloned().collect(),
            Selection::Bbox([x1, y1, x2, y2]) => {
                let box_ = Rect::new(x1.min(*x2), y1.min(*y2), x1.max(*x2), y1.max(*y2));
                doc.symbols()
                    .filter(|s| box_.contains(Point2::new(s.at.x, s.at.y)))
                    .map(|s| s.refdes().to_string())
                    .collect()
            }
        }
    }
}

/// An empty sheet, ready to be filled — what [`place_parts`] starts from when there is
/// no file yet. It is the realiser's own empty output, so a sheet created here and a
/// sheet KiCAD saved are the same kind of document.
pub fn blank_sheet() -> Result<SchDoc> {
    Ok(crate::realize::to_doc(crate::write::SchematicWriter::new())?)
}

/// Add `input`'s parts to `doc`, wired as it says and placed by `engine`.
///
/// An empty sheet is laid out whole; a sheet with content keeps every symbol it has and
/// the new parts are placed around them, avoiding their wires and labels. Either way the
/// result is gated (see the module docs) before it is kept.
pub fn place_parts(
    env: &KicadInstallation,
    doc: &mut SchDoc,
    input: &PlacePartsInput,
    engine: &dyn PlacementEngine,
) -> Result<PlaceReport> {
    let provider = SymbolTable::from_symbol_dir(env.symbol_dir().to_path_buf());
    let (added, diags) = sch_check::into_design(input, &provider);
    if diags.has_errors() {
        return Err(Error::Input(diagnostic_summary(&diags)));
    }
    let new_refs: BTreeSet<String> = added
        .blocks
        .values()
        .flat_map(|b| b.components.keys())
        .cloned()
        .collect();
    if new_refs.is_empty() {
        return Err(Error::Nothing);
    }

    let before = connect::extract(doc);
    let snapshot = doc.snapshot();
    let fresh = doc.symbols().next().is_none();

    let mut design = added;
    // A net the sheet already carries is joined by NAME: the new block hangs a label on
    // it rather than reaching across to a pin the region placement cannot draw to.
    for net in shared_nets(&design, &before) {
        design.nets.entry(net).or_default().port = true;
    }
    sch_check::nets::derive_attrs(&mut design);

    let mut ir = crate::floorplan::infer_ir(env, &design);
    if let Some(intent) = input.intent.clone() {
        apply_intent(&mut ir, intent.into_layout_ir());
    }

    let movable = SchematicPlaceProblem::from_design(env, &design)?.items;
    if movable.is_empty() {
        return Err(Error::Nothing);
    }
    let held = seated_items(doc, &before);

    let out = region_arrange(RegionProblem::new(
        env,
        &design,
        movable.clone(),
        held,
        obstacles(doc, &BTreeSet::new()),
        ir,
        engine,
    ));
    let placed = posed(movable, &out.poses);
    let inc = incidence(&placed);
    let writer = crate::realize::realize_block(
        env,
        &design,
        &placed,
        &inc,
        &out.ir,
        crate::realize::Draw {
            title: design.name.as_deref(),
            frame: fresh,
            ..Default::default()
        },
    )?;
    let warnings = writer.layout_warnings();
    crate::realize::graft(doc, writer)?;

    let mismatch = gate(doc, &design, &new_refs, &before);
    let committed = mismatch.is_empty();
    if !committed {
        doc.restore(snapshot)?;
    }
    Ok(PlaceReport {
        placed: new_refs.into_iter().collect(),
        nets: inc.keys().cloned().collect(),
        warnings,
        idioms: out.ir.idioms,
        mismatch,
        committed,
    })
}

/// Re-place `selection` among the parts around it, redrawing only its own wiring.
pub fn arrange(
    env: &KicadInstallation,
    doc: &mut SchDoc,
    selection: &Selection,
    engine: &dyn PlacementEngine,
) -> Result<ArrangeReport> {
    rearrange(env, doc, selection, Some(engine))
}

/// Redraw `selection`'s wiring where it stands, moving nothing.
pub fn rewire(
    env: &KicadInstallation,
    doc: &mut SchDoc,
    selection: &Selection,
) -> Result<ArrangeReport> {
    rearrange(env, doc, selection, None)
}

fn rearrange(
    env: &KicadInstallation,
    doc: &mut SchDoc,
    selection: &Selection,
    engine: Option<&dyn PlacementEngine>,
) -> Result<ArrangeReport> {
    let chosen = selection.resolve(doc);
    let before = connect::extract(doc);
    let mut design = Design::default();
    design
        .blocks
        .insert(SHEET_BLOCK.to_string(), lift_sheet(doc, &before));
    sch_check::nets::derive_attrs(&mut design);

    let (mut movable, held): (Vec<Item>, Vec<Item>) = seated_items(doc, &before)
        .into_iter()
        .partition(|it| chosen.contains(&it.refdes));
    if movable.is_empty() {
        return Err(Error::EmptySelection);
    }
    for it in &mut movable {
        it.frozen = false;
    }

    let ir = crate::floorplan::infer_ir(env, &design);
    let snapshot = doc.snapshot();
    let (placed, ir) = match engine {
        Some(engine) => {
            let out = region_arrange(RegionProblem::new(
                env,
                &design,
                movable.clone(),
                held,
                obstacles(doc, &chosen),
                ir,
                engine,
            ));
            (posed(movable, &out.poses), out.ir)
        }
        None => (movable, ir),
    };
    for part in &placed {
        seat(doc, part)?;
    }

    let footprint: Vec<Rect> = placed
        .iter()
        .map(|it| item_rect(it, it.at).inflate(TOUCH_MARGIN))
        .collect();
    let redrawn = doc.retain_drawing(|item| !touches(item, &footprint));

    let inc = incidence(&placed);
    let writer = crate::realize::realize_block(
        env,
        &design,
        &placed,
        &inc,
        &ir,
        crate::realize::Draw::default(),
    )?;
    let warnings = writer.layout_warnings();
    crate::realize::graft_drawing(doc, writer)?;

    let delta = Netlist::diff(&before, &connect::extract(doc));
    let mismatch = Mismatch {
        disturbed: disturbed(&delta),
        ..Default::default()
    };
    let committed = mismatch.is_empty();
    if !committed {
        doc.restore(snapshot)?;
    }
    Ok(ArrangeReport {
        moved: placed
            .iter()
            .map(|it| it.refdes.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        redrawn,
        warnings,
        mismatch,
        committed,
    })
}

/// Move a symbol in the document onto the pose the engine chose for it.
fn seat(doc: &mut SchDoc, item: &Item) -> Result<()> {
    let Some(uuid) = doc
        .symbols()
        .find(|s| s.refdes() == item.refdes && s.unit as u8 == item.unit)
        .map(|s| s.uuid.clone())
    else {
        return Ok(());
    };
    doc.move_symbol(&uuid, item.at.x, item.at.y)?;
    let mirror = if item.mirror {
        sch_doc::Mirror::Y
    } else {
        sch_doc::Mirror::None
    };
    doc.set_symbol_orientation(&uuid, item.angle, mirror)?;
    Ok(())
}

/// Whether a drawing item has any geometry inside one of `region`'s rectangles.
fn touches(item: &sch_doc::Item, region: &[Rect]) -> bool {
    let points: Vec<Point2> = match item {
        sch_doc::Item::Wire(w) => w.points.clone(),
        sch_doc::Item::Junction(j) => vec![j.at],
        sch_doc::Item::NoConnect(n) => vec![n.at],
        sch_doc::Item::Label(l) => vec![l.at.point()],
        sch_doc::Item::Text(t) => vec![t.at.point()],
        _ => Vec::new(),
    };
    points.iter().any(|p| region.iter().any(|r| r.contains(*p)))
}

/// Apply the caller's intent over the inferred IR, keeping everything the engine
/// derived for itself.
fn apply_intent(ir: &mut LayoutIr, intent: LayoutIr) {
    ir.flow = intent.flow;
    ir.rails.extend(intent.rails);
    ir.ports.extend(intent.ports);
    ir.place.extend(intent.place);
    ir.mirror.extend(intent.mirror);
    ir.relations.extend(intent.relations);
}

fn posed(mut items: Vec<Item>, poses: &[crate::region::Pose]) -> Vec<Item> {
    for (item, pose) in items.iter_mut().zip(poses) {
        item.at = pose.at;
        item.angle = pose.angle;
        item.mirror = pose.mirror;
    }
    items
}

/// The parts already on the sheet, as kernel components wired by the extracted netlist.
///
/// Power symbols are left out: they declare a rail rather than occupy a slot, and the
/// placement pipeline draws its own. They stay behind as obstacles.
fn lift_sheet(doc: &SchDoc, netlist: &Netlist) -> Block {
    let mut net_of: HashMap<(&str, &str), &str> = HashMap::new();
    for net in &netlist.nets {
        for pin in &net.pins {
            net_of.insert((&pin.refdes, &pin.pin), &net.name);
        }
    }
    let mut block = Block::default();
    for symbol in doc.symbols() {
        let refdes = symbol.refdes();
        if refdes.is_empty() || refdes.starts_with('#') || symbol.lib_id.starts_with("power:") {
            continue;
        }
        let comp = block
            .components
            .entry(refdes.to_string())
            .or_insert_with(|| Component {
                part: symbol.lib_id.clone(),
                value: Some(symbol.value().to_string()),
                footprint: symbol
                    .fields
                    .get("Footprint")
                    .map(|f| f.value.clone())
                    .filter(|f| !f.is_empty()),
                ..Component::default()
            });
        for number in symbol.pin_uuids.keys() {
            let target = match net_of.get(&(refdes, number.as_str())) {
                Some(net) => PinTarget::Net((*net).to_string()),
                None => PinTarget::NoConnect,
            };
            comp.pins.insert(number.clone(), target);
        }
    }
    block
}

/// Nets the new parts share with something already on the sheet.
fn shared_nets(design: &Design, before: &Netlist) -> Vec<String> {
    let live: BTreeSet<&str> = before.nets.iter().map(|n| n.name.as_str()).collect();
    design
        .blocks
        .values()
        .flat_map(|b| b.components.values())
        .flat_map(|comp| comp.pins.values())
        .filter_map(|target| match target {
            PinTarget::Net(net) if live.contains(net.as_str()) => Some(net.clone()),
            _ => None,
        })
        .collect()
}

/// The parts already on the sheet as frozen placement items, at their live poses.
///
/// Geometry comes from the definition the FILE embeds, so a sheet drawn with a
/// project-local library — which nothing outside that project can resolve — is still
/// something the placement can see and stay clear of.
fn seated_items(doc: &SchDoc, netlist: &Netlist) -> Vec<Item> {
    let mut net_of: HashMap<(&str, &str), &str> = HashMap::new();
    for net in &netlist.nets {
        for pin in &net.pins {
            net_of.insert((&pin.refdes, &pin.pin), &net.name);
        }
    }
    doc.symbols()
        .filter(|s| !placement_ignores(s))
        .filter_map(|symbol| {
            let definition = doc.lib_symbols()?.definition_text(&symbol.lib_id)?;
            let geom = SymbolGeometry::from_definition(&symbol.lib_id, &definition).ok()?;
            let unit = symbol.unit.clamp(1, u8::MAX as u32) as u8;
            let pins = geom
                .pins
                .iter()
                .filter(|p| p.unit == unit)
                .map(|p| {
                    let net = net_of
                        .get(&(symbol.refdes(), p.number.as_str()))
                        .map(|n| (*n).to_string());
                    (p.number.clone(), p.name.clone(), net)
                })
                .collect();
            Some(Item {
                refdes: symbol.refdes().to_string(),
                part: symbol.lib_id.clone(),
                value: symbol.value().to_string(),
                footprint: None,
                geom,
                pins,
                at: symbol.at.point(),
                angle: symbol.at.rot,
                unit,
                mirror: symbol.mirror == sch_doc::Mirror::Y,
                frozen: true,
            })
        })
        .collect()
}

/// Whether a placed symbol is furniture rather than a part: a power-rail terminal or
/// any other hidden-reference symbol the realiser draws for itself.
fn placement_ignores(symbol: &sch_doc::SymbolInst) -> bool {
    let refdes = symbol.refdes();
    refdes.is_empty() || refdes.starts_with('#') || symbol.lib_id.starts_with("power:")
}

/// What a placement must not land on: the wires, labels and power symbols that belong
/// to parts the operation is not touching.
fn obstacles(doc: &SchDoc, ignoring: &BTreeSet<String>) -> Vec<Rect> {
    let mut out = Vec::new();
    for wire in doc.wires() {
        for pair in wire.points.windows(2) {
            out.push(
                Rect::new(
                    pair[0].x.min(pair[1].x),
                    pair[0].y.min(pair[1].y),
                    pair[0].x.max(pair[1].x),
                    pair[0].y.max(pair[1].y),
                )
                .inflate(TOUCH_MARGIN),
            );
        }
    }
    for label in doc.labels() {
        let width = crate::label::text_width(&label.text);
        let at = label.at.point();
        out.push(Rect::new(at.x, at.y - 1.6, at.x + width, at.y + 1.6));
    }
    for symbol in doc.symbols() {
        if !symbol.lib_id.starts_with("power:") || ignoring.contains(symbol.refdes()) {
            continue;
        }
        out.push(Rect::new(
            symbol.at.x - 2.54,
            symbol.at.y - 2.54,
            symbol.at.x + 2.54,
            symbol.at.y + 2.54,
        ));
    }
    out
}

/// Intended net → the design pins on it, as `REF.pin`.
fn intended(design: &Design, only: &BTreeSet<String>) -> BTreeMap<String, BTreeSet<String>> {
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for block in design.blocks.values() {
        for (refdes, comp) in &block.components {
            if !only.contains(refdes) || comp.dnp || !comp.part.contains(':') {
                continue;
            }
            if comp.part.starts_with("power:") || comp.part.starts_with("label:") {
                continue;
            }
            let unit_pins = comp.units.values().flat_map(|u| u.iter());
            for (pin, target) in comp.pins.iter().chain(unit_pins) {
                if let PinTarget::Net(net) = target {
                    out.entry(net.clone())
                        .or_default()
                        .insert(format!("{refdes}.{pin}"));
                }
            }
        }
    }
    out
}

/// The full gate: the design's nets landed one-to-one, and the sheet's own nets survived.
fn gate(doc: &SchDoc, design: &Design, new_refs: &BTreeSet<String>, before: &Netlist) -> Mismatch {
    let after = connect::extract(doc);
    // A pin's home: the extracted net it landed on, or a name unique to itself so two
    // loose ends never look like one net.
    let mut home: HashMap<String, String> = HashMap::new();
    for (index, net) in after.nets.iter().enumerate() {
        for pin in &net.pins {
            home.insert(format!("{}.{}", pin.refdes, pin.pin), index.to_string());
        }
    }
    let mut mismatch = Mismatch::default();
    let mut owner: HashMap<String, String> = HashMap::new();
    for (net, pins) in intended(design, new_refs) {
        let landed: BTreeSet<String> = pins
            .iter()
            .map(|pin| home.get(pin).cloned().unwrap_or_else(|| format!("~{pin}")))
            .collect();
        if landed.len() > 1 {
            mismatch.scattered.push(net.clone());
            continue;
        }
        let Some(one) = landed.into_iter().next() else {
            continue;
        };
        if let Some(other) = owner.insert(one, net.clone()) {
            mismatch.shorted.push((other, net));
        }
    }
    mismatch.disturbed = disturbed(&Netlist::diff(before, &after));
    mismatch
}

/// Names of pre-existing nets an edit broke: split apart, dropped, fused with another,
/// or left with a pin hanging.
fn disturbed(delta: &sch_doc::NetDelta) -> Vec<String> {
    let mut out: Vec<String> = delta
        .split
        .iter()
        .map(|(name, _)| name.clone())
        .chain(delta.removed.iter().cloned())
        .chain(delta.merged.iter().flat_map(|(sources, _)| sources.clone()))
        .chain(
            delta
                .pins_now_unconnected
                .iter()
                .map(|p| format!("{}.{}", p.refdes, p.pin)),
        )
        .collect();
    out.sort();
    out.dedup();
    out
}

fn diagnostic_summary(diags: &Diagnostics) -> String {
    diags
        .0
        .iter()
        .filter(|d| d.severity == sch_check::Severity::Error)
        .map(|d| format!("{}: {}", d.code, d.message))
        .collect::<Vec<_>>()
        .join("; ")
}
