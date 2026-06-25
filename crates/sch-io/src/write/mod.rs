//! Schematic document writer: turns placed symbols into a loadable
//! `.kicad_sch`.
//!
//! This is the first real emission stage. [`SchematicWriter`] accumulates
//! placed symbol instances and the set of `(lib_symbols)` definitions they
//! reference, then [`SchematicWriter::finish`] assembles the full document
//! deterministically (header, `lib_symbols` sorted by `lib_id`, symbol
//! instances sorted by refdes, `sheet_instances`).
//!
//! ## Why the structure is exactly this shape
//!
//! The S-expression layout below is the form proven (via `kicad-cli`) to load
//! in KiCAD. The load-bearing details:
//!
//! - **`(lib_symbols)` embedding.** Every distinct `lib_id` used contributes
//!   one `(symbol "Lib:Name" …)` block, taken verbatim from
//!   [`SymbolGeometry::raw_definition`] (already retargeted to the
//!   fully-qualified name by `kicad-bridge`). The set is keyed by `lib_id`, so
//!   placing N resistors embeds the `Device:R` body exactly once.
//! - **The `(instances …)` path root.** Each symbol instance carries an
//!   `(instances (project "" (path "/<root-uuid>" (reference …) (unit 1))))`
//!   block whose path is `"/" + the schematic's own root uuid`. KiCAD resolves
//!   a placed symbol's reference/unit through this path; if the root uuid here
//!   does not match the document's `(uuid …)`, the component is not annotated
//!   and drops out of the netlist. So the writer computes the root uuid once
//!   and threads it into every instance.
//! - **Determinism.** Every uuid comes from [`crate::ids::stable_uuid`] and
//!   positions are snapped via [`crate::grid::snap_point`], so re-emitting the
//!   same placements yields byte-identical output (spec §5.1).
//!
//! ## Module layout
//!
//! The writer is split by responsibility, all sharing this module's data
//! structures via the same `write` module tree:
//!
//! - [`build`] — accumulating the document: `add_*` placement, pin-endpoint
//!   geometry, route-scene/refinement accessors, and the rigid `translate`.
//! - [`textsolve`] — the field/label placement solver: stub retraction, wire
//!   splitting at taps, the greedy candidate solver, reframing, and the
//!   readability lint.
//! - [`emit`] — the S-expression serialization: `finish`, the `render_*`
//!   helpers, `escape_sexpr_string`, and coordinate formatting.

use std::collections::{BTreeMap, BTreeSet};

use kicad_symbol::geometry::PinGeom;

mod build;
mod emit;
mod textsolve;

// Re-export the public surface VERBATIM so external `sch_io::write::…` paths
// resolve unchanged across the split.
pub use build::{pin_end0, pin_endpoint, quantize_dir};
pub use emit::fmt_coord;

/// Stable key identifying *this* schematic sheet for root-uuid derivation.
///
/// Task 3 emits a single root sheet, so a fixed key suffices; later tasks that
/// emit multiple sheets will key the root uuid on sheet identity instead.
const ROOT_SHEET_KEY: &str = "root";

/// Horizontal text justification for a solved field position. `Center` is
/// rendered by omitting the justify token (KiCAD's default is centered).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Justify {
    Left,
    Right,
    Center,
}

/// A solved field text anchor.
#[derive(Clone, Copy)]
pub struct TextPos {
    pub at: [f64; 2],
    pub justify: Justify,
}

/// One placed symbol instance, captured at `add_symbol` time and rendered in
/// `finish`.
pub(super) struct Instance {
    pub(super) lib_id: String,
    pub(super) refdes: String,
    pub(super) value: String,
    /// Footprint lib_id for the symbol's `Footprint` property, or `None` when the
    /// part is unassigned (emits an empty property, KiCAD's placeholder). Sourced
    /// from the kernel `Component.footprint` via `Item` (the schematic-side home
    /// of footprint assignment — see docs/specs/unified-kicad-pcb-state.md).
    pub(super) footprint: Option<String>,
    /// Grid-snapped sheet position.
    pub(super) at: [f64; 2],
    /// Orientation in degrees (0/90/180/270).
    pub(super) angle: f64,
    /// Whether the instance is mirrored on the X axis (`(mirror x)`). We do not
    /// emit mirror today, but the endpoint transform handles it so connectivity
    /// stays correct once placement gains mirroring.
    pub(super) mirror: bool,
    /// Extra hidden properties to write on this symbol, in insertion order. The
    /// reconciliation identity tags (`ap_block`/`ap_role`/`ap_parent`/`ap_index`)
    /// ride here so the emitted file is self-describing for the next
    /// lift/reconcile (spec §4/§7).
    pub(super) extra_props: Vec<(String, String)>,
    /// An explicit instance uuid to reuse (e.g. a surviving symbol's prior uuid
    /// during reconciliation, so diffs stay minimal). `None` falls back to the
    /// content-derived `stable_uuid("symbol", refdes)`.
    pub(super) uuid: Option<String>,
    /// Half the symbol body's approximate size `[w/2, h/2]`, used to push the
    /// Reference/Value field text clear of the body rather than a fixed offset.
    pub(super) half_extents: [f64; 2],
    /// Solver-assigned Reference/Value positions (`solve_text_positions`).
    /// `None` -> legacy fixed right-of-body offsets (kept for hidden fields
    /// and as the fallback when the solver has not run).
    pub(super) ref_pos: Option<TextPos>,
    pub(super) val_pos: Option<TextPos>,
    /// Solver-hidden Value: set when no collision-free spot exists for an
    /// OPTIONAL text (a power symbol's rail name next to siblings of the same
    /// rail — the first sibling shows the name, the rest hide). Connectivity
    /// is unaffected: KiCAD reads a power port's net from the Value field
    /// whether or not it is displayed.
    pub(super) val_hidden: bool,
    /// 1-based symbol UNIT this instance draws. A multi-unit part (op-amp, FPGA,
    /// dual/quad pack) is placed as one instance PER unit, all sharing `refdes`
    /// but with distinct `unit` (and a unit-distinguished uuid). KiCAD then draws
    /// only that unit's graphics/pins, so every unit's pins reach the netlist.
    /// Single-unit parts (the common case) are unit 1.
    pub(super) unit: u8,
}

/// One net-name label emitted at a pin's sheet-space connection endpoint.
///
/// A label whose `(at …)` coincides with a pin endpoint binds that pin to the
/// named net; two pins carrying labels with the same net name are joined by
/// KiCAD with no wires. The label uuid is derived
/// from `(refdes, pin, net)` so re-emitting the same design is byte-identical.
pub(super) struct PinLabel {
    /// Net name (free-form; escaped at render time).
    pub(super) net: String,
    /// Grid-snapped sheet-space position of the pin's connection endpoint.
    pub(super) at: [f64; 2],
    /// Stable key for the label uuid: `"<refdes>:<pin>:<net>:<index>"`. The
    /// index disambiguates the (rare) case where a pin *name* matches multiple
    /// physical pins, each of which gets its own label.
    pub(super) uuid_key: String,
    /// Direction the stub points (away from the symbol body). Drives the label's
    /// rotation angle + justification so the text reads away from the body. The
    /// legacy `add_pin_label` path defaults to `Dir::East` (angle 0, justify
    /// left), keeping its output byte-identical to pre-stub emission.
    pub(super) dir: Dir,
    /// Present for stub-mounted signal labels: the stub wire to emit and the
    /// pin endpoint to retract onto if the stub end collides with a foreign net.
    /// `None` for legacy labels placed directly on the pin endpoint.
    pub(super) stub: Option<Stub>,
    /// Render as a KiCAD `global_label` (the off-sheet I/O pentagon) rather than
    /// a plain local label. Set for ports — board-edge / cross-sheet signals —
    /// so a single-pin port reads as intentional I/O and ERC does not flag it.
    pub(super) global: bool,
}

/// A retractable stub wire backing a signal label: the pin endpoint the stub
/// starts at. The stub end is the owning [`PinLabel`]'s `at`. If retracted, the
/// wire is dropped and the label is moved back to `pin_at`.
#[derive(Clone, Copy)]
pub(super) struct Stub {
    pub(super) pin_at: [f64; 2],
}

/// One `(wire …)` segment between two grid-snapped sheet points.
#[derive(Clone)]
pub(super) struct Wire {
    pub(super) a: [f64; 2],
    pub(super) b: [f64; 2],
    /// Stable key for the wire uuid (content-derived from the endpoints).
    pub(super) uuid_key: String,
    /// The net this wire belongs to, when known (cluster-generated wires).
    /// `None` for legacy power stubs/risers (treated as a reserved foreign net).
    pub(super) net: Option<String>,
}

/// One `(junction …)` dot marking a deliberate ≥3-way wire join.
pub(super) struct Junction {
    pub(super) at: [f64; 2],
    /// Stable key for the junction uuid (content-derived from the position).
    pub(super) uuid_key: String,
}

/// Free-standing sheet text (block titles / annotations).
pub(super) struct SheetText {
    pub(super) text: String,
    pub(super) at: [f64; 2],
    /// Font size (mm); titles 2.54, annotations 1.27.
    pub(super) size: f64,
    pub(super) bold: bool,
    pub(super) uuid_key: String,
}

/// A graphic rectangle (block frame).
pub(super) struct SheetRect {
    pub(super) start: [f64; 2],
    pub(super) end: [f64; 2],
    pub(super) uuid_key: String,
}

// `Dir`, `point_on_segment`, and `transform_offset` live in `sch_place::geom`
// (shared with the `label`/`wire` modules); re-exported so `crate::write::Dir`
// etc. and the public API keep working.
pub use sch_place::geom::{point_on_segment, transform_offset, Dir};

/// One `(no_connect …)` marker emitted at a pin's sheet-space endpoint.
///
/// KiCAD's ERC flags an *unconnected* pin (no wire, no label, no marker) on most
/// pin types. A `(no_connect)` marker placed exactly on the pin endpoint tells
/// ERC the disconnection is intentional, silencing that pin's complaint. The
/// kernel auto-NCs every symbol pin the author did not mention (they arrive as
/// `PinTarget::NoConnect` in the `Design`); each becomes one of these markers.
pub(super) struct NoConnect {
    /// Grid-snapped sheet-space position of the pin's connection endpoint.
    pub(super) at: [f64; 2],
    /// Stable key for the marker uuid: `"<refdes>:<pin>:<index>"`.
    pub(super) uuid_key: String,
}

/// Accumulates placed symbols and emits a deterministic `.kicad_sch` document.
#[derive(Default)]
pub struct SchematicWriter {
    /// `(lib_symbols)` bodies, keyed by `lib_id` for dedup; `BTreeMap` keeps the
    /// emitted set sorted by `lib_id` with no extra sort step.
    pub(super) lib_symbols: BTreeMap<String, String>,
    /// Placed instances, in insertion order; sorted by refdes at `finish`.
    pub(super) instances: Vec<Instance>,
    /// Net-name labels at pin endpoints, in insertion order; sorted by uuid_key
    /// at `finish` for deterministic output.
    pub(super) labels: Vec<PinLabel>,
    /// `(no_connect)` markers at intentionally-unconnected pin endpoints.
    pub(super) no_connects: Vec<NoConnect>,
    /// Wire segments added via `add_wire`, sorted by `uuid_key` at `finish`.
    pub(super) wires: Vec<Wire>,
    /// Junction dots added via `add_junction`, sorted by `uuid_key` at `finish`.
    pub(super) junctions: Vec<Junction>,
    /// Free-standing sheet texts (block titles / notes), sorted by `uuid_key`.
    pub(super) texts: Vec<SheetText>,
    /// Graphic rectangles (block frames), sorted by `uuid_key`.
    pub(super) rects: Vec<SheetRect>,
    /// Approximate symbol body size keyed by `lib_id`, populated when a new
    /// lib_id's geometry is loaded (the dedup branch). Avoids reloading geometry
    /// per instance just to compute its field-clearance half-extents.
    pub(super) sym_sizes: BTreeMap<String, [f64; 2]>,
    /// Pin geometry per lib_id, cached at first load, for pin-text obstacles
    /// in `solve_text_positions`.
    pub(super) sym_pins: BTreeMap<String, Vec<PinGeom>>,
    /// Sheet title (the design name), rendered into the title block.
    pub(super) title: Option<String>,
    /// When set, [`Self::prepare`] reframes the drawing so its min corner sits at
    /// the page margin (the floorplan path, whose edge port labels / rail symbols
    /// extend past the symbol bodies). Off for direct-writer and legacy paths,
    /// which place content at fixed absolute coordinates.
    pub(super) frame: bool,
    /// Refdes whose Reference/Value fields should be solved ABOVE the body in
    /// preference to below. Set for a repeated-column anchor (a low-side
    /// half-bridge FET) whose down-facing source pin hangs a rotated global PORT
    /// label (`SHUNT_x`): the conventional below-body field band would crowd that
    /// label's vertical strip and the two read as one garbled token ("V_LS" right
    /// under "SHUNT_V_TOP"). Placing the fields above mirrors the high-side row
    /// (text below) and leaves the port label its own clear space. Empty on every
    /// path except the `MULTISHEET_REFINE` low-side case, so the single-sheet
    /// reference snapshots stay byte-identical.
    pub(super) fields_above: BTreeSet<String>,
}

impl SchematicWriter {
    /// A new, empty writer.
    pub fn new() -> Self {
        Self::default()
    }
}

/// An axis-aligned bbox: [min_x, min_y, max_x, max_y].
pub(super) type BBox = [f64; 4];

/// Resolved Reference/Value anchors for an instance: the solver's assignment
/// when present, else the legacy fixed right-of-body offsets. The single
/// source of truth shared by `render_instance` and the overlap lint, so the
/// lint always boxes exactly what gets emitted.
pub(super) fn field_anchors(inst: &Instance) -> (TextPos, TextPos) {
    let legacy = |dy: f64| TextPos {
        at: [inst.at[0] + inst.half_extents[0] + 1.27, inst.at[1] + dy],
        justify: Justify::Left,
    };
    (
        inst.ref_pos.unwrap_or_else(|| legacy(-1.27)),
        inst.val_pos.unwrap_or_else(|| legacy(1.27)),
    )
}

/// Bbox of a rendered field text line: bottom-anchored, 1.6 mm tall, width
/// per [`crate::label::text_width`], extending per its justification.
pub(super) fn field_box(at: [f64; 2], j: Justify, width: f64) -> BBox {
    match j {
        Justify::Left => [at[0], at[1] - 1.6, at[0] + width, at[1]],
        Justify::Right => [at[0] - width, at[1] - 1.6, at[0], at[1]],
        Justify::Center => [at[0] - width / 2.0, at[1] - 1.6, at[0] + width / 2.0, at[1]],
    }
}

/// Justify token for a solved field anchor. `Center` omits the token (KiCAD's
/// default field justification is centered).
pub(super) fn justify_token(j: Justify) -> &'static str {
    match j {
        Justify::Left => " (justify left)",
        Justify::Right => " (justify right)",
        Justify::Center => "",
    }
}

/// Whether two axis-aligned boxes overlap (open intervals, so edge-touching is
/// not a collision — symbols flush against a frame don't trip the lint).
pub(super) fn boxes_overlap(a: &BBox, b: &BBox) -> bool {
    // Tolerance matches `floorplan::rects_overlap`: a shared edge (and the
    // sub-micron float jitter around one) is a TOUCH between padded bboxes — real
    // clearance, not a collision — so it must NOT be flagged. Without this, two
    // collinear/adjacent parts whose padded boxes meet (a divider's R7/R8 spine,
    // a pull-up just above a wide IC) trip a phantom overlap warning.
    const EPS: f64 = 1e-6;
    a[0] < b[2] - EPS && b[0] < a[2] - EPS && a[1] < b[3] - EPS && b[1] < a[3] - EPS
}
