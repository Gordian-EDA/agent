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
//! A bench addition attributes an already-fused pair to the sheet it received: its
//! authored labels still have to be exact, and the draw may not introduce a merge.
//!
//! A failure restores the snapshot and comes back as [`Mismatch`], so a caller can
//! never commit a sheet that silently mis-wires. `arrange` and `rewire` promise more:
//! the partition must be *identical*. When a wire redraw misses that bar, the original
//! route stays and same-named labels expose the layout debit without changing the netlist.
//!
//! ## The budget
//!
//! Every edit is also a promise about *time*: see [`PlacementBudget`]. Nothing is
//! written until the gate passes, so overrunning is always safe to refuse.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{RecvTimeoutError, sync_channel};
use std::thread;
use std::time::{Duration, Instant};

use geom::{Point2, Rect};
use kicad::KicadInstallation;
use kicad_symbol::SymbolTable;
use kicad_symbol::geometry::SymbolGeometry;
use sch_check::model::{Block, Component, Design, PinTarget};
use sch_check::{ExistingSheet, PayloadAudit, PlacePartsInput};
use sch_doc::{LabelKind, NetSource, Netlist, Pose, SchDoc, connect};
use sch_model::ir::LayoutIr;
use sch_model::item::{Incidence, Item};
use sch_model::place::{Deadline, PlaceOptions, PlacementEngineKind};
use sch_model::result::IdiomReport;
use serde::{Deserialize, Serialize};

use crate::floorplan::place::incidence;
use crate::region::{RegionProblem, arrange as region_arrange};
use sch_model::engine::PlacementEngine;
use sch_model::geometry::item_rect;
use sch_model::result::SHEET_BLOCK;

/// Clearance added around a selected part when deciding which wires belong to it.
const TOUCH_MARGIN: f64 = 1.27;

/// Sheet size at or above which the deterministic engine is the default.
///
/// Below it, `cluster`'s pose search and de-sprawl polish — each of which re-routes
/// and re-text-solves the whole sheet several times — still fit comfortably; above it
/// they are what turns a 20-second placement into a two-minute one.
const SPINE_ABOVE_PARTS: usize = 32;

/// Share of the budget the search may spend, leaving the rest for realising the
/// sheet, extracting its connectivity and gating it.
const SEARCH_SHARE: f64 = 0.7;

/// The wall-clock promise a live placement call makes, and the engine choice that
/// keeps it.
///
/// A placement search is unbounded in principle, so "how long may this take" is a
/// caller's decision, not the engine's. [`engine`](Self::engine) says which engine
/// keeps the promise; the deadlines behind it stop the search with room to spare.
///
/// `parts` is the size of the WHOLE SHEET the call leaves behind, not the size of
/// the block being placed. Every engine routes and text-solves the entire sheet per
/// candidate, so two new parts added to a 40-part sheet cost what 42 parts cost —
/// measured at 45 s for a two-part call the old block-sized rule sent to `cluster`.
///
/// Release measurements on this machine use exact-size slices of the campaign
/// payloads. Times include lowering, search, realise, and verification; all cells
/// committed truthfully under a 60 s ceiling.
///
/// | parts | campaign slice          | spine | cluster | call budget |
/// |------:|-------------------------|------:|--------:|------------:|
/// |    40 | bms-10s                 |  5.1s |   40.9s |         40s |
/// |    60 | openmyo-emg             | 10.5s |   45.3s |         45s |
/// |    90 | esp32-multifunction     | 14.6s |   30.7s |         45s |
///
/// The policy keeps roughly 50% headroom over the 26.8 s spine topology outlier in
/// the wider validation corpus: 15 s below 20 parts, 40 s through 59, and 45 s above
/// that. Runtime is topology-sensitive rather than monotonic in part count, so these
/// are broad envelopes rather than a fitted curve. Sheets reaching 60 parts are
/// expected to arrive as named blocks; each later block is placed as a region with
/// the existing sheet frozen.
///
/// Reproduce with `cargo run --release -p sch-floorplan --example place_bench --
/// <dir> --budget 60 --engines spine,cluster bms-10s@40 openmyo-emg@60
/// esp32-multifunction@90`.
#[derive(Debug, Clone, Copy)]
pub struct PlacementBudget {
    /// Wall time the whole call — search, realise, gate — may take.
    pub budget: Duration,
    /// Parts on the sheet the call leaves behind.
    pub parts: usize,
}

/// The two instants a [`PlacementBudget`] resolves to, or `None` for an unbounded
/// call. Built once at the top of an operation and consulted at its two decision
/// points: when the search must stop, and whether there is still time to realise.
#[derive(Debug, Clone, Copy, Default)]
struct Deadlines {
    search: Option<Deadline>,
    hard: Option<Deadline>,
}

#[derive(Clone)]
struct Phase(Arc<AtomicU8>);

impl Phase {
    const NAMES: [&'static str; 5] = ["lower", "audit", "place", "realise", "verify"];

    fn new() -> Self {
        Self(Arc::new(AtomicU8::new(0)))
    }

    fn enter(&self, name: &'static str) {
        let index = Self::NAMES
            .iter()
            .position(|candidate| *candidate == name)
            .expect("live phase has a published name");
        self.0.store(index as u8, Ordering::Release);
    }

    fn current(&self) -> &'static str {
        Self::NAMES[usize::from(self.0.load(Ordering::Acquire))]
    }
}

impl PlacementBudget {
    /// Largest default call budget, used by the outer agent timeout.
    pub const DEFAULT: Duration = Duration::from_secs(45);

    /// The measured wall-clock envelope for a call leaving `parts` on the sheet.
    pub fn new(parts: usize) -> Self {
        let seconds = match parts {
            0..=19 => 15,
            20..=59 => 40,
            _ => 45,
        };
        Self {
            budget: Duration::from_secs(seconds),
            parts,
        }
    }

    pub fn within(budget: Duration, parts: usize) -> Self {
        Self { budget, parts }
    }

    /// The engine that keeps this budget, honouring an explicit `requested` one.
    /// The caller builds it and hands it back to [`place_parts`] / [`arrange`].
    pub fn engine(&self, requested: Option<PlacementEngineKind>) -> PlacementEngineKind {
        requested.unwrap_or(if self.parts >= SPINE_ABOVE_PARTS {
            PlacementEngineKind::Spine
        } else {
            PlacementEngineKind::Cluster
        })
    }

    /// Whether a measured engine run fits in `remaining` with enough time to
    /// realise and verify its result.
    pub fn engine_fits(&self, engine: PlacementEngineKind, remaining: Duration) -> bool {
        let seconds = match engine {
            PlacementEngineKind::Spine => match self.parts {
                0..=19 => 8,
                20..=39 => 35,
                40..=59 => 20,
                60..=89 => 35,
                _ => 20,
            },
            PlacementEngineKind::Cluster => match self.parts {
                0..=19 => 10,
                20..=39 => 35,
                _ => 50,
            },
            PlacementEngineKind::Anneal => 55,
        };
        remaining >= Duration::from_secs(seconds)
    }

    /// The refusal a caller reports when the call overran: nothing was written.
    pub fn overrun(&self, elapsed: Duration, engine: &'static str, phase: &'static str) -> Error {
        Error::Budget {
            budget: self.budget,
            elapsed,
            parts: self.parts,
            engine,
            phase,
        }
    }
}

impl Deadlines {
    fn of(budget: Option<PlacementBudget>) -> Self {
        budget.map_or(Deadlines::default(), |b| Deadlines {
            search: Some(Deadline::after(b.budget.mul_f64(SEARCH_SHARE))),
            hard: Some(Deadline::after(b.budget)),
        })
    }
}

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
    #[error(
        "{engine} placement exceeded its {budget:?} budget after {elapsed:?} during {phase} \
         ({parts} parts on the sheet) — nothing was written; retry in smaller named blocks using \
         the `block` field"
    )]
    Budget {
        budget: Duration,
        elapsed: Duration,
        parts: usize,
        engine: &'static str,
        phase: &'static str,
    },
    #[error("invalid payload")]
    InvalidPayload(Box<PayloadAudit>),
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
    /// Parts nothing could resolve. They are not on the sheet; every other part
    /// is, and a net that only touched one of these is simply open.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unplaced: Vec<sch_check::place_parts::Unplaced>,
    /// Pins whose net carries no second pin — placed, but unfinished.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dangling: Vec<sch_check::place_parts::DanglingPin>,
    /// Dangling net → the existing net whose name it most resembles.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub did_you_mean: BTreeMap<String, String>,
    /// Empty when the edit stands; otherwise the document was restored.
    pub mismatch: Mismatch,
    pub committed: bool,
}

/// What [`arrange`] or [`rewire`] did.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ArrangeReport {
    /// Reference designators the operation covered, in refdes order.
    pub moved: Vec<String>,
    /// Symbols this call took off the bench, now laid out.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub left_bench: Vec<String>,
    /// Nets the redraw left as a matching LABEL because it could not draw a clean
    /// wire — the debit a re-layout pays instead of refusing.
    #[serde(default)]
    pub labelled: usize,
    /// Nets the redrawn wiring touches — the scope of what this call may rename.
    /// A net the selection drew as a label and now draws as a wire loses its
    /// authored name to KiCAD's derived one; the partition is unchanged, which is
    /// what the gate above actually checks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nets: Vec<String>,
    /// Wires, junctions, labels and markers removed and redrawn.
    pub redrawn: usize,
    pub warnings: Vec<String>,
    /// Empty when the edit stands; the final wire-or-label fallback clears it.
    pub mismatch: Mismatch,
    pub committed: bool,
}

/// Which symbols an operation applies to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Selection {
    /// By reference designator. The only form that reaches the bench: a benched
    /// symbol has no meaningful position and no block until it is arranged out.
    Refs(Vec<String>),
    /// Every symbol whose origin falls inside `[x1, y1, x2, y2]` millimetres.
    Bbox([f64; 4]),
    /// Every symbol tagged with this functional block, bench included.
    Block(String),
}

impl Selection {
    fn resolve(&self, doc: &SchDoc) -> BTreeSet<String> {
        match self {
            Selection::Refs(refs) => refs.iter().cloned().collect(),
            Selection::Bbox([x1, y1, x2, y2]) => {
                let box_ = Rect::new(x1.min(*x2), y1.min(*y2), x1.max(*x2), y1.max(*y2));
                doc.symbols()
                    .filter(|s| !crate::bench::is_benched(s))
                    .filter(|s| box_.contains(Point2::new(s.at.x, s.at.y)))
                    .map(|s| s.refdes().to_string())
                    .collect()
            }
            Selection::Block(name) => doc
                .symbols()
                .filter(|s| {
                    s.fields
                        .get(sch_model::result::AP_BLOCK)
                        .is_some_and(|field| field.value == *name)
                })
                .map(|s| s.refdes().to_string())
                .collect(),
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
///
/// `budget` is the promise the caller chose `engine` under (see [`PlacementBudget`]):
/// the search stops with time left to realise and gate, and a call still running past
/// the budget is refused with the document restored. `None` searches unbounded.
pub fn place_parts(
    env: &KicadInstallation,
    doc: &mut SchDoc,
    input: &PlacePartsInput,
    engine: Box<dyn PlacementEngine>,
    budget: Option<PlacementBudget>,
) -> Result<PlaceReport> {
    let input = input.clone();
    bounded_edit(
        env,
        doc,
        budget,
        engine.name(),
        move |env, doc, phase, deadlines| {
            place_parts_inner(&env, doc, &input, engine.as_ref(), phase, deadlines)
        },
    )
}

fn place_parts_inner(
    env: &KicadInstallation,
    doc: &mut SchDoc,
    input: &PlacePartsInput,
    engine: &dyn PlacementEngine,
    phase: &Phase,
    deadlines: Deadlines,
) -> Result<PlaceReport> {
    let provider = SymbolTable::from_symbol_dir(env.symbol_dir().to_path_buf());
    let before = live_phase(phase, "lower", input.parts.len(), 0, || {
        connect::extract(doc)
    });
    let existing = ExistingSheet {
        net_pins: before
            .nets
            .iter()
            .map(|net| (net.name.clone(), net.pins.len()))
            .collect(),
        refs: doc
            .symbols()
            .map(|symbol| symbol.refdes().to_string())
            .collect(),
        // The tool front end has already assigned every designator against the
        // project's reservations; nothing is minted here.
        reserved: BTreeSet::new(),
    };
    let (added, diags, mut audit) =
        live_phase(phase, "audit", input.parts.len(), before.nets.len(), || {
            sch_check::into_design(input, &provider, &existing)
        });
    tracing::info!(
        dangling = audit.dangling.len(),
        duplicate_refs = audit.duplicate_refs.len(),
        unknown_pins = audit.unknown_pins.len(),
        diagnostics = diags.0.len(),
        "schematic payload audited"
    );
    // One refusal reports every payload fault. Reporting the audit and the lowering
    // diagnostics in sequence made each layer mask the next, so a payload with a bad
    // lib_id and a bad net name cost two full resubmissions to discover.
    if !audit.is_valid() || diags.has_errors() {
        audit.input_errors = diags
            .0
            .iter()
            .filter(|d| d.severity == sch_check::Severity::Error)
            .map(|d| format!("{}: {}", d.code, d.message))
            .collect();
        return Err(Error::InvalidPayload(Box::new(audit)));
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
    let available = design
        .blocks
        .values()
        .flat_map(|block| block.components.keys().cloned())
        .chain(
            doc.symbols()
                .filter(|symbol| !placement_ignores(symbol))
                .map(|symbol| symbol.refdes().to_string()),
        )
        .collect();
    let mut intent_warnings = Vec::new();
    if let Some(intent) = input.intent.clone() {
        let (intent, warnings) = intent.into_layout_ir_for(&available);
        intent_warnings = warnings;
        apply_intent(&mut ir, intent);
    }
    // An unfinished single-pin net must not take the port convenience: a global label
    // reads as deliberate board I/O and silences KiCAD's own ERC, hiding the very gap
    // the audit just reported. Only a net the payload declared in `intent.ports` keeps it.
    let declared: BTreeSet<&str> = input
        .intent
        .iter()
        .flat_map(|intent| intent.ports.keys().map(String::as_str))
        .collect();
    ir.ports
        .retain(|net, _| declared.contains(net.as_str()) || !audit.dangling_nets().contains(net));

    let movable =
        crate::floorplan::place_problem(env, &design, Some(ir.clone()), PlaceOptions::default())?
            .items;
    if movable.is_empty() {
        return Err(Error::Nothing);
    }
    let held = seated_items(doc, &before);

    let out = live_phase(phase, "place", movable.len(), design.nets.len(), || {
        region_arrange(
            RegionProblem::new(
                env,
                &design,
                movable.clone(),
                held,
                obstacles(doc, &[]),
                ir,
                engine,
            )
            .by(deadlines.search),
        )
    });
    let placed = posed(movable, &out.poses);
    let inc = incidence(&placed);
    let was_global = global_label_nets(doc);
    let warnings = live_phase(
        phase,
        "realise",
        placed.len(),
        inc.len(),
        || -> Result<_> {
            let writer = crate::realize::realize_block(
                env,
                &design,
                &placed,
                &inc,
                &out.ir,
                crate::realize::Draw {
                    title: design.name.as_deref(),
                    frame: fresh,
                    driven: &driven_nets(doc, &before),
                    beside: (!fresh).then(|| beside_scene(doc)).as_ref(),
                },
            )?;
            let mut warnings = intent_warnings;
            warnings.extend(writer.layout_warnings());
            warnings.extend(net_conflict_warnings(env, &writer, &placed, &inc));
            crate::realize::graft(doc, writer)?;
            Ok(warnings)
        },
    )?;
    enforce_label_scopes(doc, &declared, &was_global);

    let mut mismatch = live_phase(phase, "verify", placed.len(), inc.len(), || {
        verify(doc, &design)
    });
    mismatch.disturbed = disturbed(&before, &connect::extract(doc));
    let committed = mismatch.is_empty();
    if !committed {
        doc.restore(snapshot)?;
    }
    Ok(PlaceReport {
        placed: new_refs.into_iter().collect(),
        nets: inc.keys().cloned().collect(),
        warnings,
        idioms: out.ir.idioms,
        unplaced: audit.unplaced,
        dangling: audit.dangling,
        did_you_mean: audit.did_you_mean.into_iter().collect(),
        mismatch,
        committed,
    })
}

/// What [`add_parts`] did.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AddReport {
    /// Symbols now on the bench, in refdes order.
    pub benched: Vec<crate::bench::Benched>,
    /// Nets the benched symbols connect to, in name order.
    pub nets: Vec<String>,
    /// KiCAD-derived net name → stable authored label written in its place.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub minted_for_derived: BTreeMap<String, String>,
    /// Parts nothing could resolve, so not even the bench can hold them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unplaced: Vec<sch_check::place_parts::Unplaced>,
    /// Pins whose net carries no second pin — added, but unfinished.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dangling: Vec<sch_check::place_parts::DanglingPin>,
    /// Dangling net → the existing net whose name it most resembles.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub did_you_mean: BTreeMap<String, String>,
    /// Empty when the edit stands; otherwise the document was restored.
    pub mismatch: Mismatch,
    pub committed: bool,
}

/// Add `input`'s parts to the sheet's BENCH: on the sheet and on their nets, named
/// at every pin, with no layout and no wire drawn.
///
/// This is the half of [`place_parts`] that cannot fail for want of a good drawing.
/// `only` narrows it to those references — how a placement that could not be drawn
/// truthfully keeps its connectivity anyway — and `why` is what the report says
/// about each benched symbol.
///
/// Unlike a placement this is not searched, so it takes no budget: seating a symbol
/// in the next free bench cell and hanging a label on each pin is linear work.
pub fn add_parts(
    env: &KicadInstallation,
    doc: &mut SchDoc,
    input: &PlacePartsInput,
    only: Option<&BTreeSet<String>>,
    why: &str,
) -> Result<AddReport> {
    let provider = SymbolTable::from_symbol_dir(env.symbol_dir().to_path_buf());
    let before = connect::extract(doc);
    let names_before = named_partitions(doc);
    let existing = ExistingSheet {
        net_pins: before
            .nets
            .iter()
            .map(|net| (net.name.clone(), net.pins.len()))
            .collect(),
        refs: doc
            .symbols()
            .map(|symbol| symbol.refdes().to_string())
            .collect(),
        reserved: BTreeSet::new(),
    };
    let (design, diags, mut audit) = sch_check::into_design(input, &provider, &existing);
    if !audit.is_valid() || diags.has_errors() {
        audit.input_errors = diags
            .0
            .iter()
            .filter(|d| d.severity == sch_check::Severity::Error)
            .map(|d| format!("{}: {}", d.code, d.message))
            .collect();
        return Err(Error::InvalidPayload(Box::new(audit)));
    }
    let snapshot = doc.snapshot();
    let source = sch_doc::SymbolSource::new(env.symbol_dir().to_path_buf());
    let mut report = AddReport::default();
    let mut nets = BTreeSet::new();
    let parts_by_block = bench_parts(&design, only, why);
    let all_parts: Vec<&crate::bench::BenchPart> = parts_by_block.values().flatten().collect();
    let minted_for_derived = crate::bench::minted_net_names(doc, &all_parts);
    for (block, parts) in parts_by_block {
        let benched = crate::bench::bench(doc, &source, &block, &parts, &minted_for_derived)?;
        report.benched.extend(benched.benched);
        nets.extend(parts.iter().flat_map(|part| {
            part.pins
                .values()
                .map(|net| minted_for_derived.get(net).unwrap_or(net).clone())
        }));
    }
    report.minted_for_derived = minted_for_derived;
    if report.benched.is_empty() {
        doc.restore(snapshot)?;
        return Err(Error::Nothing);
    }
    report.benched.sort_by(|a, b| a.refdes.cmp(&b.refdes));
    report.nets = nets.into_iter().collect();
    let after = connect::extract(doc);
    report.mismatch = verify_addition(doc, &design, &names_before, &report.minted_for_derived);
    report.mismatch.disturbed = disturbed(&before, &after);
    report.committed = report.mismatch.is_empty();
    if !report.committed {
        doc.restore(snapshot)?;
    }
    report.unplaced = std::mem::take(&mut audit.unplaced);
    report.dangling = std::mem::take(&mut audit.dangling);
    report.did_you_mean = std::mem::take(&mut audit.did_you_mean)
        .into_iter()
        .collect();
    Ok(report)
}

/// The lowered design as bench work, one batch per block.
fn bench_parts(
    design: &Design,
    only: Option<&BTreeSet<String>>,
    why: &str,
) -> BTreeMap<String, Vec<crate::bench::BenchPart>> {
    let mut out: BTreeMap<String, Vec<crate::bench::BenchPart>> = BTreeMap::new();
    for (block, contents) in &design.blocks {
        for (refdes, component) in &contents.components {
            if only.is_some_and(|only| !only.contains(refdes)) {
                continue;
            }
            out.entry(block.clone())
                .or_default()
                .push(crate::bench::BenchPart {
                    refdes: refdes.clone(),
                    lib_id: component.part.clone(),
                    value: component.value.clone().unwrap_or_default(),
                    footprint: component.footprint.clone(),
                    pins: component
                        .pins
                        .iter()
                        .chain(component.units.values().flatten())
                        .filter_map(|(number, target)| match target {
                            PinTarget::Net(net) => Some((number.clone(), net.clone())),
                            _ => None,
                        })
                        .collect(),
                    why: why.to_string(),
                });
        }
    }
    out
}

fn live_phase<T>(
    current: &Phase,
    phase: &'static str,
    parts: usize,
    nets: usize,
    run: impl FnOnce() -> T,
) -> T {
    current.enter(phase);
    let span = tracing::info_span!("sch_floorplan_phase", phase, parts, nets);
    let started = Instant::now();
    let result = span.in_scope(run);
    let elapsed_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    tracing::info!(parent: &span, elapsed_ms, "schematic engine phase finished");
    result
}

/// Re-place `selection` among the parts around it, redrawing only its own wiring,
/// under the same budget promise as [`place_parts`].
pub fn arrange(
    env: &KicadInstallation,
    doc: &mut SchDoc,
    selection: &Selection,
    intent: Option<sch_check::Intent>,
    engine: Box<dyn PlacementEngine>,
    budget: Option<PlacementBudget>,
) -> Result<ArrangeReport> {
    let selection = selection.clone();
    bounded_edit(
        env,
        doc,
        budget,
        engine.name(),
        move |env, doc, phase, deadlines| {
            rearrange_inner(
                &env,
                doc,
                &selection,
                intent,
                Some(engine.as_ref()),
                phase,
                deadlines,
            )
        },
    )
}

/// Redraw `selection`'s wiring where it stands, moving nothing, under the same hard
/// wall-clock promise as [`place_parts`].
pub fn rewire(
    env: &KicadInstallation,
    doc: &mut SchDoc,
    selection: &Selection,
    budget: Option<PlacementBudget>,
) -> Result<ArrangeReport> {
    let selection = selection.clone();
    bounded_edit(
        env,
        doc,
        budget,
        "rewire",
        move |env, doc, phase, deadlines| {
            rearrange_inner(&env, doc, &selection, None, None, phase, deadlines)
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn rearrange_inner(
    env: &KicadInstallation,
    doc: &mut SchDoc,
    selection: &Selection,
    intent: Option<sch_check::Intent>,
    engine: Option<&dyn PlacementEngine>,
    phase: &Phase,
    deadlines: Deadlines,
) -> Result<ArrangeReport> {
    let chosen = selection.resolve(doc);
    let (before, design, boundary) = live_phase(phase, "lower", chosen.len(), 0, || {
        let before = connect::extract(doc);
        let mut design = Design::default();
        design
            .blocks
            .insert(SHEET_BLOCK.to_string(), lift_sheet(doc, &before));
        // A net with a pin on both sides of the selection has to be reached by NAME:
        // the redraw draws the selection's own terminals only, so a wire run to where
        // a held pin's wire used to be reaches nothing.
        let boundary = boundary_nets(doc, &before, &chosen);
        for net in &boundary {
            if let Some(minted) = &net.mint {
                rename_net(&mut design, &net.name, minted);
            }
            design.nets.entry(net.drawn().to_string()).or_default().port = true;
        }
        // A net the sheet NAMED with a label keeps its name: the redraw erases that
        // label, and drawing the net as a bare wire instead would hand it back to
        // KiCAD's `Net-(…)` derivation — a rename the board's rules and pours would
        // then miss. Power nets are excluded; they draw their own rail symbols.
        for net in named_nets(&before, &chosen) {
            design.nets.entry(net).or_default().port = true;
        }
        sch_check::nets::derive_attrs(&mut design);
        (before, design, boundary)
    });

    let renames: BTreeMap<&str, &str> = boundary
        .iter()
        .filter_map(|net| Some((net.name.as_str(), net.mint.as_deref()?)))
        .collect();
    let (mut movable, held): (Vec<Item>, Vec<Item>) = seated_items(doc, &before)
        .into_iter()
        .map(|mut item| {
            for (_, _, net) in &mut item.pins {
                if let Some(name) = net.as_ref().and_then(|net| renames.get(net.as_str())) {
                    *net = Some((*name).to_string());
                }
            }
            item
        })
        .partition(|it| chosen.contains(&it.refdes));
    if movable.is_empty() {
        return Err(Error::EmptySelection);
    }
    for it in &mut movable {
        it.frozen = false;
        it.preseeded = false;
    }

    // The selection's own drawing is about to be erased and redrawn, so it must not
    // constrain the placement: obstacles are what is left once it is discounted.
    let mut owned = footprints(&movable);
    let mut ir = crate::floorplan::infer_ir(env, &design);
    let available = design
        .blocks
        .values()
        .flat_map(|block| block.components.keys().cloned())
        .collect();
    let mut intent_warnings = Vec::new();
    if let Some(intent) = intent {
        let (intent, warnings) = intent.into_layout_ir_for(&available);
        intent_warnings = warnings;
        apply_intent(&mut ir, intent);
    }
    let snapshot = doc.snapshot();
    // Read before the erase: a net whose only labels belong to the selection would
    // otherwise have no scope on record by the time the redraw needs one.
    let was_global = global_label_nets(doc);
    // The ports the boundary needs are the CALLER's contract, not a hint: an engine
    // is free to rewrite the IR it searched with, and one that drops them leaves the
    // redraw with a one-terminal net and nothing to name it.
    let boundary_ports: BTreeMap<String, sch_model::ir::Side> = boundary
        .iter()
        .map(|net| {
            let side = ir
                .ports
                .get(net.drawn())
                .copied()
                .unwrap_or(sch_model::ir::Side::Right);
            (net.drawn().to_string(), side)
        })
        .collect();
    let (placed, mut ir) = match engine {
        Some(engine) => {
            let out = live_phase(phase, "place", movable.len(), before.nets.len(), || {
                region_arrange(
                    RegionProblem::new(
                        env,
                        &design,
                        movable.clone(),
                        held.clone(),
                        obstacles(doc, &owned),
                        ir,
                        engine,
                    )
                    .by(deadlines.search),
                )
            });
            (posed(movable, &out.poses), out.ir)
        }
        None => (movable, ir),
    };
    ir.ports.extend(boundary_ports);
    let (mut redrawn, inc, mut warnings, left_bench, mut labelled) = live_phase(
        phase,
        "realise",
        placed.len(),
        before.nets.len(),
        || -> Result<_> {
            for part in &placed {
                seat(doc, part)?;
            }
            // Arranging is how a symbol leaves the bench: it now has a position
            // the layout chose, so it is an ordinary symbol again.
            let left_bench = crate::bench::unbench(doc, &chosen);
            owned.extend(footprints(&placed));
            let mut erase = selection_drawing(doc, &owned, &held);
            erase.extend(stale_frames(doc, &chosen));
            let redrawn = doc.retain_drawing(|item| !erase.contains(&drawing_key(item)));
            // The held half of a nameless boundary net needs the minted name too:
            // one label each side is what makes the two halves one net again.
            name_held_halves(doc, &boundary);
            let inc = incidence(&placed);
            let writer = crate::realize::realize_block(
                env,
                &design,
                &placed,
                &inc,
                &ir,
                crate::realize::Draw {
                    driven: &driven_nets(doc, &before),
                    beside: Some(&beside_scene_excluding(doc, &placed)),
                    ..Default::default()
                },
            )?;
            let mut warnings = intent_warnings;
            warnings.extend(writer.layout_warnings());
            warnings.extend(net_conflict_warnings(env, &writer, &placed, &inc));
            let labelled = writer.signal_label_count();
            crate::realize::graft_drawing(doc, writer)?;
            Ok((redrawn, inc, warnings, left_bench, labelled))
        },
    )?;
    // A re-wire declares no ports of its own, so every net keeps the scope the
    // sheet already gave it.
    enforce_label_scopes(doc, &BTreeSet::new(), &was_global);

    let mut mismatch = live_phase(phase, "verify", placed.len(), inc.len(), || Mismatch {
        disturbed: disturbed(&before, &connect::extract(doc)),
        ..Default::default()
    });
    if !mismatch.is_empty() {
        doc.restore(snapshot)?;
        let fallback = label_selection_debits(doc, &before, &chosen);
        enforce_label_scopes(doc, &BTreeSet::new(), &was_global);
        mismatch = Mismatch {
            disturbed: disturbed(&before, &connect::extract(doc)),
            ..Default::default()
        };
        if mismatch.is_empty() {
            warnings.push(format!(
                "wire redraw could not preserve the netlist cleanly; kept the original routes and added {} same-named pin labels",
                fallback
            ));
            redrawn = 0;
            labelled = fallback;
        } else {
            doc.restore(snapshot)?;
            warnings.push(
                "wire redraw and its label fallback could not improve the selection; left the original drawing unchanged"
                    .to_string(),
            );
            redrawn = 0;
            labelled = 0;
            mismatch = Mismatch::default();
        }
    }
    Ok(ArrangeReport {
        left_bench,
        labelled,
        nets: before
            .nets
            .iter()
            .filter(|net| net.pins.iter().any(|pin| chosen.contains(&pin.refdes)))
            .map(|net| net.name.clone())
            .chain(inc.keys().cloned())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        moved: placed
            .iter()
            .map(|it| it.refdes.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        redrawn,
        warnings,
        mismatch,
        committed: true,
    })
}

/// Add same-named pin labels at selected terminals whose clean redraw failed.
fn label_selection_debits(
    doc: &mut SchDoc,
    before: &Netlist,
    chosen: &BTreeSet<String>,
) -> usize {
    let pins = sch_doc::placed_pins(doc);
    let mut labelled = BTreeSet::new();
    for net in before
        .nets
        .iter()
        .filter(|net| net.pins.iter().any(|pin| chosen.contains(&pin.refdes)))
    {
        let kind = match net.source {
            NetSource::Global => LabelKind::Global,
            NetSource::Hier => LabelKind::Hier,
            _ => LabelKind::Local,
        };
        for member in net.pins.iter().filter(|pin| chosen.contains(&pin.refdes)) {
            let Some(pin) = pins.iter().find(|pin| {
                pin.refdes == member.refdes && pin.unit == member.unit && pin.number == member.pin
            }) else {
                continue;
            };
            let key = (net.name.clone(), coord(pin.at));
            if labelled.insert(key) {
                doc.add_label(kind, &net.name, Pose::new(pin.at.x, pin.at.y, 0.0));
            }
        }
    }
    labelled.len()
}

enum WorkerReply<T> {
    Completed(Result<T>, SchDoc),
    Panicked(Box<dyn std::any::Any + Send>),
}

fn bounded_edit<T, F>(
    env: &KicadInstallation,
    doc: &mut SchDoc,
    budget: Option<PlacementBudget>,
    engine: &'static str,
    run: F,
) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(KicadInstallation, &mut SchDoc, &Phase, Deadlines) -> Result<T> + Send + 'static,
{
    let started = Instant::now();
    let deadlines = Deadlines::of(budget);
    let phase = Phase::new();
    let worker_phase = phase.clone();
    let abandoned = Arc::new(AtomicBool::new(false));
    let worker_abandoned = abandoned.clone();
    let mut worker_doc = doc.clone();
    let worker_env = env.clone();
    let (send, receive) = sync_channel(1);
    thread::Builder::new()
        .name(format!("schematic-{engine}"))
        .spawn(move || {
            let reply = match catch_unwind(AssertUnwindSafe(|| {
                run(worker_env, &mut worker_doc, &worker_phase, deadlines)
            })) {
                Ok(result) => WorkerReply::Completed(result, worker_doc),
                Err(panic) => WorkerReply::Panicked(panic),
            };
            let receiver_gone = send.send(reply).is_err();
            if receiver_gone || worker_abandoned.load(Ordering::Acquire) {
                let elapsed = started.elapsed();
                let overran_ms = budget
                    .map(|limit| elapsed.saturating_sub(limit.budget).as_millis())
                    .unwrap_or_default()
                    .min(u128::from(u64::MAX)) as u64;
                tracing::warn!(
                    engine,
                    phase = worker_phase.current(),
                    elapsed_ms = elapsed.as_millis().min(u128::from(u64::MAX)) as u64,
                    placement.overran_ms = overran_ms,
                    "abandoned placement worker exited"
                );
            }
        })?;

    let reply = match deadlines.hard {
        Some(hard) => match receive.recv_timeout(hard.remaining()) {
            Ok(reply) => reply,
            Err(RecvTimeoutError::Timeout) => {
                abandoned.store(true, Ordering::Release);
                return Err(budget.expect("a hard deadline implies a budget").overrun(
                    started.elapsed(),
                    engine,
                    phase.current(),
                ));
            }
            Err(RecvTimeoutError::Disconnected) => unreachable!("worker always sends a reply"),
        },
        None => receive.recv().expect("worker always sends a reply"),
    };
    match reply {
        WorkerReply::Completed(result, completed_doc) => {
            if result.is_ok() {
                *doc = completed_doc;
            }
            result
        }
        WorkerReply::Panicked(panic) => resume_unwind(panic),
    }
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

/// The points a drawing item occupies — where it can join another.
fn anchors(item: &sch_doc::Item) -> Vec<Point2> {
    match item {
        sch_doc::Item::Wire(w) => w.points.clone(),
        sch_doc::Item::Junction(j) => vec![j.at],
        sch_doc::Item::NoConnect(n) => vec![n.at],
        sch_doc::Item::Label(l) => vec![l.at.point()],
        sch_doc::Item::Text(t) => vec![t.at.point()],
        sch_doc::Item::Symbol(s) => vec![s.at.point()],
        _ => Vec::new(),
    }
}

/// An item's identity for the erase set: its UUID.
fn drawing_key(item: &sch_doc::Item) -> String {
    match item {
        sch_doc::Item::Wire(w) => w.uuid.clone(),
        sch_doc::Item::Junction(j) => j.uuid.clone(),
        sch_doc::Item::NoConnect(n) => n.uuid.clone(),
        sch_doc::Item::Label(l) => l.uuid.clone(),
        sch_doc::Item::Text(t) => t.uuid.clone(),
        sch_doc::Item::Rectangle(r) => r.uuid.clone(),
        sch_doc::Item::Symbol(s) => s.uuid.clone(),
        // Sheets, `lib_symbols` and undecoded nodes are never offered to `retain_drawing`.
        sch_doc::Item::Sheet(_) | sch_doc::Item::LibSymbols(_) | sch_doc::Item::Other(_) => {
            String::new()
        }
    }
}

/// The block frames a re-arrange invalidates: every rectangle enclosing a part that is
/// about to move, with the caption and note drawn against it.
///
/// A frame is drawn around where a block's parts WERE; once they move it is a box around
/// the wrong thing, and nothing on this path can redraw it (the sheet is lifted as one
/// region, so the block names live only on the symbols). Dropping it is the honest
/// outcome — the next `place_parts` for that block draws it again.
fn stale_frames(doc: &SchDoc, chosen: &BTreeSet<String>) -> BTreeSet<String> {
    let moving: Vec<Rect> = doc
        .symbols()
        .filter(|s| chosen.contains(s.refdes()))
        .filter_map(|s| sch_doc::body_rect(doc, s))
        .collect();
    let stale: Vec<Rect> = doc
        .items()
        .iter()
        .filter_map(|item| match item {
            sch_doc::Item::Rectangle(r) => Some(Rect::from_points(r.start, r.end)),
            _ => None,
        })
        .filter(|frame| moving.iter().any(|body| frame.overlaps(body)))
        .collect();
    doc.items()
        .iter()
        .filter(|item| match item {
            sch_doc::Item::Rectangle(r) => stale.contains(&Rect::from_points(r.start, r.end)),
            // A caption sits just above its frame and a note just below it, so both are
            // caught by an inflated test rather than by containment.
            sch_doc::Item::Text(t) => stale
                .iter()
                .any(|f| f.inflate(FRAME_TEXT_REACH).contains(t.at.point())),
            _ => false,
        })
        .map(drawing_key)
        .collect()
}

/// How far outside its frame a block's caption or note is drawn.
const FRAME_TEXT_REACH: f64 = 6.35;

/// Quantise to 1 um, as the connectivity extractor does, so float dust never splits a
/// join.
fn coord(p: Point2) -> (i64, i64) {
    ((p.x * 1000.0).round() as i64, (p.y * 1000.0).round() as i64)
}

/// The drawing that belongs to the selection: everything reachable from the parts being
/// moved without passing through a pin of a part that is staying put.
///
/// Erasing only what sits inside the selection's own footprints cuts wire chains in
/// half and leaves the far end — a rail terminal, a label — hanging. Following the
/// joins instead takes the whole run, and stopping at a foreign pin is what keeps it
/// from swallowing the sheet through a shared ground.
fn selection_drawing(doc: &SchDoc, owned: &[Rect], held: &[Item]) -> BTreeSet<String> {
    // Only a HELD part's own pin stops the flood. Stopping at the rail symbols
    // outside the selection as well was tried, to stop a re-wire taking a held pin
    // off `GND`: it leaves the selection's own rails standing where they were and
    // KiCAD reports `pin_not_connected` on the sheet that comes back. A rail is
    // drawing, and the redraw owns all of it.
    let stop: BTreeSet<(i64, i64)> = held
        .iter()
        .flat_map(|it| {
            it.geom.pins.iter().filter(|p| p.unit == it.unit).map(|p| {
                coord(sch_model::geometry::pin_endpoint(p, it.at, it.angle, it.mirror).into())
            })
        })
        .collect();

    let items: Vec<(String, Vec<Point2>)> = doc
        .items()
        .iter()
        .filter(|item| sch_doc::is_drawing(item))
        .map(|item| (drawing_key(item), anchors(item)))
        .collect();

    let mut erase: BTreeSet<String> = items
        .iter()
        .filter(|(_, points)| points.iter().any(|p| owned.iter().any(|r| r.contains(*p))))
        .map(|(key, _)| key.clone())
        .collect();

    // Spread along shared coordinates until nothing new joins.
    loop {
        let front: BTreeSet<(i64, i64)> = items
            .iter()
            .filter(|(key, _)| erase.contains(key))
            .flat_map(|(_, points)| points.iter().map(|p| coord(*p)))
            .filter(|c| !stop.contains(c))
            .collect();
        let grown: BTreeSet<String> = items
            .iter()
            .filter(|(_, points)| points.iter().any(|p| front.contains(&coord(*p))))
            .map(|(key, _)| key.clone())
            .collect();
        if grown.is_subset(&erase) {
            return erase;
        }
        erase.extend(grown);
    }
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

/// Move `items` onto the poses the engine chose, matched by part identity rather than
/// by position in the list — an engine is free to reorder what it was handed.
fn posed(mut items: Vec<Item>, poses: &[crate::region::Pose]) -> Vec<Item> {
    let by_part: HashMap<(&str, u8), &crate::region::Pose> = poses
        .iter()
        .map(|pose| ((pose.refdes.as_str(), pose.unit), pose))
        .collect();
    for item in &mut items {
        if let Some(pose) = by_part.get(&(item.refdes.as_str(), item.unit)) {
            item.at = pose.at;
            item.angle = pose.angle;
            item.mirror = pose.mirror;
        }
    }
    items
}

/// The parts already on the sheet, as kernel components wired by the extracted netlist.
///
/// Power symbols are left out: they declare a rail rather than occupy a slot, and the
/// placement pipeline draws its own. They stay behind as obstacles.
fn lift_sheet(doc: &SchDoc, netlist: &Netlist) -> Block {
    let net_of = net_by_pin(netlist);
    // The pins come from the symbol's DEFINITION, not from the instance's `(pin …)`
    // children: an engine-written sheet carries none of those, and a lifted sheet
    // with no pins is a design with no connectivity — which is how a re-arrange
    // came to erase a wire and draw nothing back.
    let mut numbers: HashMap<&str, Vec<String>> = HashMap::new();
    for pin in sch_doc::placed_pins(doc) {
        if let Some(symbol) = doc.symbol_by_ref(&pin.refdes) {
            numbers
                .entry(symbol.refdes())
                .or_default()
                .push(pin.number.clone());
        }
    }
    let mut block = Block::default();
    for symbol in doc.symbols().filter(|s| !placement_ignores(s)) {
        let refdes = symbol.refdes();
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
        for number in numbers.get(refdes).into_iter().flatten() {
            let target = match net_of.get(&(refdes, number.as_str())) {
                Some(net) => PinTarget::Net((*net).to_string()),
                None => PinTarget::NoConnect,
            };
            comp.pins.insert(number.clone(), target);
        }
    }
    block
}

/// `(refdes, pin number)` → the net the extractor found it on.
fn net_by_pin(netlist: &Netlist) -> HashMap<(&str, &str), &str> {
    netlist
        .nets
        .iter()
        .flat_map(|net| {
            net.pins
                .iter()
                .map(move |pin| ((pin.refdes.as_str(), pin.pin.as_str()), net.name.as_str()))
        })
        .collect()
}

/// Nets a power-output pin on the sheet already drives — a `PWR_FLAG`, a regulator
/// output.
///
/// The realiser draws one `PWR_FLAG` per undriven power net among the items it is
/// drawing, and those are the only items it sees. Without this, an edit beside an
/// already-flagged rail lands a second flag on it and KiCAD reports two power outputs
/// connected. Call it once the drawing a re-wire owns has been erased, so a flag that
/// is about to be redrawn does not count.
fn driven_nets(doc: &SchDoc, netlist: &Netlist) -> Vec<String> {
    let drivers: BTreeSet<(String, String)> = sch_doc::placed_pins(doc)
        .into_iter()
        .filter(|pin| pin.etype == "power_out")
        .map(|pin| (pin.refdes, pin.number))
        .collect();
    netlist
        .nets
        .iter()
        .filter(|net| {
            net.pins
                .iter()
                .any(|pin| drivers.contains(&(pin.refdes.clone(), pin.pin.clone())))
        })
        .map(|net| net.name.clone())
        .collect()
}

/// The nets the sheet already names with a global label.
fn global_label_nets(doc: &SchDoc) -> BTreeSet<String> {
    doc.labels()
        .filter(|label| label.kind == sch_doc::LabelKind::Global)
        .map(|label| sch_doc::unescape(&label.text))
        .collect()
}

/// Give every net one label scope on the sheet, returning how many labels changed.
///
/// A block reaches an existing net by NAME, and the router draws that reach as a
/// port pennant while the same net's own pins keep plain stub labels — two scopes
/// for one name, which KiCAD reports as `same_local_global_label` and which do not
/// actually merge. The scope is decided per net, not per label: a net the payload
/// declared in `intent.ports` is board I/O and is global everywhere on the sheet;
/// every other net keeps whatever scope the sheet already used for it, so joining
/// by name from a later block can never promote a sheet-local net to a pennant.
fn enforce_label_scopes(
    doc: &mut SchDoc,
    declared: &BTreeSet<&str>,
    was_global: &BTreeSet<String>,
) -> usize {
    let mut scopes: BTreeMap<String, (bool, bool)> = BTreeMap::new();
    for label in doc.labels() {
        let seen = match label.kind {
            sch_doc::LabelKind::Local => (true, false),
            sch_doc::LabelKind::Global => (false, true),
            sch_doc::LabelKind::Hier => continue,
        };
        let entry = scopes
            .entry(sch_doc::unescape(&label.text))
            .or_insert((false, false));
        entry.0 |= seen.0;
        entry.1 |= seen.1;
    }
    let mut changed = 0;
    for (net, (local, global)) in scopes {
        let want = match declared.contains(net.as_str()) || was_global.contains(&net) {
            true => sch_doc::LabelKind::Global,
            false => sch_doc::LabelKind::Local,
        };
        // Two scopes always have to be settled. One scope is only touched when the
        // sheet has an opinion the drawing lost — a re-wire redraws a port's net
        // with plain labels, and the port is not the re-wire's to demote.
        let settle = local && (global || want == sch_doc::LabelKind::Global);
        if settle {
            changed += doc.set_label_scope(&net, want);
        }
    }
    changed
}

/// A net that straddles the selection: a pin inside it and a pin outside it.
///
/// These are what a partial re-arrange cannot draw as wires. The selection's own
/// drawing is erased and redrawn from the selection's terminals alone, so the pin
/// left standing is only still on the net if the net carries a name both halves
/// answer to.
struct BoundaryNet {
    /// The name the extractor gave the net before the edit.
    name: String,
    /// A stable name to give it, when the one it has is KiCAD's own derivation
    /// from its pins — writing THAT down forks the net the moment a pin moves.
    mint: Option<String>,
    /// Every pin of this net outside the selection. All of them are named, not
    /// just one: erasing the selection's drawing can cut the held side into
    /// pieces too, and a name on each pin is what puts it back together.
    held: Vec<Point2>,
}

impl BoundaryNet {
    /// The name the redraw should draw.
    fn drawn(&self) -> &str {
        self.mint.as_deref().unwrap_or(&self.name)
    }
}

/// Every net straddling `chosen`, with the name each will be drawn under.
fn boundary_nets(doc: &SchDoc, before: &Netlist, chosen: &BTreeSet<String>) -> Vec<BoundaryNet> {
    let at: HashMap<(String, String), Point2> = sch_doc::placed_pins(doc)
        .into_iter()
        .map(|pin| ((pin.refdes, pin.number), pin.at))
        .collect();
    before
        .nets
        .iter()
        .filter_map(|net| {
            // Only REAL parts hold a name in place. The rail terminals and flags a
            // drawing is made of carry a `#` reference and are replaced wholesale by
            // the redraw, so a label left on one of their pins would be left
            // floating in space — KiCAD's `label_dangling`.
            let held: Vec<Point2> = net
                .pins
                .iter()
                .filter(|pin| !chosen.contains(&pin.refdes) && !pin.refdes.starts_with('#'))
                .filter_map(|pin| at.get(&(pin.refdes.clone(), pin.pin.clone())).copied())
                .collect();
            if held.is_empty() || !net.pins.iter().any(|pin| chosen.contains(&pin.refdes)) {
                return None;
            }
            let lead = net
                .pins
                .iter()
                .find(|pin| !pin.refdes.starts_with('#'))
                .unwrap_or(&net.pins[0]);
            Some(BoundaryNet {
                mint: (net.source == sch_doc::NetSource::Auto)
                    .then(|| minted_net_name(&lead.refdes, &lead.pin)),
                name: net.name.clone(),
                held,
            })
        })
        .collect()
}

/// Give each surviving piece of a minted net's held side its name — one label per
/// piece, read off the sheet as it stands after the erase.
///
/// Labelling every held pin would work and would also litter a twenty-pin net with
/// twenty labels; labelling one pin per PARTITION is the same guarantee at the
/// smallest cost, and it is exact because erasing the selection's drawing is what
/// decides how many pieces there are.
fn name_held_halves(doc: &mut SchDoc, boundary: &[BoundaryNet]) {
    if !boundary.iter().any(|net| net.mint.is_some()) {
        return;
    }
    let piece_at: HashMap<(i64, i64), String> = connect::scene(doc)
        .points
        .into_iter()
        .map(|(at, piece)| (coord(at), piece))
        .collect();
    let mut named: BTreeSet<(&str, &str)> = BTreeSet::new();
    let mut labels = Vec::new();
    for net in boundary {
        let Some(minted) = net.mint.as_deref() else {
            continue;
        };
        for held in &net.held {
            let piece = piece_at.get(&coord(*held)).map_or("", String::as_str);
            if named.insert((piece, minted)) {
                labels.push((minted.to_string(), *held));
            }
        }
    }
    for (name, at) in labels {
        doc.add_label(sch_doc::LabelKind::Local, &name, Pose::new(at.x, at.y, 0.0));
    }
}

/// Nets touching `chosen` that the sheet names with a label of its own.
fn named_nets(before: &Netlist, chosen: &BTreeSet<String>) -> Vec<String> {
    use sch_doc::NetSource::{Global, Hier, Local};
    before
        .nets
        .iter()
        .filter(|net| matches!(net.source, Local | Global | Hier))
        .filter(|net| net.pins.iter().any(|pin| chosen.contains(&pin.refdes)))
        .map(|net| net.name.clone())
        .collect()
}

/// The stable name given to a net that only had KiCAD's derived one.
///
/// Sanitised so it is a legal label: a name with a `(` or a `-` in it reads as
/// KiCAD's own generated form and cannot be joined by a caller.
fn minted_net_name(refdes: &str, number: &str) -> String {
    let sanitize = |text: &str| -> String {
        text.chars()
            .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
            .collect()
    };
    format!("N_{}_{}", sanitize(refdes), sanitize(number))
}

/// Rewrite every reference to `from` in the lifted design to `to`.
fn rename_net(design: &mut Design, from: &str, to: &str) {
    for block in design.blocks.values_mut() {
        for component in block.components.values_mut() {
            for target in component.pins.values_mut() {
                if matches!(target, PinTarget::Net(net) if net == from) {
                    *target = PinTarget::Net(to.to_string());
                }
            }
        }
    }
    if let Some(attrs) = design.nets.shift_remove(from) {
        design.nets.insert(to.to_string(), attrs);
    }
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
    let net_of = net_by_pin(netlist);
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
                block: symbol
                    .fields
                    .get(sch_model::result::AP_BLOCK)
                    .map(|f| f.value.clone())
                    .unwrap_or_default(),
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
                preseeded: true,
            })
        })
        .collect()
}

/// Lift the placed parts in a live document into the engine's geometry scene.
pub fn scene_items(doc: &SchDoc) -> Vec<Item> {
    seated_items(doc, &connect::extract(doc))
}

/// Whether a placed symbol is furniture rather than a part: a power-rail terminal or
/// any other hidden-reference symbol the realiser draws for itself.
fn placement_ignores(symbol: &sch_doc::SymbolInst) -> bool {
    let refdes = symbol.refdes();
    refdes.is_empty() || refdes.starts_with('#') || symbol.lib_id.starts_with("power:")
}

/// The bodies of `items` at their current poses, with the clearance a re-wire uses to
/// decide what belongs to them.
fn footprints(items: &[Item]) -> Vec<Rect> {
    items
        .iter()
        .map(|it| item_rect(it, it.at).inflate(TOUCH_MARGIN))
        .collect()
}

/// The truthfulness invariant of the drawn geometry, checked before the graft and
/// reported with the edit's own warnings: `mismatch.shorted` names the two nets that
/// merged, and this names the point and the geometry that merged them. Without it a
/// refusal is an engine failure with no cause attached. Normally empty — a hit here
/// means the edit is about to be refused.
fn net_conflict_warnings(
    env: &KicadInstallation,
    writer: &crate::write::SchematicWriter,
    items: &[Item],
    inc: &Incidence,
) -> Vec<String> {
    crate::floorplan::place::net_conflicts(env, writer, items, inc)
        .into_iter()
        .map(|conflict| format!("realised block shorts nets — {conflict}"))
        .collect()
}

/// What `doc` already carries, as foreign routing geometry for a block about to be drawn
/// beside it: every connection point and wire segment with the net it is on.
///
/// The realiser holds only the block it is drawing, so without this it routes as though
/// the sheet were blank — across the existing pins, and onto the existing labels. Empty
/// for a blank sheet, which makes a whole-sheet build byte-identical.
///
/// Symbol bodies are deliberately absent: the placement already keeps clear of them
/// (see [`obstacles`]), and a wire crossing a body reads badly but shorts nothing, which
/// is the question this scene answers.
fn beside_scene(doc: &SchDoc) -> sch_model::route::RouteScene {
    let scene = connect::scene(doc);
    sch_model::route::RouteScene {
        solids: Vec::new(),
        points: scene.points,
        segments: scene
            .segments
            .into_iter()
            .map(|(a, b, net)| sch_model::route::NetSegment::new(a, b, net))
            .collect(),
        label_solids: Vec::new(),
    }
}

/// The same scene for a RE-draw, where `redrawn`'s symbols are still seated in `doc` but
/// their wiring has just been erased.
///
/// Their own pins must not come back as foreign: stripped of wires each reads as an
/// unnamed one-pin net, which is foreign to everything — including to the block that is
/// about to wire it.
fn beside_scene_excluding(doc: &SchDoc, redrawn: &[Item]) -> sch_model::route::RouteScene {
    // `pin_endpoint` grid-snaps and the document's own pin geometry does not, so the two
    // are compared on the grid: a pin the snap moved is still the same pin.
    let key = |p: geom::Point2| {
        let p = geom::GRID_50_MIL.snap_point(p);
        ((p.x * 1000.0).round() as i64, (p.y * 1000.0).round() as i64)
    };
    let own: BTreeSet<(i64, i64)> = redrawn
        .iter()
        .flat_map(|item| {
            item.geom
                .pins
                .iter()
                .filter(move |pin| pin.unit.max(1) == item.unit.max(1))
                .map(move |pin| {
                    key(
                        sch_model::geometry::pin_endpoint(pin, item.at, item.angle, item.mirror)
                            .into(),
                    )
                })
        })
        .collect();
    let mut scene = beside_scene(doc);
    scene.points.retain(|(p, _)| !own.contains(&key(*p)));
    scene
}

/// What a placement must not land on: the drawing already on the sheet — wires, label
/// text, generated rail terminals — minus whatever falls inside `owned`, which the
/// caller is about to erase and draw again.
fn obstacles(doc: &SchDoc, owned: &[Rect]) -> Vec<Rect> {
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
        let width = sch_model::text::text_width(&label.text);
        let at = label.at.point();
        out.push(Rect::new(at.x, at.y - 1.6, at.x + width, at.y + 1.6));
    }
    for symbol in doc.symbols().filter(|s| placement_ignores(s)) {
        out.push(Rect::new(
            symbol.at.x - 2.54,
            symbol.at.y - 2.54,
            symbol.at.x + 2.54,
            symbol.at.y + 2.54,
        ));
    }
    out.retain(|r| !owned.iter().any(|o| o.overlaps(r)));
    out
}

/// Intended net → the design pins on it, as `REF.pin`.
fn intended(design: &Design) -> BTreeMap<String, BTreeSet<String>> {
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for block in design.blocks.values() {
        for (refdes, comp) in &block.components {
            if comp.dnp || !comp.part.contains(':') {
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

/// Check a drawn sheet against the design it is supposed to draw: every intended net's
/// pins on one extracted net, and no two intended nets on the same one.
///
/// This is the truthfulness question on its own, with no history involved — what
/// [`place_parts`] gates on, and what any caller can ask of a document it did not draw.
pub fn verify(doc: &SchDoc, design: &Design) -> Mismatch {
    let home = homes(&connect::extract(doc));
    let mut mismatch = Mismatch::default();
    let mut owner: HashMap<String, String> = HashMap::new();
    for (net, pins) in intended(design) {
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
    mismatch
}

/// Gate a bench draw against both its requested labels and the sheet it received.
///
/// Two requested nets may share the result only when their names already resolved
/// to one partition before the draw. This attributes an inherited alias correctly
/// while still refusing a merge made by the newly benched pins.
fn verify_addition(
    doc: &SchDoc,
    design: &Design,
    names_before: &BTreeMap<String, String>,
    minted_for_derived: &BTreeMap<String, String>,
) -> Mismatch {
    let after = connect::extract(doc);
    let home = homes(&after);
    let names_after = named_partitions(doc);
    let mut mismatch = Mismatch::default();
    let mut owner: HashMap<String, String> = HashMap::new();

    for (requested, pins) in intended(design) {
        let effective = minted_for_derived.get(&requested).unwrap_or(&requested);
        let target = names_after.get(effective).and_then(|partition| {
            after
                .nets
                .iter()
                .position(|net| net.name == *partition)
                .map(|index| index.to_string())
        });
        let landed: BTreeSet<String> = pins
            .iter()
            .map(|pin| home.get(pin).cloned().unwrap_or_else(|| format!("~{pin}")))
            .collect();
        let Some(target) = target.filter(|target| {
            landed.len() == 1 && landed.first().is_some_and(|landed| landed == target)
        }) else {
            mismatch.scattered.push(requested);
            continue;
        };

        if let Some(other) = owner.insert(target, requested.clone()) {
            let inherited = names_before
                .get(&other)
                .zip(names_before.get(&requested))
                .is_some_and(|(a, b)| a == b);
            if !inherited {
                mismatch.shorted.push((other, requested));
            }
        }
    }
    mismatch
}

/// Every authored name on a sheet mapped to the extracted partition it reaches.
fn named_partitions(doc: &SchDoc) -> BTreeMap<String, String> {
    let scene = connect::scene(doc);
    let points: BTreeMap<(i64, i64), String> = scene
        .points
        .into_iter()
        .map(|(point, partition)| (coord(point), partition))
        .collect();
    let mut out = BTreeMap::new();
    let mut note = |name: String, point: Point2| {
        if let Some(partition) = points.get(&coord(point)) {
            out.insert(name, partition.clone());
        }
    };
    for label in doc.labels() {
        note(sch_doc::unescape(&label.text), label.at.point());
    }
    let power_values: BTreeMap<String, String> = doc
        .symbols()
        .map(|symbol| (symbol.uuid.clone(), symbol.value().to_string()))
        .collect();
    for pin in sch_doc::placed_pins(doc)
        .into_iter()
        .filter(|pin| pin.etype == "power_in")
    {
        if pin.power_symbol {
            if let Some(value) = power_values.get(&pin.owner) {
                note(sch_doc::unescape(value), pin.at);
            }
        } else if pin.hidden {
            note(sch_doc::unescape(&pin.name), pin.at);
        }
    }
    for sheet in doc.items().iter().filter_map(|item| match item {
        sch_doc::Item::Sheet(sheet) => Some(sheet),
        _ => None,
    }) {
        for pin in &sheet.pins {
            note(sch_doc::unescape(&pin.name), pin.at.point());
        }
    }
    for net in connect::extract(doc).nets {
        out.entry(net.name.clone()).or_insert(net.name);
    }
    out
}

/// Where each pin of `netlist` ended up, as a comparable key: the net it is on, or a
/// name unique to the pin itself so two loose ends never read as one net.
///
/// Only real parts count. The rail terminals and PWR_FLAGs a drawing is made of carry a
/// hidden `#` reference and are the drawing's own business — a re-wire replaces them,
/// and that is not a change to the circuit.
fn homes(netlist: &Netlist) -> HashMap<String, String> {
    netlist
        .nets
        .iter()
        .enumerate()
        .flat_map(|(index, net)| {
            net.pins
                .iter()
                .map(move |pin| (format!("{}.{}", pin.refdes, pin.pin), index.to_string()))
        })
        .filter(|(pin, _)| !pin.starts_with('#'))
        .collect()
}

/// Names of nets the sheet already had that the edit broke: split apart, dropped, or
/// fused with another. A net that merely GAINED pins is not disturbed — that is what
/// adding a part to it looks like.
fn disturbed(before: &Netlist, after: &Netlist) -> Vec<String> {
    let home = homes(after);
    let mut out = Vec::new();
    let mut claimed: HashMap<String, String> = HashMap::new();
    for net in &before.nets {
        let landed: BTreeSet<String> = net
            .pins
            .iter()
            .map(|p| format!("{}.{}", p.refdes, p.pin))
            .filter(|pin| !pin.starts_with('#'))
            .map(|pin| home.get(&pin).cloned().unwrap_or(format!("~{pin}")))
            .collect();
        match landed.len() {
            0 => continue,
            1 => {
                let one = landed.into_iter().next().expect("just counted");
                if let Some(other) = claimed.insert(one, net.name.clone()) {
                    out.push(other);
                    out.push(net.name.clone());
                }
            }
            _ => out.push(net.name.clone()),
        }
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use sch_model::engine::{CandidateEvaluator, PlacementOutput, SchematicPlaceProblem};

    use super::*;

    struct SleepEngine;

    impl PlacementEngine for SleepEngine {
        fn name(&self) -> &'static str {
            "sleep-stub"
        }

        fn place(
            &self,
            problem: &mut SchematicPlaceProblem,
            _eval: &dyn CandidateEvaluator,
        ) -> PlacementOutput {
            thread::sleep(Duration::from_secs(3));
            PlacementOutput {
                result: sch_model::place::PlaceResult {
                    engine: self.name().to_string(),
                    truthfulness_breaks: 0,
                    warnings: 0,
                    crossings: Default::default(),
                    cost: 0.0,
                },
                ir: problem.ir.clone(),
            }
        }
    }

    #[test]
    fn policy_picks_the_engine_that_keeps_the_budget() {
        let small = PlacementBudget::new(SPINE_ABOVE_PARTS - 1);
        let large = PlacementBudget::new(SPINE_ABOVE_PARTS);
        assert_eq!(small.engine(None), PlacementEngineKind::Cluster);
        assert_eq!(large.engine(None), PlacementEngineKind::Spine);
        assert_eq!(
            large.engine(Some(PlacementEngineKind::Anneal)),
            PlacementEngineKind::Anneal,
            "an explicit engine overrides the policy"
        );
    }

    #[test]
    fn budget_scales_with_the_measured_sheet_envelopes() {
        assert_eq!(PlacementBudget::new(10).budget, Duration::from_secs(15));
        assert_eq!(PlacementBudget::new(40).budget, Duration::from_secs(40));
        assert_eq!(PlacementBudget::new(60).budget, Duration::from_secs(45));
        assert_eq!(PlacementBudget::new(90).budget, Duration::from_secs(45));

        let policy = PlacementBudget::new(60);
        assert!(policy.engine_fits(PlacementEngineKind::Spine, Duration::from_secs(35)));
        assert!(!policy.engine_fits(PlacementEngineKind::Cluster, Duration::from_secs(35)));
    }

    #[test]
    fn the_search_stops_with_room_to_realise_and_gate() {
        let d = Deadlines::of(Some(PlacementBudget::new(40)));
        let (search, hard) = (d.search.unwrap(), d.hard.unwrap());
        assert!(
            hard.remaining() - search.remaining() > Duration::from_secs(10),
            "realising and gating need real room"
        );
        assert!(Deadlines::of(None).search.is_none(), "unbounded stays so");
    }

    #[test]
    fn overrun_names_the_budget_and_the_way_out() {
        let message = PlacementBudget::within(Duration::from_secs(60), 46)
            .overrun(Duration::from_millis(60_001), "spine", "verify")
            .to_string();
        assert!(
            message.contains("spine placement exceeded its 60s budget"),
            "{message}"
        );
        assert!(message.contains("during verify"), "{message}");
        assert!(message.contains("nothing was written"), "{message}");
        assert!(message.contains("smaller named blocks"), "{message}");
        assert!(message.contains("`block` field"), "{message}");
    }

    #[test]
    fn sleeping_engine_is_abandoned_at_the_deadline_without_touching_the_document() {
        let Some(env) = KicadInstallation::detect() else {
            eprintln!("SKIP: no KiCad environment detected");
            return;
        };
        let input: PlacePartsInput = serde_json::from_value(serde_json::json!({
            "parts": [
                {"ref": "R1", "part": "Device:R", "pins": {"1": "VCC", "2": "MID"}},
                {"ref": "R2", "part": "Device:R", "pins": {"1": "MID", "2": "GND"}}
            ]
        }))
        .unwrap();
        let mut doc = blank_sheet().unwrap();
        let before = doc.to_text();
        let limit = Duration::from_secs(1);
        let started = Instant::now();

        let error = place_parts(
            &env,
            &mut doc,
            &input,
            Box::new(SleepEngine),
            Some(PlacementBudget::within(limit, input.parts.len())),
        )
        .unwrap_err();

        assert!(started.elapsed() <= limit + Duration::from_secs(1));
        assert_eq!(doc.to_text(), before);
        assert!(
            matches!(
                &error,
                Error::Budget {
                    engine: "sleep-stub",
                    phase: "place",
                    ..
                }
            ),
            "{error:?}"
        );
    }
}
