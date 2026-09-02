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
use sch_doc::{Netlist, SchDoc, connect};
use sch_model::ir::LayoutIr;
use sch_model::item::Item;
use sch_model::place::{Deadline, PlaceOptions, PlacementEngineKind};
use sch_model::result::IdiomReport;
use serde::{Deserialize, Serialize};

use crate::floorplan::place::incidence;
use crate::region::{RegionProblem, arrange as region_arrange};
use sch_model::engine::PlacementEngine;
use sch_model::geometry::item_rect;

/// Block name the parts already on the sheet are lifted into. Prefixed so it cannot
/// collide with a block an author named.
const SHEET_BLOCK: &str = "$sheet";

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
    if let Some(intent) = input.intent.clone() {
        apply_intent(&mut ir, intent.into_layout_ir());
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
                },
            )?;
            let warnings = writer.layout_warnings();
            crate::realize::graft(doc, writer)?;
            Ok(warnings)
        },
    )?;

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
        dangling: audit.dangling,
        did_you_mean: audit.did_you_mean.into_iter().collect(),
        mismatch,
        committed,
    })
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
            rearrange_inner(&env, doc, &selection, None, phase, deadlines)
        },
    )
}

fn rearrange_inner(
    env: &KicadInstallation,
    doc: &mut SchDoc,
    selection: &Selection,
    engine: Option<&dyn PlacementEngine>,
    phase: &Phase,
    deadlines: Deadlines,
) -> Result<ArrangeReport> {
    let chosen = selection.resolve(doc);
    let (before, design) = live_phase(phase, "lower", chosen.len(), 0, || {
        let before = connect::extract(doc);
        let mut design = Design::default();
        design
            .blocks
            .insert(SHEET_BLOCK.to_string(), lift_sheet(doc, &before));
        sch_check::nets::derive_attrs(&mut design);
        (before, design)
    });

    let (mut movable, held): (Vec<Item>, Vec<Item>) = seated_items(doc, &before)
        .into_iter()
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
    let ir = crate::floorplan::infer_ir(env, &design);
    let snapshot = doc.snapshot();
    let (placed, ir) = match engine {
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
    let (redrawn, inc, warnings) = live_phase(
        phase,
        "realise",
        placed.len(),
        before.nets.len(),
        || -> Result<_> {
            for part in &placed {
                seat(doc, part)?;
            }
            owned.extend(footprints(&placed));
            let erase = selection_drawing(doc, &owned, &held);
            let redrawn = doc.retain_drawing(|item| !erase.contains(&drawing_key(item)));
            let inc = incidence(&placed);
            let writer = crate::realize::realize_block(
                env,
                &design,
                &placed,
                &inc,
                &ir,
                crate::realize::Draw {
                    driven: &driven_nets(doc, &before),
                    ..Default::default()
                },
            )?;
            let warnings = writer.layout_warnings();
            crate::realize::graft_drawing(doc, writer)?;
            Ok((redrawn, inc, warnings))
        },
    )?;

    let mismatch = live_phase(phase, "verify", placed.len(), inc.len(), || Mismatch {
        disturbed: disturbed(&before, &connect::extract(doc)),
        ..Default::default()
    });
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
        sch_doc::Item::Symbol(s) => s.uuid.clone(),
        _ => String::new(),
    }
}

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
