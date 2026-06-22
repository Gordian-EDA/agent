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

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io;

use kicad_bridge::env::KicadEnv;
use kicad_bridge::geometry::{PinGeom, SymbolGeometry};

use crate::grid::snap_point;
use crate::ids::stable_uuid;

/// Stable key identifying *this* schematic sheet for root-uuid derivation.
///
/// Task 3 emits a single root sheet, so a fixed key suffices; later tasks that
/// emit multiple sheets will key the root uuid on sheet identity instead.
const ROOT_SHEET_KEY: &str = "root";

/// Horizontal text justification for a solved field position. `Center` is
/// rendered by omitting the justify token (KiCAD's default is centered).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Justify {
    Left,
    Right,
    Center,
}

/// A solved field text anchor.
#[derive(Clone, Copy)]
pub(crate) struct TextPos {
    pub at: [f64; 2],
    pub justify: Justify,
}

/// One placed symbol instance, captured at `add_symbol` time and rendered in
/// `finish`.
struct Instance {
    lib_id: String,
    refdes: String,
    value: String,
    /// Grid-snapped sheet position.
    at: [f64; 2],
    /// Orientation in degrees (0/90/180/270).
    angle: f64,
    /// Whether the instance is mirrored on the X axis (`(mirror x)`). We do not
    /// emit mirror today, but the endpoint transform handles it so connectivity
    /// stays correct once placement gains mirroring.
    mirror: bool,
    /// Extra hidden properties to write on this symbol, in insertion order. The
    /// reconciliation identity tags (`ap_block`/`ap_role`/`ap_parent`/`ap_index`)
    /// ride here so the emitted file is self-describing for the next
    /// lift/reconcile (spec §4/§7).
    extra_props: Vec<(String, String)>,
    /// An explicit instance uuid to reuse (e.g. a surviving symbol's prior uuid
    /// during reconciliation, so diffs stay minimal). `None` falls back to the
    /// content-derived `stable_uuid("symbol", refdes)`.
    uuid: Option<String>,
    /// Half the symbol body's approximate size `[w/2, h/2]`, used to push the
    /// Reference/Value field text clear of the body rather than a fixed offset.
    half_extents: [f64; 2],
    /// Solver-assigned Reference/Value positions (`solve_text_positions`).
    /// `None` -> legacy fixed right-of-body offsets (kept for hidden fields
    /// and as the fallback when the solver has not run).
    ref_pos: Option<TextPos>,
    val_pos: Option<TextPos>,
    /// Solver-hidden Value: set when no collision-free spot exists for an
    /// OPTIONAL text (a power symbol's rail name next to siblings of the same
    /// rail — the first sibling shows the name, the rest hide). Connectivity
    /// is unaffected: KiCAD reads a power port's net from the Value field
    /// whether or not it is displayed.
    val_hidden: bool,
    /// 1-based symbol UNIT this instance draws. A multi-unit part (op-amp, FPGA,
    /// dual/quad pack) is placed as one instance PER unit, all sharing `refdes`
    /// but with distinct `unit` (and a unit-distinguished uuid). KiCAD then draws
    /// only that unit's graphics/pins, so every unit's pins reach the netlist.
    /// Single-unit parts (the common case) are unit 1.
    unit: u8,
}

/// One net-name label emitted at a pin's sheet-space connection endpoint.
///
/// A label whose `(at …)` coincides with a pin endpoint binds that pin to the
/// named net; two pins carrying labels with the same net name are joined by
/// KiCAD with no wires. The label uuid is derived
/// from `(refdes, pin, net)` so re-emitting the same design is byte-identical.
struct PinLabel {
    /// Net name (free-form; escaped at render time).
    net: String,
    /// Grid-snapped sheet-space position of the pin's connection endpoint.
    at: [f64; 2],
    /// Stable key for the label uuid: `"<refdes>:<pin>:<net>:<index>"`. The
    /// index disambiguates the (rare) case where a pin *name* matches multiple
    /// physical pins, each of which gets its own label.
    uuid_key: String,
    /// Direction the stub points (away from the symbol body). Drives the label's
    /// rotation angle + justification so the text reads away from the body. The
    /// legacy `add_pin_label` path defaults to `Dir::East` (angle 0, justify
    /// left), keeping its output byte-identical to pre-stub emission.
    dir: Dir,
    /// Present for stub-mounted signal labels: the stub wire to emit and the
    /// pin endpoint to retract onto if the stub end collides with a foreign net.
    /// `None` for legacy labels placed directly on the pin endpoint.
    stub: Option<Stub>,
    /// Render as a KiCAD `global_label` (the off-sheet I/O pentagon) rather than
    /// a plain local label. Set for ports — board-edge / cross-sheet signals —
    /// so a single-pin port reads as intentional I/O and ERC does not flag it.
    global: bool,
}

/// A retractable stub wire backing a signal label: the pin endpoint the stub
/// starts at. The stub end is the owning [`PinLabel`]'s `at`. If retracted, the
/// wire is dropped and the label is moved back to `pin_at`.
#[derive(Clone, Copy)]
struct Stub {
    pin_at: [f64; 2],
}

/// One `(wire …)` segment between two grid-snapped sheet points.
#[derive(Clone)]
struct Wire {
    a: [f64; 2],
    b: [f64; 2],
    /// Stable key for the wire uuid (content-derived from the endpoints).
    uuid_key: String,
    /// The net this wire belongs to, when known (cluster-generated wires).
    /// `None` for legacy power stubs/risers (treated as a reserved foreign net).
    net: Option<String>,
}

/// One `(junction …)` dot marking a deliberate ≥3-way wire join.
struct Junction {
    at: [f64; 2],
    /// Stable key for the junction uuid (content-derived from the position).
    uuid_key: String,
}

/// Free-standing sheet text (block titles / annotations).
struct SheetText {
    text: String,
    at: [f64; 2],
    /// Font size (mm); titles 2.54, annotations 1.27.
    size: f64,
    bold: bool,
    uuid_key: String,
}

/// A graphic rectangle (block frame).
struct SheetRect {
    start: [f64; 2],
    end: [f64; 2],
    uuid_key: String,
}

/// A pin's outward direction on the sheet, quantized to the four axes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    East,
    West,
    North,
    South,
}

impl Dir {
    /// Sheet-space unit vector (sheet Y grows downward, so North is -y).
    pub fn vec(self) -> [f64; 2] {
        match self {
            Dir::East => [1.0, 0.0],
            Dir::West => [-1.0, 0.0],
            Dir::North => [0.0, -1.0],
            Dir::South => [0.0, 1.0],
        }
    }
}

/// One `(no_connect …)` marker emitted at a pin's sheet-space endpoint.
///
/// KiCAD's ERC flags an *unconnected* pin (no wire, no label, no marker) on most
/// pin types. A `(no_connect)` marker placed exactly on the pin endpoint tells
/// ERC the disconnection is intentional, silencing that pin's complaint. The
/// kernel auto-NCs every symbol pin the author did not mention (they arrive as
/// `PinTarget::NoConnect` in the `Design`); each becomes one of these markers.
struct NoConnect {
    /// Grid-snapped sheet-space position of the pin's connection endpoint.
    at: [f64; 2],
    /// Stable key for the marker uuid: `"<refdes>:<pin>:<index>"`.
    uuid_key: String,
}

/// Accumulates placed symbols and emits a deterministic `.kicad_sch` document.
#[derive(Default)]
pub struct SchematicWriter {
    /// `(lib_symbols)` bodies, keyed by `lib_id` for dedup; `BTreeMap` keeps the
    /// emitted set sorted by `lib_id` with no extra sort step.
    lib_symbols: BTreeMap<String, String>,
    /// Placed instances, in insertion order; sorted by refdes at `finish`.
    instances: Vec<Instance>,
    /// Net-name labels at pin endpoints, in insertion order; sorted by uuid_key
    /// at `finish` for deterministic output.
    labels: Vec<PinLabel>,
    /// `(no_connect)` markers at intentionally-unconnected pin endpoints.
    no_connects: Vec<NoConnect>,
    /// Wire segments added via `add_wire`, sorted by `uuid_key` at `finish`.
    wires: Vec<Wire>,
    /// Junction dots added via `add_junction`, sorted by `uuid_key` at `finish`.
    junctions: Vec<Junction>,
    /// Free-standing sheet texts (block titles / notes), sorted by `uuid_key`.
    texts: Vec<SheetText>,
    /// Graphic rectangles (block frames), sorted by `uuid_key`.
    rects: Vec<SheetRect>,
    /// Approximate symbol body size keyed by `lib_id`, populated when a new
    /// lib_id's geometry is loaded (the dedup branch). Avoids reloading geometry
    /// per instance just to compute its field-clearance half-extents.
    sym_sizes: BTreeMap<String, [f64; 2]>,
    /// Pin geometry per lib_id, cached at first load, for pin-text obstacles
    /// in `solve_text_positions`.
    sym_pins: BTreeMap<String, Vec<PinGeom>>,
    /// Sheet title (the design name), rendered into the title block.
    title: Option<String>,
    /// When set, [`Self::prepare`] reframes the drawing so its min corner sits at
    /// the page margin (the floorplan path, whose edge port labels / rail symbols
    /// extend past the symbol bodies). Off for direct-writer and legacy paths,
    /// which place content at fixed absolute coordinates.
    frame: bool,
}

impl SchematicWriter {
    /// A new, empty writer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Place one symbol instance.
    ///
    /// Loads the symbol's geometry/definition for `lib_id` from `env`, registers
    /// its `(lib_symbols)` body (deduplicated by `lib_id`), and records a placed
    /// instance with the given `refdes`, `value`, position `at` (snapped to the
    /// grid), and `angle` (degrees). Nothing is written until [`Self::finish`].
    ///
    /// Returns the error from [`SymbolGeometry::load`] if the symbol cannot be
    /// resolved.
    pub fn add_symbol(
        &mut self,
        env: &KicadEnv,
        lib_id: &str,
        refdes: &str,
        value: &str,
        at: [f64; 2],
        angle: f64,
    ) -> io::Result<()> {
        self.add_symbol_full(env, lib_id, refdes, value, at, angle, &[], None)
    }

    /// Place one symbol instance with reconciliation metadata.
    ///
    /// The fuller form of [`Self::add_symbol`]: in addition to the placement, it
    /// attaches `extra_props` (the hidden `ap_*` identity tags that make the
    /// emitted file self-describing for the next lift/reconcile, spec §4/§7) and
    /// an optional explicit instance `uuid` to reuse a surviving symbol's prior
    /// id (so re-emitting after a user edit produces a minimal diff). `uuid =
    /// None` falls back to the content-derived `stable_uuid("symbol", refdes)`.
    ///
    /// `at` is grid-snapped exactly as in [`Self::add_symbol`]; passing a
    /// position read back from a prior `.kicad_sch` therefore preserves it (the
    /// editor already keeps placements on-grid). Returns the error from
    /// [`SymbolGeometry::load`] if the symbol cannot be resolved.
    #[allow(clippy::too_many_arguments)]
    pub fn add_symbol_full(
        &mut self,
        env: &KicadEnv,
        lib_id: &str,
        refdes: &str,
        value: &str,
        at: [f64; 2],
        angle: f64,
        extra_props: &[(String, String)],
        uuid: Option<String>,
    ) -> io::Result<()> {
        // Register the lib_symbol body once per lib_id (dedup). The same branch
        // caches the symbol's approximate size by lib_id so field placement need
        // not reload geometry per instance.
        if !self.lib_symbols.contains_key(lib_id) {
            let geom = SymbolGeometry::load(env, lib_id)?;
            self.sym_sizes.insert(lib_id.to_string(), geom.approx_size());
            self.sym_pins.insert(lib_id.to_string(), geom.pins.clone());
            self.lib_symbols
                .insert(lib_id.to_string(), geom.raw_definition);
        }

        // Cached above on the first instance of this lib_id; reused for the rest.
        let size = self.sym_sizes.get(lib_id).copied().unwrap_or([0.0, 0.0]);
        let half_extents = [size[0] / 2.0, size[1] / 2.0];

        self.instances.push(Instance {
            lib_id: lib_id.to_string(),
            refdes: refdes.to_string(),
            value: value.to_string(),
            at: snap_point(at),
            angle,
            mirror: false,
            extra_props: extra_props.to_vec(),
            uuid,
            half_extents,
            ref_pos: None,
            val_pos: None,
            val_hidden: false,
            unit: 1,
        });
        Ok(())
    }

    /// Set the symbol UNIT of the most-recently-added instance (the dual of
    /// [`Self::set_mirror_last`]). The floorplan engine calls this when it places
    /// the units of a multi-unit part as separate instances sharing a refdes.
    pub fn set_unit_last(&mut self, unit: u8) {
        if let Some(i) = self.instances.last_mut() {
            i.unit = unit;
        }
    }

    /// Mirror the most recently added symbol left-to-right (`(mirror y)`). Used
    /// by the floorplan engine to flip an IC so the pins facing its neighbours
    /// (e.g. a translator's B-side toward the connector) point the right way.
    pub fn set_mirror_last(&mut self) {
        if let Some(i) = self.instances.last_mut() {
            i.mirror = true;
        }
    }

    /// Place a net-name label at the connection endpoint of one pin.
    ///
    /// This is the connectivity mechanism: a label whose position coincides with
    /// a pin's sheet-space connection point binds that pin to the named net, and
    /// two pins carrying labels with the *same* net name are joined by KiCAD with
    /// no wires. Power
    /// nets get plain labels too — they suffice for ERC connectivity; power
    /// symbols are an optional later enhancement.
    ///
    /// `pin` is resolved against the symbol geometry **by number first, then by
    /// name** (matching `circuit-lang`'s pin resolution). A pin *name* may match
    /// several physical pins; in that case a label is emitted at **every**
    /// matching pin so they all join the net.
    ///
    /// The endpoint is computed from the placed instance's recorded position and
    /// orientation (and mirror, when present): the pin's local connection point
    /// is rotated/flipped into sheet space and snapped to the grid. See
    /// [`pin_endpoint`] for the exact transform.
    ///
    /// Returns an error if `refdes` was never placed, if its geometry cannot be
    /// loaded, or if no pin matches `pin` by number or name.
    pub fn add_pin_label(
        &mut self,
        env: &KicadEnv,
        refdes: &str,
        pin: &str,
        net: &str,
    ) -> io::Result<()> {
        let endpoints = self.pin_endpoints(env, refdes, pin)?;
        for (idx, at) in endpoints.into_iter().enumerate() {
            self.labels.push(PinLabel {
                net: net.to_string(),
                at,
                uuid_key: format!("{refdes}:{pin}:{net}:{idx}"),
                // Legacy/no-stub path: East -> angle 0, justify left bottom,
                // byte-identical to pre-stub label output.
                dir: Dir::East,
                stub: None,
                global: false,
            });
        }
        Ok(())
    }

    /// Signal-net connectivity with breathing room: a stub wire out of the pin
    /// and the net label at the stub's far end, oriented along the stub so the
    /// text reads away from the symbol body.
    ///
    /// Displacing the label off the pin endpoint risks landing it on a *foreign*
    /// connection point (most often a horizontal power pin's power symbol, which
    /// `emit_power_pin` parks one row over via a riser): a label there would
    /// silently merge two nets. No fixed stub length is collision-free in a dense
    /// auto-placed sheet. So the label/stub is recorded as *retractable* and a
    /// finalize pass ([`SchematicWriter::retract_colliding_stubs`]) drops the stub
    /// (snapping the label back onto its always-safe pin endpoint) for any signal
    /// label whose stub end coincides with another net's anchor. The pin endpoint
    /// is the same place the pre-stub emitter put the label, so the fallback is
    /// proven connectivity-safe.
    pub fn add_signal_label(
        &mut self,
        env: &KicadEnv,
        refdes: &str,
        pin: &str,
        net: &str,
    ) -> io::Result<()> {
        const STUB_MM: f64 = 3.81;
        for (idx, (ep, dir)) in self.pin_dirs(env, refdes, pin)?.into_iter().enumerate() {
            let ep = snap_point(ep);
            let v = dir.vec();
            let end = snap_point([ep[0] + v[0] * STUB_MM, ep[1] + v[1] * STUB_MM]);
            self.labels.push(PinLabel {
                net: net.to_string(),
                at: end,
                uuid_key: format!("{refdes}:{pin}:{net}:{idx}"),
                dir,
                stub: Some(Stub { pin_at: ep }),
                global: false,
            });
        }
        Ok(())
    }

    /// Place a power symbol (graphic power port) whose **Value names the net**.
    ///
    /// KiCAD derives a power port's global net from the symbol's Value field,
    /// so a stock `power:GND` drives `GND` and any donor symbol with an
    /// overridden Value drives that custom rail. The single pin of every
    /// `power:` symbol sits at the symbol origin, so `at` IS the connection
    /// point. `refdes` must be `#`-prefixed (hidden, netlist-excluded).
    pub fn add_power_symbol(
        &mut self,
        env: &KicadEnv,
        lib_id: &str,
        refdes: &str,
        net: &str,
        at: [f64; 2],
        angle: f64,
    ) -> io::Result<()> {
        debug_assert!(refdes.starts_with('#'), "power symbol refdes must be #-prefixed, got {refdes:?}");
        self.add_symbol(env, lib_id, refdes, net, at, angle)
    }

    /// Place a `PWR_FLAG` whose pin is **pin-coincident** with `at`.
    ///
    /// Power nets are joined by global power ports, and a *local* label does
    /// not merge with a global net — so the flag attaches by position, not by
    /// label: its pin (at the symbol origin) lands exactly on an existing
    /// power-port connection point.
    pub fn add_power_flag_at(
        &mut self,
        env: &KicadEnv,
        refdes: &str,
        at: [f64; 2],
        angle: f64,
    ) -> io::Result<()> {
        self.add_symbol(env, "power:PWR_FLAG", refdes, "PWR_FLAG", at, angle)
    }

    /// Add a wire segment between two sheet points (snapped).
    ///
    /// If the two points are identical after snapping, the segment is silently
    /// dropped (a zero-length wire would clutter the schematic with no benefit).
    /// The `uuid_key` is content-derived so repeated calls with the same
    /// endpoints produce one deterministic wire.
    pub fn add_wire(&mut self, a: [f64; 2], b: [f64; 2]) {
        self.push_wire(a, b, None);
    }

    /// Add a wire that belongs to a known net (cluster geometry). Same-net
    /// touches against it are deliberate joins, not collisions.
    pub fn add_wire_on_net(&mut self, a: [f64; 2], b: [f64; 2], net: &str) {
        self.push_wire(a, b, Some(net.to_string()));
    }

    fn push_wire(&mut self, a: [f64; 2], b: [f64; 2], net: Option<String>) {
        let a = snap_point(a);
        let b = snap_point(b);
        if a == b {
            return;
        }
        let uuid_key = format!("{}:{}:{}:{}", a[0], a[1], b[0], b[1]);
        if self.wires.iter().any(|w| w.uuid_key == uuid_key) {
            return;
        }
        self.wires.push(Wire { a, b, uuid_key, net });
    }

    /// Place a cluster net label at `at`, oriented `dir`.
    ///
    /// Thin entry point for cluster decoration: a cluster emits exactly one
    /// label per externally-visible net at the net's tap point, so connectivity
    /// joins to the rest of the sheet without per-pin label spam. The label is
    /// keyed on `cluster:{net}:{x}:{y}` (position-derived) and carries no stub —
    /// it sits directly on the cluster wire it labels.
    pub fn add_cluster_label(&mut self, net: &str, at: [f64; 2], dir: Dir, global: bool) {
        let at = snap_point(at);
        self.labels.push(PinLabel {
            net: net.to_string(),
            at,
            uuid_key: format!("cluster:{net}:{}:{}", at[0], at[1]),
            dir,
            stub: None,
            global,
        });
    }

    /// Add a junction dot at a wire join. Deduplicated by position.
    pub fn add_junction(&mut self, at: [f64; 2]) {
        let at = snap_point(at);
        let uuid_key = format!("{}:{}", at[0], at[1]);
        if self.junctions.iter().any(|j| j.uuid_key == uuid_key) {
            return;
        }
        self.junctions.push(Junction { at, uuid_key });
    }

    /// Set the sheet title (rendered in the drawing frame's title block).
    pub fn set_title(&mut self, title: &str) {
        self.title = Some(title.to_string());
    }

    /// Add free-standing text to the sheet.
    pub fn add_text(&mut self, text: &str, at: [f64; 2], size: f64, bold: bool, key: &str) {
        self.texts.push(SheetText {
            text: text.to_string(),
            at: snap_point(at),
            size,
            bold,
            uuid_key: key.to_string(),
        });
    }

    /// Add a graphic rectangle (no fill, dashed) to the sheet.
    pub fn add_rect(&mut self, start: [f64; 2], end: [f64; 2], key: &str) {
        self.rects.push(SheetRect {
            start: snap_point(start),
            end: snap_point(end),
            uuid_key: key.to_string(),
        });
    }

    /// Resolve a pin to its endpoint(s) AND outward direction(s) on the sheet.
    ///
    /// A pin's local `angle` points from the connection point INTO the body, so
    /// outward is `angle + 180°`, transformed exactly like the endpoint itself
    /// (mirror -> instance rotation -> sheet Y-flip) and quantized to an axis.
    pub fn pin_dirs(
        &self,
        env: &KicadEnv,
        refdes: &str,
        pin: &str,
    ) -> io::Result<Vec<([f64; 2], Dir)>> {
        // A refdes may have SEVERAL instances — one per unit of a multi-unit part,
        // each at its own position. Pick the first as the lib_id/geometry source
        // (units share a lib_id), then resolve each matched pin against the
        // instance that draws ITS unit, so a unit-B pin lands at unit B's body.
        let any = self
            .instances
            .iter()
            .find(|i| i.refdes == refdes)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no placed symbol with refdes {refdes:?}"),
                )
            })?;
        let lib_id = any.lib_id.clone();
        // Pins are cached per lib_id when the symbol is first added, so this hot
        // path (called once per net-pin during routing, and many times over while
        // the refinement loop re-routes candidate placements) never re-reads the
        // `.kicad_sym` from disk. Fall back to a load only if somehow uncached.
        let pins: Vec<PinGeom> = match self.sym_pins.get(&lib_id).cloned() {
            Some(p) => p,
            None => SymbolGeometry::load(env, &lib_id)?.pins,
        };

        let matches: Vec<&PinGeom> = {
            let by_number: Vec<&PinGeom> = pins.iter().filter(|p| p.number == pin).collect();
            if !by_number.is_empty() {
                by_number
            } else {
                pins.iter().filter(|p| p.name == pin).collect()
            }
        };
        if matches.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no pin {pin:?} on {lib_id}"),
            ));
        }
        // The instance that draws unit `u` (its pins live at its placement); fall
        // back to `any` when a unit has no dedicated instance (single-unit parts,
        // or an unplaced unit).
        let inst_for = |u: u8| -> &Instance {
            self.instances
                .iter()
                .find(|i| i.refdes == refdes && i.unit == u)
                .unwrap_or(any)
        };
        Ok(matches
            .into_iter()
            .map(|pg| {
                let inst = inst_for(pg.unit.max(1));
                let ep = pin_endpoint(pg, inst.at, inst.angle, inst.mirror);
                let dir = quantize_dir(pg.angle, inst.angle, inst.mirror);
                (ep, dir)
            })
            .collect())
    }

    /// Place a `(no_connect)` marker at the endpoint(s) of one pin.
    ///
    /// This is the dual of [`Self::add_pin_label`] for *intentionally*
    /// unconnected pins: where a label binds a pin to a net, a no-connect marker
    /// declares the disconnection deliberate so KiCAD's ERC does not report the
    /// pin as floating. The kernel auto-NCs every symbol pin the author left
    /// unmentioned (they arrive as `PinTarget::NoConnect`); emitting a marker for
    /// each keeps ERC clean on those.
    ///
    /// Pin resolution is identical to [`Self::add_pin_label`] (number first, then
    /// name; a name may match several physical pins, each getting its own
    /// marker), so a labelled pin and a no-connected pin land on the very same
    /// endpoint. The marker uuid is content-derived for byte-identical re-emit.
    ///
    /// Returns an error if `refdes` was never placed, if its geometry cannot be
    /// loaded, or if no pin matches `pin` by number or name.
    pub fn add_no_connect(&mut self, env: &KicadEnv, refdes: &str, pin: &str) -> io::Result<()> {
        let endpoints = self.pin_endpoints(env, refdes, pin)?;
        for (idx, at) in endpoints.into_iter().enumerate() {
            self.no_connects.push(NoConnect {
                at,
                uuid_key: format!("{refdes}:{pin}:{idx}"),
            });
        }
        Ok(())
    }

    /// Register a `PWR_FLAG` power source on `net`.
    ///
    /// KiCAD ERC treats a net carrying only power-*input* pins (e.g. an MCU's
    /// `VDD`/`VSS`) and net-name labels as undriven — there is no power *source*
    /// on it — and reports `power_pin_not_driven` at error severity. A
    /// `power:PWR_FLAG` symbol is the canonical fix: it is a graphic-only symbol
    /// whose single pin is typed `power_out`, so dropping one on each power net
    /// (with a label binding it to that net) supplies the missing source. The
    /// flag's reference is the hidden `#FLG…` form KiCAD uses for power symbols;
    /// such `#`-prefixed references are excluded from the netlist's component
    /// list, so a flag never inflates the BOM/component count.
    ///
    /// `at` is the flag's instance position; its single pin sits at the symbol
    /// origin, so the net label is attached there. Returns an error if the
    /// `power:PWR_FLAG` symbol cannot be resolved from `env`.
    pub fn add_power_flag(
        &mut self,
        env: &KicadEnv,
        net: &str,
        refdes: &str,
        at: [f64; 2],
    ) -> io::Result<()> {
        self.add_symbol(env, "power:PWR_FLAG", refdes, "PWR_FLAG", at, 0.0)?;
        // Label the flag's single pin with the net so it drives that net.
        self.add_pin_label(env, refdes, "1", net)?;
        Ok(())
    }

    /// Resolve a pin reference to its sheet-space connection endpoint(s).
    ///
    /// Looks up the placed instance for `refdes`, loads its symbol geometry, and
    /// matches `pin` by number first then name (a name may match several physical
    /// pins). Each match is transformed through the instance's
    /// position/orientation/mirror into a grid-snapped sheet point. Shared by
    /// label, no-connect, and power-flag emission so they always agree on where a
    /// pin's connection point lands.
    fn pin_endpoints(&self, env: &KicadEnv, refdes: &str, pin: &str) -> io::Result<Vec<[f64; 2]>> {
        let inst = self
            .instances
            .iter()
            .find(|i| i.refdes == refdes)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no placed symbol with refdes {refdes:?}"),
                )
            })?;
        let inst_at = inst.at;
        let inst_angle = inst.angle;
        let inst_mirror = inst.mirror;

        let geom = SymbolGeometry::load(env, &inst.lib_id)?;

        // Resolve the pin: number first, then name. A name may match several
        // physical pins (e.g. multiple GND pins), so collect all matches.
        let matches: Vec<&PinGeom> = {
            let by_number: Vec<&PinGeom> = geom.pins.iter().filter(|p| p.number == pin).collect();
            if !by_number.is_empty() {
                by_number
            } else {
                geom.pins.iter().filter(|p| p.name == pin).collect()
            }
        };
        if matches.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no pin {pin:?} (by number or name) on {}", inst.lib_id),
            ));
        }

        Ok(matches
            .into_iter()
            .map(|pg| pin_endpoint(pg, inst_at, inst_angle, inst_mirror))
            .collect())
    }

    /// Retract any signal stub whose wire or far-end label would touch a *foreign*
    /// net's geometry, then emit the surviving stub wires.
    ///
    /// Displacing a signal label off its pin by a stub can make it (or its wire)
    /// touch another net's geometry, silently merging the two nets — KiCAD reads a
    /// shared point or a wire-end-on-wire T-junction as a deliberate connection,
    /// so there is *no ERC error* to catch it. The collisions come in several
    /// flavours (label on a power symbol parked one row over by `emit_power_pin`'s
    /// riser; a stub end landing on a neighbour's stub wire; a stub crossing a
    /// foreign pin) and no fixed stub length avoids them all in a dense
    /// auto-placed sheet. Rather than chase each flavour, we resolve it with one
    /// occupancy model.
    ///
    /// ## Idempotence
    ///
    /// The pass only processes labels with `stub.is_some()` and clears `stub` to
    /// `None` on any that retract; a *surviving* stub keeps its `Some(..)`, its
    /// emitted wire is `add_wire`-deduped, and that wire is registered **on the
    /// stub's own net**, so a re-run reads it as a deliberate same-net join (not
    /// a foreign segment) and the survivor survives again. A second call is
    /// therefore a no-op. This lets a caller run it early (e.g. to lint the
    /// post-retraction geometry) and have `finish` run it again harmlessly.
    ///
    /// **Foreign geometry** at pass start = every *fixed* connection point (power
    /// symbol pins — origin, net = the Value; no-connect markers — a reserved
    /// sentinel net; legacy labels; and every signal stub's own pin endpoint,
    /// always safe) plus every existing wire **segment**. Existing wires are
    /// registered under their own net when known (cluster wires added via
    /// `add_wire_on_net`) or the reserved `PWR` sentinel (power stubs/risers).
    /// A stub touching a wire of the *same* net is a deliberate join and
    /// survives; only a touch with a *different* net is foreign.
    ///
    /// Signal stubs are then walked in deterministic `uuid_key` order. A stub is
    /// **retracted** — its label snapped back onto its always-safe pin endpoint
    /// (keeping its outward orientation, so the text reads away from the
    /// body), no wire emitted — when its end coincides
    /// with a foreign point, its end lies on a foreign segment, or its segment
    /// passes through a foreign point. A *surviving* stub registers its endpoint
    /// and segment as occupancy so a later differing-net stub cannot then collide
    /// with it. The pin-endpoint fallback reproduces the proven pre-stub
    /// connectivity, so retraction only ever removes an accidental merge.
    pub fn retract_colliding_stubs(&mut self) {
        // Sentinel "net" for no-connect anchors: a stub on a no-connect pin is
        // still a wrong attachment, so treat it as a foreign net.
        const NC: &str = "\0no_connect";
        // Sentinel net for the pre-existing power wires (all power-net, never a
        // signal net — any signal touch is therefore foreign).
        const PWR: &str = "\0power_wire";

        let bits = |p: [f64; 2]| {
            let p = snap_point(p);
            (p[0].to_bits(), p[1].to_bits())
        };

        // Foreign points: net name(s) at each occupied point.
        let mut points: BTreeMap<(u64, u64), std::collections::BTreeSet<String>> = BTreeMap::new();
        let add_point = |p: [f64; 2], net: &str, m: &mut BTreeMap<(u64, u64), std::collections::BTreeSet<String>>| {
            m.entry(bits(p)).or_default().insert(net.to_string());
        };
        // Foreign axis-aligned segments: (a, b, net).
        let mut segments: Vec<([f64; 2], [f64; 2], String)> = Vec::new();

        for inst in &self.instances {
            // Power-symbol/flag pin origins (identified by `power:` lib_id) occupy
            // the points that signal stubs must not be retracted onto.
            if inst.lib_id.starts_with("power:") {
                add_point(inst.at, &inst.value, &mut points);
            }
        }
        for nc in &self.no_connects {
            add_point(nc.at, NC, &mut points);
        }
        for label in &self.labels {
            match &label.stub {
                None => add_point(label.at, &label.net, &mut points),
                Some(stub) => add_point(stub.pin_at, &label.net, &mut points),
            }
        }
        // Existing wires: power stubs/risers carry the reserved PWR net; cluster
        // wires carry their real net so same-net stubs may touch them.
        for w in &self.wires {
            let net = w.net.clone().unwrap_or_else(|| PWR.to_string());
            segments.push((w.a, w.b, net.clone()));
            // Only register endpoints as points for wires with a known net, so
            // that a same-net stub whose end lands exactly on a cluster wire
            // endpoint is recognized as a deliberate join. Power-wire endpoints
            // stay off the points map (they already block via the segment check,
            // and adding them under PWR would over-retract power stubs that
            // happen to share the same location).
            if let Some(n) = &w.net {
                add_point(w.a, n, &mut points);
                add_point(w.b, n, &mut points);
            }
        }

        // Deterministic processing order for stub labels.
        let mut order: Vec<usize> = (0..self.labels.len())
            .filter(|&i| self.labels[i].stub.is_some())
            .collect();
        order.sort_by(|&a, &b| self.labels[a].uuid_key.cmp(&self.labels[b].uuid_key));

        for i in order {
            let net = self.labels[i].net.clone();
            let end = self.labels[i].at;
            let pin_at = self.labels[i].stub.unwrap().pin_at;

            // Collision if: the end coincides with a foreign point; the end lies
            // on a foreign segment; or the stub segment passes through a foreign
            // point. (The pin endpoint is this net's own anchor, never foreign.)
            // Note: because cluster-wire endpoints are registered as points, a
            // foreign-net cluster wire endpoint landing exactly on this stub's
            // pin endpoint will trip `seg_thru_point` and conservatively retract
            // this stub. This is safe — it falls back to label-on-pin, never a
            // silent merge. Cluster geometry (Task 11) is responsible for not
            // terminating a wire on a foreign component's pin.
            let end_on_point = points
                .get(&bits(end))
                .is_some_and(|nets| nets.iter().any(|n| *n != net));
            let end_on_seg = segments
                .iter()
                .any(|(a, b, n)| *n != net && point_on_segment(end, *a, *b));
            let seg_thru_point = points.iter().any(|(&(xb, yb), nets)| {
                let p = [f64::from_bits(xb), f64::from_bits(yb)];
                nets.iter().any(|n| *n != net) && point_on_segment(p, pin_at, end)
            });

            if end_on_point || end_on_seg || seg_thru_point {
                // Keep the outward dir: the text still reads away from the
                // body (an East reset would run a west-side pin's text back
                // across the pin line, over the pin name).
                self.labels[i].at = pin_at;
                self.labels[i].stub = None;
            } else {
                // The stub wire is attributed to its own net: a later pass (or
                // a re-run of this one) must read it as a deliberate same-net
                // join, not a foreign PWR-sentinel segment — otherwise the
                // second call would retract every survivor onto its pin.
                self.add_wire_on_net(pin_at, end, &net);
                add_point(end, &net, &mut points);
                segments.push((pin_at, end, net));
            }
        }
    }

    /// Assign collision-free positions to all movable text via the greedy
    /// candidate solver in `textplace.rs`.
    ///
    /// **Obstacles:** symbol bodies (angle-aware extents, exempt for text
    /// owned by that refdes), pin name/number text, wires, no-connect markers,
    /// and fixed (stub-less) labels.
    ///
    /// **Movables, most-constrained first:**
    /// 1. *Stub signal labels* (2 candidates): stay at the stub end, or
    ///    retract onto the always-safe pin endpoint keeping the outward dir
    ///    (the stub wire is dropped when retraction wins).
    /// 2. *Reference+Value field pairs* (4 candidates): right / left / above /
    ///    below of the body; wide bodies (rotated passives) prefer
    ///    above/below. The first candidate of an unrotated symbol reproduces
    ///    the legacy fixed right-of-body offsets, so an uncrowded sheet keeps
    ///    its conventional look.
    /// 3. *Power-symbol Values* (rail names; 3 candidates): beyond the symbol
    ///    tip (above for up-pointing rails, below for down-pointing), else
    ///    right / left — so adjacent rails never merge their names.
    ///
    /// Idempotent: every assignment is recomputed from scratch on each call
    /// (a retract-chosen label has no stub on the re-run and becomes a fixed
    /// obstacle at the same position), so reconcile may run it early to lint
    /// solved geometry and `finish`'s own call is a harmless re-run.
    pub fn solve_text_positions(&mut self) {
        use crate::textplace::{
            choose, label_box, pin_text_boxes, rotated_half_extents, text_width, wire_box,
            BBox, Movable, ObKind, Obstacle,
        };
        // Round a candidate coordinate to 0.01 mm: field anchors are derived
        // from float sums (position + extents) and would otherwise render as
        // 107.94999999999999-style noise. Determinism is unaffected (same
        // inputs, same rounding).
        let r2 = |v: f64| (v * 100.0).round() / 100.0;

        // ---- Obstacles ----
        let mut obstacles: Vec<Obstacle> = Vec::new();
        for inst in &self.instances {
            let h = rotated_half_extents(inst.half_extents, inst.angle);
            obstacles.push(Obstacle {
                bbox: [
                    inst.at[0] - h[0],
                    inst.at[1] - h[1],
                    inst.at[0] + h[0],
                    inst.at[1] + h[1],
                ],
                kind: ObKind::OwnExempt(inst.refdes.clone()),
            });
            // Pin name/number text (skip power/flag graphics — single
            // unnamed pin, no meaningful pin text).
            if !inst.refdes.starts_with('#') {
                if let Some(pins) = self.sym_pins.get(&inst.lib_id) {
                    for pg in pins {
                        for b in pin_text_boxes(pg, inst.at, inst.angle, inst.mirror) {
                            obstacles.push(Obstacle { bbox: b, kind: ObKind::Hard });
                        }
                    }
                }
            }
        }
        for w in &self.wires {
            obstacles.push(Obstacle { bbox: wire_box(w.a, w.b), kind: ObKind::Hard });
        }
        for nc in &self.no_connects {
            obstacles.push(Obstacle {
                bbox: [nc.at[0] - 0.64, nc.at[1] - 0.64, nc.at[0] + 0.64, nc.at[1] + 0.64],
                kind: ObKind::Hard,
            });
        }
        // Fixed (stub-less) labels are obstacles; stub labels become movables.
        for l in &self.labels {
            if l.stub.is_none() {
                obstacles.push(Obstacle {
                    bbox: label_box(l.at, l.dir, text_width(&l.net)),
                    kind: ObKind::Hard,
                });
            }
        }

        // ---- Movables ----
        // What to mutate for each movable, parallel to `movables`.
        enum Apply {
            /// labels[i]: candidate 1 retracts onto the pin endpoint.
            StubLabel(usize),
            /// instances[i]: per-candidate (Reference, Value) anchors.
            Fields(usize, Vec<(TextPos, TextPos)>),
            /// instances[i]: per-candidate Value anchor (power rail name).
            PowerVal(usize, Vec<TextPos>),
        }
        let mut movables: Vec<Movable> = Vec::new();
        let mut applies: Vec<Apply> = Vec::new();

        // 1. Stub labels, deterministic uuid_key order (most constrained).
        let mut stub_idx: Vec<usize> = (0..self.labels.len())
            .filter(|&i| self.labels[i].stub.is_some())
            .collect();
        stub_idx.sort_by(|&a, &b| self.labels[a].uuid_key.cmp(&self.labels[b].uuid_key));
        for &i in &stub_idx {
            let l = &self.labels[i];
            let wdt = text_width(&l.net);
            let owner = l.uuid_key.split(':').next().unwrap_or("").to_string();
            movables.push(Movable {
                owner: Some(owner),
                candidates: vec![
                    label_box(l.at, l.dir, wdt),
                    label_box(l.stub.unwrap().pin_at, l.dir, wdt),
                ],
            });
            applies.push(Apply::StubLabel(i));
        }

        // 2./3. Fields and power values, deterministic refdes order.
        let mut order: Vec<usize> = (0..self.instances.len()).collect();
        order.sort_by(|&a, &b| self.instances[a].refdes.cmp(&self.instances[b].refdes));
        for &i in &order {
            let inst = &self.instances[i];
            let h = rotated_half_extents(inst.half_extents, inst.angle);
            let (cx, cy) = (inst.at[0], inst.at[1]);
            let (minx, miny, maxx, maxy) = (cx - h[0], cy - h[1], cx + h[0], cy + h[1]);
            let vw = text_width(&inst.value);

            if inst.refdes.starts_with('#') {
                // Power symbol: the Value IS the rail name. PWR_FLAG hides
                // its Value, so there is nothing to place.
                if inst.lib_id == "power:PWR_FLAG" {
                    continue;
                }
                let above = (
                    TextPos { at: [r2(cx), r2(miny - 0.64)], justify: Justify::Center },
                    [cx - vw / 2.0, miny - 2.24, cx + vw / 2.0, miny - 0.64] as BBox,
                );
                let below = (
                    TextPos { at: [r2(cx), r2(maxy + 2.24)], justify: Justify::Center },
                    [cx - vw / 2.0, maxy + 0.64, cx + vw / 2.0, maxy + 2.24],
                );
                let right = (
                    TextPos { at: [r2(maxx + 0.64), r2(cy + 0.8)], justify: Justify::Left },
                    [maxx + 0.64, cy - 0.8, maxx + 0.64 + vw, cy + 0.8],
                );
                let left = (
                    TextPos { at: [r2(minx - 0.64), r2(cy + 0.8)], justify: Justify::Right },
                    [minx - 0.64 - vw, cy - 0.8, minx - 0.64, cy + 0.8],
                );
                // A 180-rotated power symbol points down (GND family): the
                // name goes below the graphic; otherwise above.
                let cands = if inst.angle == 180.0 {
                    vec![below, right, left]
                } else {
                    vec![above, right, left]
                };
                movables.push(Movable {
                    owner: Some(inst.refdes.clone()),
                    candidates: cands.iter().map(|c| c.1).collect(),
                });
                applies.push(Apply::PowerVal(i, cands.into_iter().map(|c| c.0).collect()));
                continue;
            }

            let rw = text_width(&inst.refdes);
            let wmax = rw.max(vw);
            // Each candidate: (ref anchor, val anchor, union bbox). Text is
            // bottom-anchored and 1.6 tall, so a line anchored at Y occupies
            // [Y-1.6, Y].
            let right = (
                TextPos { at: [r2(maxx + 1.27), r2(cy - 1.27)], justify: Justify::Left },
                TextPos { at: [r2(maxx + 1.27), r2(cy + 1.27)], justify: Justify::Left },
                [maxx + 1.27, cy - 2.87, maxx + 1.27 + wmax, cy + 1.27] as BBox,
            );
            let left = (
                TextPos { at: [r2(minx - 1.27), r2(cy - 1.27)], justify: Justify::Right },
                TextPos { at: [r2(minx - 1.27), r2(cy + 1.27)], justify: Justify::Right },
                [minx - 1.27 - wmax, cy - 2.87, minx - 1.27, cy + 1.27],
            );
            let above = (
                TextPos { at: [r2(cx), r2(miny - 3.18)], justify: Justify::Center },
                TextPos { at: [r2(cx), r2(miny - 0.64)], justify: Justify::Center },
                [cx - wmax / 2.0, miny - 4.78, cx + wmax / 2.0, miny - 0.64],
            );
            let below = (
                TextPos { at: [r2(cx), r2(maxy + 2.24)], justify: Justify::Center },
                TextPos { at: [r2(cx), r2(maxy + 4.78)], justify: Justify::Center },
                [cx - wmax / 2.0, maxy + 0.64, cx + wmax / 2.0, maxy + 4.78],
            );
            // Corner fallbacks for crowded symbols (an IC whose four sides all
            // carry labels/power): the field pair tucks against a body corner.
            let above_left = (
                TextPos { at: [r2(minx), r2(miny - 3.18)], justify: Justify::Left },
                TextPos { at: [r2(minx), r2(miny - 0.64)], justify: Justify::Left },
                [minx, miny - 4.78, minx + wmax, miny - 0.64] as BBox,
            );
            let above_right = (
                TextPos { at: [r2(maxx), r2(miny - 3.18)], justify: Justify::Right },
                TextPos { at: [r2(maxx), r2(miny - 0.64)], justify: Justify::Right },
                [maxx - wmax, miny - 4.78, maxx, miny - 0.64],
            );
            let below_left = (
                TextPos { at: [r2(minx), r2(maxy + 2.24)], justify: Justify::Left },
                TextPos { at: [r2(minx), r2(maxy + 4.78)], justify: Justify::Left },
                [minx, maxy + 0.64, minx + wmax, maxy + 4.78],
            );
            let below_right = (
                TextPos { at: [r2(maxx), r2(maxy + 2.24)], justify: Justify::Right },
                TextPos { at: [r2(maxx), r2(maxy + 4.78)], justify: Justify::Right },
                [maxx - wmax, maxy + 0.64, maxx, maxy + 4.78],
            );
            // Last-resort FAR bands (pushed ~5 mm further out): when a body is
            // ringed by packed neighbours — a tight decoupling cluster on a dense
            // board — every near spot is blocked and the solver would fall onto a
            // sibling's label/field. A far band clears it (the text reads a touch
            // detached but never overlaps). Appended LAST for both ICs and passives,
            // so a part with any near free spot is unaffected.
            let above_far = (
                TextPos { at: [r2(cx), r2(miny - 8.18)], justify: Justify::Center },
                TextPos { at: [r2(cx), r2(miny - 5.64)], justify: Justify::Center },
                [cx - wmax / 2.0, miny - 9.78, cx + wmax / 2.0, miny - 5.64] as BBox,
            );
            let below_far = (
                TextPos { at: [r2(cx), r2(maxy + 5.64)], justify: Justify::Center },
                TextPos { at: [r2(cx), r2(maxy + 8.18)], justify: Justify::Center },
                [cx - wmax / 2.0, maxy + 5.64, cx + wmax / 2.0, maxy + 9.78],
            );
            // Multi-pin parts (ICs) carry refdes+value on a HORIZONTAL band
            // (above/below the body), the reference convention — a long MPN
            // ("SN74LVC2T45DCUR") on a band clears the horizontal series
            // neighbours (R15/R13) it would smear onto placed to the side. Such a
            // wide value rarely fits any fully-clear gap, so the solver falls back
            // to candidate 0; that candidate must be the band/corner clear of this
            // IC's OWN pin text (the artifact the lint catches and the eye reads
            // as broken). We therefore stable-sort the band candidates by how many
            // of the IC's pin-text boxes they hit: MCP1703 (GND exits bottom) →
            // above wins; SN74 (VCC top, GND bottom-centre) → below-left/right
            // win, dodging the centre GND drop. Passives keep the KiCAD
            // convention: wide (rotated) bodies prefer above/below, tall prefer
            // right/left.
            let is_ic =
                self.sym_pins.get(&inst.lib_id).is_some_and(|p| p.len() >= 3);
            let cands = if is_ic {
                let pin_boxes: Vec<BBox> = self
                    .sym_pins
                    .get(&inst.lib_id)
                    .map(|pins| {
                        pins.iter()
                            .flat_map(|pg| pin_text_boxes(pg, inst.at, inst.angle, inst.mirror))
                            .collect()
                    })
                    .unwrap_or_default();
                let hits = |c: &(TextPos, TextPos, BBox)| {
                    pin_boxes.iter().filter(|pb| boxes_overlap(&c.2, pb)).count()
                };
                let mut bands =
                    vec![below, above, below_left, below_right, above_left, above_right];
                bands.sort_by_key(hits);
                // Far bands (detached but clear of the body's OWN pins) BEFORE right/left: on a crowded
                // IC whose every near band is blocked by a decoupling cap, right/left sit at the body
                // edge ON the side pins, so the refdes/value smears across the pin stubs (the
                // TPA3116 / driver-IC "value over pins 16/17" defect). A slightly-detached far band
                // reads far better than text over the pins; right/left stay the genuine last resort.
                // Multi-sheet sub-sheets only (where the dense power-IC sheets live) so the single-sheet
                // reference snapshots stay byte-identical.
                if std::env::var("MULTISHEET_REFINE").is_ok() {
                    bands.push(above_far);
                    bands.push(below_far);
                    bands.push(right);
                    bands.push(left);
                } else {
                    bands.push(right);
                    bands.push(left);
                    bands.push(above_far);
                    bands.push(below_far);
                }
                bands
            } else if h[0] > h[1] {
                vec![
                    above, below, right, left, above_left, above_right, below_left, below_right,
                    above_far, below_far,
                ]
            } else {
                vec![
                    right, left, above, below, above_left, above_right, below_left, below_right,
                    above_far, below_far,
                ]
            };
            movables.push(Movable {
                owner: Some(inst.refdes.clone()),
                candidates: cands.iter().map(|c| c.2).collect(),
            });
            applies.push(Apply::Fields(i, cands.into_iter().map(|c| (c.0, c.1)).collect()));
        }

        // ---- Solve and apply ----
        let picks = choose(&obstacles, &movables);
        for (apply, (pick, fits)) in applies.into_iter().zip(picks) {
            match apply {
                Apply::StubLabel(i) => {
                    if pick == 1 {
                        let pin_at = self.labels[i].stub.unwrap().pin_at;
                        let end = self.labels[i].at;
                        // Drop the stub wire retract_colliding_stubs
                        // materialized (content-derived key).
                        let a = snap_point(pin_at);
                        let b = snap_point(end);
                        let key = format!("{}:{}:{}:{}", a[0], a[1], b[0], b[1]);
                        self.wires.retain(|w| w.uuid_key != key);
                        self.labels[i].at = pin_at;
                        self.labels[i].stub = None;
                    }
                }
                Apply::Fields(i, cands) => {
                    let (r, v) = cands[pick];
                    self.instances[i].ref_pos = Some(r);
                    self.instances[i].val_pos = Some(v);
                }
                Apply::PowerVal(i, cands) => {
                    // A rail name with no free spot is OPTIONAL text: hide it
                    // rather than smear it over a sibling. Greedy order means
                    // the first symbol of a tight same-rail run shows the
                    // name and the rest hide — the conventional tidy look.
                    self.instances[i].val_pos = Some(cands[pick]);
                    self.instances[i].val_hidden = !fits;
                }
            }
        }
    }

    /// Build the routing obstacle scene from everything placed so far.
    ///
    /// Solids are symbol bodies SHRUNK by 2.54 mm per side: `approx_size` pads
    /// 2.54 beyond the pin endpoints, so shrinking puts pin connection points
    /// exactly ON the solid boundary (open-interval checks let wires depart
    /// from them) while the glyph stays protected. Points carry the same
    /// foreign-anchor model as `retract_colliding_stubs` (power origins,
    /// no-connects, label anchors); wire segments carry their net (the power
    /// sentinel for unattributed stubs/risers).
    pub fn route_scene(&self) -> crate::route::RouteScene {
        use crate::textplace::rotated_half_extents;
        const NC: &str = "\0no_connect";
        const PWR: &str = "\0power_wire";
        let mut scene = crate::route::RouteScene {
            solids: Vec::new(),
            points: Vec::new(),
            segments: Vec::new(),
            label_solids: Vec::new(),
        };
        for inst in &self.instances {
            if inst.refdes.starts_with('#') {
                // Power symbols: the single pin at the origin is the anchor.
                scene.points.push((inst.at, inst.value.clone()));
                continue;
            }
            let h = rotated_half_extents(inst.half_extents, inst.angle);
            let (hx, hy) = ((h[0] - 2.54).max(1.27), (h[1] - 2.54).max(1.27));
            scene.solids.push([
                inst.at[0] - hx,
                inst.at[1] - hy,
                inst.at[0] + hx,
                inst.at[1] + hy,
            ]);
        }
        // A no-connect X is a glyph with real extent, owned by no net. Register it
        // both as a foreign anchor point (a wire may not pass exactly through it) AND
        // as a net-tagged keepout box, so the router DETOURS foreign wires around the
        // glyph instead of grazing it — the X never lands on top of a wire. The
        // sentinel net matches no real net, so every wire is foreign and detours.
        const NC_KEEPOUT: f64 = 1.27; // ≈ the X half-extent (~0.7 mm) plus margin.
        for nc in &self.no_connects {
            scene.points.push((nc.at, NC.to_string()));
            scene.label_solids.push((
                [nc.at[0] - NC_KEEPOUT, nc.at[1] - NC_KEEPOUT, nc.at[0] + NC_KEEPOUT, nc.at[1] + NC_KEEPOUT],
                NC.to_string(),
            ));
        }
        for l in &self.labels {
            scene.points.push((l.at, l.net.clone()));
            if let Some(stub) = &l.stub {
                scene.points.push((stub.pin_at, l.net.clone()));
            }
        }
        for w in &self.wires {
            let net = w.net.clone().unwrap_or_else(|| PWR.to_string());
            scene.segments.push((w.a, w.b, net));
        }
        scene
    }

    /// Wire segments attributed to `net` (for junction counting at taps).
    pub fn wire_segments_on_net(&self, net: &str) -> Vec<([f64; 2], [f64; 2])> {
        self.wires
            .iter()
            .filter(|w| w.net.as_deref() == Some(net))
            .map(|w| (w.a, w.b))
            .collect()
    }

    /// Junction-dot count (a routing-quality signal for the refinement scorer).
    pub(crate) fn junction_count(&self) -> usize {
        self.junctions.len()
    }

    /// Junction-dot positions (for the scorer's merge check: a junction sitting
    /// on wires of two different nets fuses them).
    pub(crate) fn junction_positions(&self) -> Vec<[f64; 2]> {
        self.junctions.iter().map(|j| j.at).collect()
    }

    /// Count of plain (non-global) labels — i.e. signal-label fallbacks where the
    /// router could not wire a net. Port pentagons are `global` and excluded, so
    /// this is a direct "how many nets degraded to labels" signal.
    pub(crate) fn signal_label_count(&self) -> usize {
        self.labels.iter().filter(|l| !l.global).count()
    }

    /// Bounding boxes of the global/port labels (the edge pentagons), for the
    /// refinement scorer to keep symbol bodies from colliding with a port label
    /// (the label is placed during routing, so it is not an `Item`).
    pub(crate) fn cluster_label_boxes(&self) -> Vec<[f64; 4]> {
        self.labels
            .iter()
            .filter(|l| l.global)
            .map(|l| {
                let w = crate::textplace::text_width(&l.net) + 2.54;
                [l.at[0] - w, l.at[1] - 2.0, l.at[0] + w, l.at[1] + 2.0]
            })
            .collect()
    }

    /// Every drawn wire segment with its net (`None` for unattributed power
    /// stubs). For the refinement scorer's crossing / length / short metrics.
    pub(crate) fn wires_with_nets(&self) -> Vec<([f64; 2], [f64; 2], Option<String>)> {
        self.wires.iter().map(|w| (w.a, w.b, w.net.clone())).collect()
    }

    /// Assemble the complete `.kicad_sch` document as a deterministic string.
    ///
    /// `lib_symbols` are emitted sorted by `lib_id` (via the backing
    /// `BTreeMap`); symbol instances are emitted sorted by refdes. All uuids are
    /// content-derived, so the same placements always produce identical bytes.
    /// The page size for a content-fit `User` page: the maximum x/y extent of
    /// all drawn geometry (symbol bodies, wires, labels, junctions, no-connects)
    /// plus a margin. `None` when there is nothing to draw.
    ///
    /// Geometry is laid out near the origin by the floorplan engine, so the
    /// content fills a page of `max + margin`. The minimum corner is not
    /// subtracted (KiCAD's page origin is the top-left); the floorplan
    /// normalizes content to a small positive margin already.
    fn content_extent(&self) -> Option<[f64; 2]> {
        use crate::textplace::{rotated_half_extents, text_width};
        const PAGE_MARGIN: f64 = 12.7;
        let mut max_x = f64::MIN;
        let mut max_y = f64::MIN;
        let mut acc = |x: f64, y: f64| {
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        };
        for i in &self.instances {
            let h = rotated_half_extents(i.half_extents, i.angle);
            acc(i.at[0] + h[0], i.at[1] + h[1]);
            for p in [i.ref_pos, i.val_pos].into_iter().flatten() {
                acc(p.at[0] + 5.0, p.at[1]);
            }
        }
        for w in &self.wires {
            acc(w.a[0], w.a[1]);
            acc(w.b[0], w.b[1]);
        }
        for l in &self.labels {
            acc(l.at[0] + text_width(&l.net), l.at[1]);
        }
        for j in &self.junctions {
            acc(j.at[0], j.at[1]);
        }
        for nc in &self.no_connects {
            acc(nc.at[0], nc.at[1]);
        }
        if max_x == f64::MIN {
            return None;
        }
        Some([max_x + PAGE_MARGIN, max_y + PAGE_MARGIN])
    }

    /// Split each wire at every junction / other-wire endpoint lying strictly in
    /// its interior, so every electrical tap is an endpoint-to-endpoint join.
    ///
    /// KiCAD's netlister connects wires only where they share an endpoint (with a
    /// junction dot marking a ≥3-way meet); a tap whose riser ends on a
    /// through-wire's MID-SPAN does **not** connect unless that through-wire is
    /// physically split at the tap. The router draws long rails/trunks with
    /// junction dots but never splits them, so without this pass every mid-span
    /// tap is silently disconnected (decoupling caps off a rail, a filter cap off
    /// an OUT trunk, …) — the schematic renders fine but netlists wrong. Run once
    /// at finalize. Only endpoints-on-interior split a wire, so a clean
    /// perpendicular crossing of two different nets is never split (and never
    /// merged): the router already forbids a foreign endpoint on our wire, so any
    /// interior node is a same-net tap.
    fn split_wires_at_nodes(&mut self) {
        const EPS: f64 = 1e-6;
        let same = |p: [f64; 2], q: [f64; 2]| (p[0] - q[0]).abs() < EPS && (p[1] - q[1]).abs() < EPS;
        // Candidate split points: every junction position + every wire endpoint.
        let mut pts: Vec<[f64; 2]> = self.junctions.iter().map(|j| j.at).collect();
        for w in &self.wires {
            pts.push(w.a);
            pts.push(w.b);
        }
        let mk = |a: [f64; 2], b: [f64; 2], net: Option<String>| Wire {
            a,
            b,
            uuid_key: format!("{}:{}:{}:{}", a[0], a[1], b[0], b[1]),
            net,
        };
        // Iteratively split until stable (a rail tapped at N points needs N passes).
        loop {
            let mut next: Vec<Wire> = Vec::with_capacity(self.wires.len());
            let mut changed = false;
            for w in &self.wires {
                // The interior split point closest to `a` (deterministic order).
                let mut best: Option<[f64; 2]> = None;
                let mut best_d = f64::INFINITY;
                for &p in &pts {
                    if same(p, w.a) || same(p, w.b) || !point_on_segment(p, w.a, w.b) {
                        continue;
                    }
                    let d = (p[0] - w.a[0]).abs() + (p[1] - w.a[1]).abs();
                    if d < best_d {
                        best_d = d;
                        best = Some(p);
                    }
                }
                match best {
                    Some(p) => {
                        next.push(mk(w.a, p, w.net.clone()));
                        next.push(mk(p, w.b, w.net.clone()));
                        changed = true;
                    }
                    None => next.push(w.clone()),
                }
            }
            self.wires = next;
            if !changed {
                break;
            }
        }
        // Dedup any sub-segments that coincide after splitting (keep first).
        let mut seen = std::collections::BTreeSet::new();
        self.wires.retain(|w| seen.insert(w.uuid_key.clone()));
    }

    /// Shift the whole drawing so its true minimum corner — including the rail
    /// power symbols, edge port labels, and solved field text that extend beyond
    /// the symbol bodies — lands at the page margin. The floorplan's `normalize`
    /// only shifts symbol bodies, and it runs *before* wiring adds those edge
    /// elements, so a left/top port label can otherwise sit at a negative
    /// coordinate and be clipped off the content-fit page. Run last, after text is
    /// solved, so field positions move with their symbols.
    fn reframe(&mut self) {
        use crate::textplace::{rotated_half_extents, text_width};
        const M: f64 = 12.7;
        let (mut minx, mut miny) = (f64::MAX, f64::MAX);
        let mut lo = |x: f64, y: f64| {
            minx = minx.min(x);
            miny = miny.min(y);
        };
        for i in &self.instances {
            let h = rotated_half_extents(i.half_extents, i.angle);
            lo(i.at[0] - h[0], i.at[1] - h[1]);
            for p in [i.ref_pos, i.val_pos].into_iter().flatten() {
                lo(p.at[0] - 5.0, p.at[1] - 1.6);
            }
        }
        for w in &self.wires {
            lo(w.a[0], w.a[1]);
            lo(w.b[0], w.b[1]);
        }
        for l in &self.labels {
            // A right-justified edge label (a left/top port) extends back toward
            // smaller x by its text width; cover both directions conservatively.
            lo(l.at[0] - text_width(&l.net), l.at[1] - 1.6);
        }
        for j in &self.junctions {
            lo(j.at[0], j.at[1]);
        }
        for nc in &self.no_connects {
            lo(nc.at[0], nc.at[1]);
        }
        for t in &self.texts {
            lo(t.at[0], t.at[1] - 1.6);
        }
        for r in &self.rects {
            lo(r.start[0].min(r.end[0]), r.start[1].min(r.end[1]));
        }
        if minx == f64::MAX {
            return;
        }
        // Snap the shift to the grid: all wire/pin geometry is grid-aligned, so a
        // grid-multiple shift keeps it grid-aligned (KiCAD ERCs off-grid endpoints).
        // `minx`/`miny` include off-grid text extents, so an unsnapped shift would
        // knock the whole sheet off the 1.27 mm grid.
        let (dx, dy) = (crate::grid::snap(M - minx), crate::grid::snap(M - miny));
        if dx.abs() < 1e-9 && dy.abs() < 1e-9 {
            return;
        }
        let sh = |p: &mut [f64; 2]| {
            p[0] += dx;
            p[1] += dy;
        };
        for i in &mut self.instances {
            sh(&mut i.at);
            if let Some(p) = &mut i.ref_pos {
                sh(&mut p.at);
            }
            if let Some(p) = &mut i.val_pos {
                sh(&mut p.at);
            }
        }
        for w in &mut self.wires {
            sh(&mut w.a);
            sh(&mut w.b);
        }
        for l in &mut self.labels {
            sh(&mut l.at);
            if let Some(s) = &mut l.stub {
                sh(&mut s.pin_at);
            }
        }
        for j in &mut self.junctions {
            sh(&mut j.at);
        }
        for nc in &mut self.no_connects {
            sh(&mut nc.at);
        }
        for t in &mut self.texts {
            sh(&mut t.at);
        }
        for r in &mut self.rects {
            sh(&mut r.start);
            sh(&mut r.end);
        }
    }

    /// Run every geometry-finalizing pass: stub retraction, wire splitting at
    /// taps, text placement, and reframing. All four are idempotent, so calling
    /// this before [`Self::layout_warnings`] (to lint the *final* geometry) and
    /// then [`Self::finish`] (which re-runs it harmlessly) is safe and is how the
    /// floorplan engine reports truthful, post-solve warnings.
    pub fn prepare(&mut self) {
        // Dedup labels that are IDENTICAL (same net) AND COINCIDENT (same point): a multi-unit BGA
        // stacks its many same-rail power balls onto ONE schematic point, so each pin's signal label
        // lands exactly on top of the previous one — 686 overlapping "P1V1"/"P2V5" labels tanked an
        // ECP5 core sheet. The coincident pins are already electrically joined, so one label per
        // (net, point) suffices. Keyed to 1µm, so DISTINCT grid pins keep their own labels ⇒ the
        // single-sheet reference fixtures (no coincident pins) are byte-identical.
        {
            let mut seen: std::collections::HashSet<(String, i64, i64)> =
                std::collections::HashSet::new();
            self.labels.retain(|l| {
                seen.insert((
                    l.net.clone(),
                    (l.at[0] * 1000.0).round() as i64,
                    (l.at[1] * 1000.0).round() as i64,
                ))
            });
        }
        // Resolve signal-stub collisions and materialize the surviving stub wires
        // before any rendering, so labels/wires below render the reconciled state.
        self.retract_colliding_stubs();
        // Split through-wires at their taps so every junction actually connects in
        // the netlist (KiCAD won't connect a mid-span tap on an unsplit wire).
        self.split_wires_at_nodes();
        // Then place movable text (fields, stub labels) collision-free against
        // the final geometry.
        self.solve_text_positions();
        // Finally reframe so nothing (edge port labels, rail symbols) is clipped
        // off the content-fit page (floorplan path only).
        if self.frame {
            self.reframe();
        }
    }

    /// Enable [`Self::reframe`] at finalize (floorplan engine).
    pub fn set_frame(&mut self, on: bool) {
        self.frame = on;
    }

    pub fn finish(mut self) -> String {
        self.prepare();

        let root_uuid = stable_uuid("sheet", ROOT_SHEET_KEY);

        let mut out = String::new();
        out.push_str("(kicad_sch\n");
        out.push_str("\t(version 20250114)\n");
        out.push_str("\t(generator \"auto-pcb\")\n");
        out.push_str("\t(generator_version \"0.1\")\n");
        let _ = writeln!(out, "\t(uuid \"{root_uuid}\")");
        // Content-fit page: a custom `User` page just larger than the drawn
        // content so the schematic fills the view (no tiny-in-an-A4-corner).
        // Falls back to A4 when there is no content to measure.
        match self.content_extent() {
            Some([w, h]) => {
                // A multi-sheet sub-sheet carries a title block, which KiCAD draws at the page
                // bottom-right; the tight content-fit page leaves no room, so it overprints the
                // lowest parts (the committed-sheet defect — content-only renders hide it).
                // Reserve a bottom band so content sits above it. Gated on MULTISHEET_REFINE +
                // a title, so single-sheet references (no MULTISHEET_REFINE) stay byte-identical.
                const TITLE_BLOCK_RESERVE: f64 = 33.0;
                let reserve = self.title.is_some() && std::env::var("MULTISHEET_REFINE").is_ok();
                let h = if reserve { h + TITLE_BLOCK_RESERVE } else { h };
                let _ = writeln!(out, "\t(paper \"User\" {} {})", fmt_coord(w), fmt_coord(h));
            }
            None => out.push_str("\t(paper \"A4\")\n"),
        }
        if let Some(title) = &self.title {
            let t = escape_sexpr_string(title);
            let _ = writeln!(out, "\t(title_block\n\t\t(title \"{t}\")\n\t)");
        }

        // lib_symbols set, sorted by lib_id (BTreeMap order).
        out.push_str("\t(lib_symbols\n");
        for body in self.lib_symbols.values() {
            out.push_str("\t\t");
            out.push_str(body);
            out.push('\n');
        }
        out.push_str("\t)\n");

        // `(no_connect)` markers at intentionally-unconnected pins, sorted by
        // their stable uuid_key for deterministic order/uuids.
        let mut no_connects = self.no_connects;
        no_connects.sort_by(|a, b| a.uuid_key.cmp(&b.uuid_key));
        for nc in &no_connects {
            out.push_str(&render_no_connect(nc));
        }

        // Net-name labels at pin endpoints, sorted by their stable uuid_key so
        // the emitted order (and uuids) are deterministic.
        let mut labels = self.labels;
        labels.sort_by(|a, b| a.uuid_key.cmp(&b.uuid_key));
        for label in &labels {
            out.push_str(&render_label(label));
        }

        // Wire segments, sorted by uuid_key for deterministic order.
        let mut wires = self.wires;
        wires.sort_by(|a, b| a.uuid_key.cmp(&b.uuid_key));
        for wire in &wires {
            let uuid = stable_uuid("wire", &wire.uuid_key);
            let _ = writeln!(
                out,
                "\t(wire\n\t\t(pts\n\t\t\t(xy {} {}) (xy {} {})\n\t\t)\n\t\t(stroke (width 0) (type default))\n\t\t(uuid \"{uuid}\")\n\t)",
                fmt_coord(wire.a[0]),
                fmt_coord(wire.a[1]),
                fmt_coord(wire.b[0]),
                fmt_coord(wire.b[1]),
            );
        }

        // Junction dots, sorted by uuid_key for deterministic order/uuids.
        let mut junctions = self.junctions;
        junctions.sort_by(|a, b| a.uuid_key.cmp(&b.uuid_key));
        for j in &junctions {
            let uuid = stable_uuid("junction", &j.uuid_key);
            let _ = writeln!(
                out,
                "\t(junction\n\t\t(at {} {})\n\t\t(diameter 0)\n\t\t(color 0 0 0 0)\n\t\t(uuid \"{uuid}\")\n\t)",
                fmt_coord(j.at[0]),
                fmt_coord(j.at[1]),
            );
        }

        // Free-standing graphic decoration (block titles/notes + frames), both
        // sorted by uuid_key for deterministic order/uuids.
        let mut texts = self.texts;
        texts.sort_by(|a, b| a.uuid_key.cmp(&b.uuid_key));
        let mut rects = self.rects;
        rects.sort_by(|a, b| a.uuid_key.cmp(&b.uuid_key));
        for t in &texts {
            let body = escape_sexpr_string(&t.text);
            let uuid = stable_uuid("text", &t.uuid_key);
            let weight = if t.bold { " bold" } else { "" };
            let _ = writeln!(
                out,
                "\t(text \"{body}\"\n\t\t(exclude_from_sim no)\n\t\t(at {} {} 0)\n\t\t(effects (font (size {sz} {sz}){weight}) (justify left bottom))\n\t\t(uuid \"{uuid}\")\n\t)",
                fmt_coord(t.at[0]), fmt_coord(t.at[1]), sz = t.size,
            );
        }
        for r in &rects {
            let uuid = stable_uuid("rect", &r.uuid_key);
            let _ = writeln!(
                out,
                "\t(rectangle\n\t\t(start {} {})\n\t\t(end {} {})\n\t\t(stroke (width 0.1524) (type dash))\n\t\t(fill (type none))\n\t\t(uuid \"{uuid}\")\n\t)",
                fmt_coord(r.start[0]), fmt_coord(r.start[1]),
                fmt_coord(r.end[0]), fmt_coord(r.end[1]),
            );
        }

        // Symbol instances, sorted by refdes for deterministic output.
        let mut instances = self.instances;
        instances.sort_by(|a, b| a.refdes.cmp(&b.refdes));
        for inst in &instances {
            out.push_str(&render_instance(inst, &root_uuid));
        }

        // A single root sheet.
        out.push_str("\t(sheet_instances\n");
        out.push_str("\t\t(path \"/\"\n");
        out.push_str("\t\t\t(page \"1\")\n");
        out.push_str("\t\t)\n");
        out.push_str("\t)\n");

        out.push_str(")\n");
        out
    }
}

/// Escape a free-form string for embedding inside a double-quoted S-expr atom.
///
/// KiCAD S-expressions quote string atoms with `"`; a literal backslash or
/// double-quote in the payload must be escaped or the document fails to parse.
/// Order matters: escape backslash first, then the quote, so the backslash we
/// add in front of a quote is not itself doubled.
///
/// Apply this to every LLM-/user-derived string written as `"…"` (e.g. the
/// component value). Do **not** apply it to the verbatim `raw_definition`
/// splice (already valid KiCAD output) or to internally generated tokens
/// (uuids, validated lib_ids).
fn escape_sexpr_string(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Format a snapped coordinate, canonicalizing `-0.0` to `0.0`.
///
/// Snapping can produce `-0.0`, which `f64`'s `Display` renders as `-0`. That
/// is harmless to KiCAD but breaks byte-for-byte determinism (the same logical
/// position could render as `0` or `-0`), so we collapse negative zero here.
pub fn fmt_coord(v: f64) -> f64 {
    if v == 0.0 { 0.0 } else { v }
}

/// Transform a symbol-space offset (y up) into a sheet-space offset (y down)
/// for an instance at `angle` degrees, optionally mirrored.
///
/// This is the offset half of [`pin_endpoint`]: mirror → rotate → y-flip,
/// returning `[rx, -ry]`. Cluster geometry reuses it to reason about pin ends
/// before any instance position is known.
/// Angle-0 sheet-space pin-end offsets (relative to the symbol origin) of a
/// pin, resolved against `lib_id`'s geometry by number first then name.
///
/// The single source of truth for "where does this pin land at instance angle
/// 0" — used by cluster geometry's pin callback (which takes the first end) and
/// by anchor-pin slotting (offset + [`quantize_dir`]). A pin *name* can match
/// several physical pins, so a `Vec` is returned. The offset is
/// `transform_offset(pin.at, 0.0, false)`, i.e. `[pin.x, -pin.y]`.
pub fn pin_end0(env: &KicadEnv, lib_id: &str, pin: &str) -> io::Result<Vec<[f64; 2]>> {
    let geom = SymbolGeometry::load(env, lib_id)?;
    let matches: Vec<&PinGeom> = {
        let by_number: Vec<&PinGeom> = geom.pins.iter().filter(|p| p.number == pin).collect();
        if !by_number.is_empty() {
            by_number
        } else {
            geom.pins.iter().filter(|p| p.name == pin).collect()
        }
    };
    Ok(matches
        .into_iter()
        .map(|pg| transform_offset(pg.at, 0.0, false))
        .collect())
}

/// Quantize a pin's outward direction to the four sheet axes.
///
/// `pin_angle` is the pin's local `(at … angle)` in the symbol — it points from
/// the connection tip INTO the body, so outward (away from the body) is
/// `pin_angle + 180`. That outward vector is transformed by the instance
/// orientation (mirror → rotate → sheet Y-flip) exactly like the endpoint, then
/// snapped to the dominant axis. Shared by [`SchematicWriter::pin_dirs`] (stub
/// directions) and anchor-pin slotting (cluster join sides).
pub fn quantize_dir(pin_angle: f64, inst_angle: f64, mirror: bool) -> Dir {
    let theta = (pin_angle + 180.0).to_radians();
    let (mut dx, dy) = (theta.cos(), theta.sin());
    if mirror {
        dx = -dx;
    }
    let phi = inst_angle.to_radians();
    let (s, c) = phi.sin_cos();
    let rx = dx * c - dy * s;
    let ry = dx * s + dy * c;
    // Sheet flip: sheet-space y component is -ry (symbol Y up, sheet Y down).
    let sy = -ry;
    if rx.abs() >= sy.abs() {
        if rx >= 0.0 { Dir::East } else { Dir::West }
    } else if sy >= 0.0 {
        Dir::South
    } else {
        Dir::North
    }
}

pub(crate) fn transform_offset(local: [f64; 2], angle: f64, mirror: bool) -> [f64; 2] {
    let (mut x, y) = (local[0], local[1]);
    if mirror {
        x = -x;
    }
    let phi = angle.to_radians();
    let (s, c) = phi.sin_cos();
    let rx = x * c - y * s;
    let ry = x * s + y * c;
    [rx, -ry]
}

/// Compute the sheet-space connection endpoint of a pin on a placed instance.
///
/// ## What "connection endpoint" means
///
/// In a `.kicad_sym`, a pin's `(at x y angle)` is the pin's **connection point**
/// — the tip where wires/labels attach — and the pin line extends `length` mm
/// *into the symbol body* along `angle`. So the connection point is exactly the
/// pin's local `at`; no `length` projection is applied (projecting by `length`
/// would land inside the body, off the connection). E.g. Device:R pin 1 at local
/// `(0, 3.81)` maps to sheet `(inst_x, inst_y - 3.81)`.
///
/// ## The transform (symbol space → sheet space)
///
/// KiCAD symbol Y grows **upward**; the schematic sheet Y grows **downward**. A
/// placed instance applies, in order: an optional X-mirror, a rotation by the
/// instance `angle`, then the Y-flip into sheet space, then a translation to the
/// instance position. Concretely, for a local point `(lx, ly)`:
///
/// 1. **Mirror** (`(mirror x)`): negate `lx` → `(-lx, ly)`.
/// 2. **Rotate** by the instance angle θ (KiCAD rotates counter-clockwise in
///    symbol space): `(lx·cosθ − ly·sinθ, lx·sinθ + ly·cosθ)`.
/// 3. **Y-flip + translate**: `sheet = (inst_x + rx, inst_y − ry)`.
///
/// At θ = 0 with no mirror this reduces to `(inst_x + lx, inst_y − ly)`, the
/// spike's proven form. Angles are restricted to 0/90/180/270 in practice, so
/// the sin/cos are exact (±1, 0) and the result stays on the grid; we still snap
/// to absorb floating-point dust.
pub(crate) fn pin_endpoint(pin: &PinGeom, inst_at: [f64; 2], inst_angle: f64, mirror: bool) -> [f64; 2] {
    let off = transform_offset(pin.at, inst_angle, mirror);
    snap_point([inst_at[0] + off[0], inst_at[1] + off[1]])
}

/// Whether point `p` lies on the axis-aligned segment `a`–`b` (endpoints
/// included), within grid-snap floating-point dust.
///
/// All stub/power wires are horizontal or vertical, so the test reduces to: `p`
/// is collinear with the segment's constant axis and within its varying-axis
/// span. Endpoints count as "on" — a stub end meeting a foreign wire's endpoint
/// is just as much a connection as meeting its middle.
pub fn point_on_segment(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> bool {
    const EPS: f64 = 1e-6;
    let within = |v: f64, lo: f64, hi: f64| v >= lo - EPS && v <= hi + EPS;
    if (a[0] - b[0]).abs() < EPS {
        // Vertical segment: x constant.
        (p[0] - a[0]).abs() < EPS && within(p[1], a[1].min(b[1]), a[1].max(b[1]))
    } else if (a[1] - b[1]).abs() < EPS {
        // Horizontal segment: y constant.
        (p[1] - a[1]).abs() < EPS && within(p[0], a[0].min(b[0]), a[0].max(b[0]))
    } else {
        // Non-axis-aligned (should not occur for our wires): fall back to the
        // collinearity + bounding-box test.
        let cross = (p[0] - a[0]) * (b[1] - a[1]) - (p[1] - a[1]) * (b[0] - a[0]);
        cross.abs() < EPS
            && within(p[0], a[0].min(b[0]), a[0].max(b[0]))
            && within(p[1], a[1].min(b[1]), a[1].max(b[1]))
    }
}

/// Render one net-name label at a pin endpoint into a `(label …)` block.
///
/// The net name is free-form (LLM-/user-derived), so it is escaped before
/// embedding. The label's rotation + justification derive from its `dir` so the
/// text reads *away* from the symbol body along the stub: East→0/left,
/// West→180/right, North→90/left, South→270/right. (Rotation does not affect
/// connectivity — a label binds to whatever pin shares its `(at …)` — only how
/// the text reads.) The `East` case is byte-identical to the pre-stub output
/// (angle 0, justify left bottom). The uuid is content-derived from the label's
/// stable key for byte-identical re-emission.
fn render_label(label: &PinLabel) -> String {
    let x = fmt_coord(label.at[0]);
    let y = fmt_coord(label.at[1]);
    let net = escape_sexpr_string(&label.net);
    let uuid = stable_uuid("label", &label.uuid_key);
    let (angle, justify) = match label.dir {
        Dir::East => (0, "left"),
        Dir::West => (180, "right"),
        Dir::North => (90, "left"),
        Dir::South => (270, "right"),
    };

    let mut s = String::new();
    if label.global {
        // A port: render the off-sheet I/O pentagon. `bidirectional` suits a
        // generic board-edge signal and KiCAD does not flag a global label as an
        // isolated single-pin net (it is, by definition, a cross-sheet link).
        let _ = writeln!(s, "\t(global_label \"{net}\"");
        let _ = writeln!(s, "\t\t(shape bidirectional)");
        let _ = writeln!(s, "\t\t(at {x} {y} {angle})");
        let _ = writeln!(s, "\t\t(effects (font (size 1.27 1.27)) (justify {justify}))");
        let _ = writeln!(s, "\t\t(uuid \"{uuid}\")");
        s.push_str("\t)\n");
        return s;
    }
    let _ = writeln!(s, "\t(label \"{net}\"");
    let _ = writeln!(s, "\t\t(at {x} {y} {angle})");
    let _ = writeln!(
        s,
        "\t\t(effects (font (size 1.27 1.27)) (justify {justify} bottom))"
    );
    let _ = writeln!(s, "\t\t(uuid \"{uuid}\")");
    s.push_str("\t)\n");
    s
}

/// Render one `(no_connect …)` marker at a pin endpoint.
///
/// The marker carries only its `(at …)` position and a content-derived uuid. Its
/// position must coincide with the pin's connection endpoint (the same point a
/// label would attach to) for KiCAD to associate it with that pin and suppress
/// the unconnected-pin ERC report.
fn render_no_connect(nc: &NoConnect) -> String {
    let x = fmt_coord(nc.at[0]);
    let y = fmt_coord(nc.at[1]);
    let uuid = stable_uuid("no_connect", &nc.uuid_key);

    let mut s = String::new();
    let _ = writeln!(s, "\t(no_connect");
    let _ = writeln!(s, "\t\t(at {x} {y})");
    let _ = writeln!(s, "\t\t(uuid \"{uuid}\")");
    s.push_str("\t)\n");
    s
}

/// Render one placed symbol instance into its `(symbol …)` S-expression block.
///
/// The instance uuid is keyed on the refdes; the property/effects layout and
/// the `(instances (project "" (path "/<root-uuid>" …)))` block match the form
/// proven to load + netlist in the emission spike. The instance path root uuid
/// is the schematic's own `root_uuid` — this is what binds the placement to its
/// reference/unit annotation.
fn render_instance(inst: &Instance, root_uuid: &str) -> String {
    let x = fmt_coord(inst.at[0]);
    let y = fmt_coord(inst.at[1]);
    let angle = inst.angle;
    let lib_id = &inst.lib_id;
    // Free-form, LLM-/user-derived strings must be escaped before embedding.
    let refdes = escape_sexpr_string(&inst.refdes);
    let value = escape_sexpr_string(&inst.value);

    // Reuse the prior instance uuid for a surviving symbol (minimal diff on
    // reconcile); otherwise derive it from the refdes for byte-identical re-emit.
    // A multi-unit part places several instances under one refdes, so units >1
    // take a unit-distinguished key to keep instance uuids unique. Unit 1 keeps
    // the bare-refdes key so single-unit parts stay byte-identical.
    let sym_uuid = inst.uuid.clone().unwrap_or_else(|| {
        if inst.unit <= 1 {
            stable_uuid("symbol", &inst.refdes)
        } else {
            stable_uuid("symbol", &format!("{}#u{}", inst.refdes, inst.unit))
        }
    });
    // Field anchors: solver-assigned when present, else the legacy fixed
    // right-of-body offset (text clear of the glyph via the half-extent).
    let (rp, vp) = field_anchors(inst);
    let (ref_at, ref_j) = (rp.at, rp.justify);
    let (val_at, val_j) = (vp.at, vp.justify);
    let (ref_x, ref_y) = (fmt_coord(ref_at[0]), fmt_coord(ref_at[1]));
    let (val_x, val_y) = (fmt_coord(val_at[0]), fmt_coord(val_at[1]));
    // KiCAD renders a field's text angle RELATIVE to the symbol's rotation,
    // with an auto-flip that already keeps 180-rotated text readable. So a
    // 90/270 symbol needs the inverse angle to render horizontal text, while
    // 0/180 symbols take 0 (compensating 180 with 180 renders upside-down —
    // verified empirically against kicad-cli 10.0.3). The solver models all
    // field text as horizontal, so this keeps geometry and render in sync.
    let field_angle = match inst.angle.rem_euclid(360.0) as i32 {
        90 => 270,
        270 => 90,
        _ => 0,
    };

    // Hide Reference for power/flag symbols whose refdes is `#`-prefixed
    // (KiCAD convention: #PWR…, #FLG…) — they must not appear in the netlist
    // component list or on the visible schematic.
    let hide_ref = inst.refdes.starts_with('#');
    // Hide the Value of PWR_FLAG symbols (keyed on lib_id) — the graphic makes
    // the flag self-evident and the "PWR_FLAG" string would clutter power rail
    // junctions. Also hide solver-suppressed values (`val_hidden`).
    let hide_val = inst.lib_id == "power:PWR_FLAG" || inst.val_hidden;

    let mut s = String::new();
    s.push_str("\t(symbol\n");
    let _ = writeln!(s, "\t\t(lib_id \"{lib_id}\")");
    let _ = writeln!(s, "\t\t(at {x} {y} {angle})");
    // A left-right flip negates local x — that is `(mirror y)` in KiCAD — so the
    // render matches the endpoint transform (`transform_offset` negates x).
    if inst.mirror {
        s.push_str("\t\t(mirror y)\n");
    }
    let _ = writeln!(s, "\t\t(unit {})", inst.unit);
    s.push_str("\t\t(exclude_from_sim no)\n");
    s.push_str("\t\t(in_bom yes)\n");
    s.push_str("\t\t(on_board yes)\n");
    s.push_str("\t\t(dnp no)\n");
    let _ = writeln!(s, "\t\t(uuid \"{sym_uuid}\")");
    let _ = writeln!(s, "\t\t(property \"Reference\" \"{refdes}\"");
    let _ = writeln!(s, "\t\t\t(at {ref_x} {ref_y} {field_angle})");
    if hide_ref {
        let _ = writeln!(
            s,
            "\t\t\t(effects (font (size 1.27 1.27)){} (hide yes))",
            justify_token(ref_j)
        );
    } else {
        let _ = writeln!(
            s,
            "\t\t\t(effects (font (size 1.27 1.27)){})",
            justify_token(ref_j)
        );
    }
    s.push_str("\t\t)\n");
    let _ = writeln!(s, "\t\t(property \"Value\" \"{value}\"");
    let _ = writeln!(s, "\t\t\t(at {val_x} {val_y} {field_angle})");
    if hide_val {
        let _ = writeln!(
            s,
            "\t\t\t(effects (font (size 1.27 1.27)){} (hide yes))",
            justify_token(val_j)
        );
    } else {
        let _ = writeln!(
            s,
            "\t\t\t(effects (font (size 1.27 1.27)){})",
            justify_token(val_j)
        );
    }
    s.push_str("\t\t)\n");
    let _ = writeln!(s, "\t\t(property \"Footprint\" \"\"");
    let _ = writeln!(s, "\t\t\t(at {x} {y} 0)");
    s.push_str("\t\t\t(effects (font (size 1.27 1.27)) (hide yes))\n");
    s.push_str("\t\t)\n");

    // Hidden reconciliation identity tags (`ap_*`), in insertion order. These
    // make the file self-describing: on the next lift/reconcile a synthesized
    // part is matched by `(ap_parent, ap_role, ap_index)` and every part by its
    // block. They are hidden so they never clutter the schematic visually. Keys
    // and values are internally generated (block/role names, refdes, indices),
    // but escaping them is cheap insurance against odd block names.
    for (key, val) in &inst.extra_props {
        let k = escape_sexpr_string(key);
        let v = escape_sexpr_string(val);
        let _ = writeln!(s, "\t\t(property \"{k}\" \"{v}\"");
        let _ = writeln!(s, "\t\t\t(at {x} {y} 0)");
        s.push_str("\t\t\t(effects (font (size 1.27 1.27)) (hide yes))\n");
        s.push_str("\t\t)\n");
    }

    let _ = writeln!(
        s,
        "\t\t(instances\n\t\t\t(project \"\"\n\t\t\t\t(path \"/{root_uuid}\"\n\t\t\t\t\t(reference \"{refdes}\")\n\t\t\t\t\t(unit {unit})\n\t\t\t\t)\n\t\t\t)\n\t\t)",
        unit = inst.unit
    );
    s.push('\n');
    s.push_str("\t)\n");
    s
}

/// Resolved Reference/Value anchors for an instance: the solver's assignment
/// when present, else the legacy fixed right-of-body offsets. The single
/// source of truth shared by `render_instance` and the overlap lint, so the
/// lint always boxes exactly what gets emitted.
fn field_anchors(inst: &Instance) -> (TextPos, TextPos) {
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
/// per `textplace::text_width`, extending per its justification.
fn field_box(at: [f64; 2], j: Justify, width: f64) -> BBox {
    match j {
        Justify::Left => [at[0], at[1] - 1.6, at[0] + width, at[1]],
        Justify::Right => [at[0] - width, at[1] - 1.6, at[0], at[1]],
        Justify::Center => [at[0] - width / 2.0, at[1] - 1.6, at[0] + width / 2.0, at[1]],
    }
}

/// Justify token for a solved field anchor. `Center` omits the token (KiCAD's
/// default field justification is centered).
fn justify_token(j: Justify) -> &'static str {
    match j {
        Justify::Left => " (justify left)",
        Justify::Right => " (justify right)",
        Justify::Center => "",
    }
}

/// An axis-aligned bbox: [min_x, min_y, max_x, max_y].
type BBox = [f64; 4];

/// Whether two axis-aligned boxes overlap (open intervals, so edge-touching is
/// not a collision — symbols flush against a frame don't trip the lint).
fn boxes_overlap(a: &BBox, b: &BBox) -> bool {
    // Tolerance matches `floorplan::rects_overlap`: a shared edge (and the
    // sub-micron float jitter around one) is a TOUCH between padded bboxes — real
    // clearance, not a collision — so it must NOT be flagged. Without this, two
    // collinear/adjacent parts whose padded boxes meet (a divider's R7/R8 spine,
    // a pull-up just above a wide IC) trip a phantom overlap warning.
    const EPS: f64 = 1e-6;
    a[0] < b[2] - EPS && b[0] < a[2] - EPS && a[1] < b[3] - EPS && b[1] < a[3] - EPS
}

impl SchematicWriter {
    /// Deterministic readability lint over everything placed so far: symbol
    /// bodies (from their half-extents) and label text (estimated at 1.1 mm per
    /// character along the label's direction, 1.6 mm tall). Returns one
    /// human-readable warning per overlapping pair. Power symbols (#-prefixed)
    /// and wires are exempt (they legitimately touch the pins they serve).
    ///
    /// The output is sorted, so the same placement always yields the same
    /// warning list regardless of the underlying Vec order.
    pub fn layout_warnings(&self) -> Vec<String> {
        self.layout_warnings_excluding(&std::collections::BTreeSet::new())
    }

    /// Same readability lint as [`Self::layout_warnings`], but suppresses an
    /// overlap warning for any symbol pair whose two owning refdes form an
    /// entry in `ignore_pairs` (stored normalized: sorted so `(A,B) == (B,A)`).
    ///
    /// Used to silence INTENTIONAL same-bank adjacency: bank members (parallel
    /// decouple caps packed at `BANK_PITCH`) sit tight on purpose and share a
    /// bus rather than carrying per-cap labels, so their padded label-clearance
    /// cells overlap even though the real symbol bodies don't collide. The
    /// allowlist is structured (refdes pairs), not string-matched, so only the
    /// specific intentional adjacencies are exempted; any other collision —
    /// bank-vs-anchor, cap-vs-non-cap, label-vs-anything — still warns.
    pub fn layout_warnings_excluding(
        &self,
        ignore_pairs: &std::collections::BTreeSet<(String, String)>,
    ) -> Vec<String> {
        use crate::textplace::{label_box, pin_text_boxes, rotated_half_extents, text_width};

        /// What an item is, for exemption decisions.
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum Kind {
            Body,
            PinText,
            Text,
        }
        // Each item carries its owning refdes so intra-symbol pairs can be
        // exempted where legitimate:
        //   - anything vs its OWN body (a label on its own pin endpoint sits
        //     inside the body's generous bbox; fields hug the body edge);
        //   - own pin text vs own pin text (intra-symbol layout is the
        //     library's business, not ours).
        // Same-owner text-vs-pin-text is NOT exempt — a net label over its own
        // symbol's pin names is exactly the artifact class this lint exists
        // to catch. Label items own the refdes parsed from the
        // `"<refdes>:<pin>:<net>:<idx>"` uuid_key (substring before the first
        // ':'); a power-flag/cluster label without a real refdes prefix simply
        // won't match any symbol's refdes, which is harmless.
        let mut items: Vec<(String, BBox, String, Kind)> = Vec::new();
        for inst in &self.instances {
            if inst.refdes.starts_with('#') {
                // Power/flag graphics are exempt as bodies (they legitimately
                // touch the pins they serve), but their visible Value text
                // (the rail name) must not collide with anything: adjacent
                // rails merging their names is a real artifact class.
                if inst.lib_id != "power:PWR_FLAG" && !inst.val_hidden {
                    let (_, vp) = field_anchors(inst);
                    items.push((
                        format!("value \"{}\" of {}", inst.value, inst.refdes),
                        field_box(vp.at, vp.justify, text_width(&inst.value)),
                        inst.refdes.clone(),
                        Kind::Text,
                    ));
                }
                continue;
            }
            let h = rotated_half_extents(inst.half_extents, inst.angle);
            items.push((
                format!("symbol {}", inst.refdes),
                [
                    inst.at[0] - h[0],
                    inst.at[1] - h[1],
                    inst.at[0] + h[0],
                    inst.at[1] + h[1],
                ],
                inst.refdes.clone(),
                Kind::Body,
            ));
            if let Some(pins) = self.sym_pins.get(&inst.lib_id) {
                for pg in pins {
                    for b in pin_text_boxes(pg, inst.at, inst.angle, inst.mirror) {
                        items.push((
                            format!("pin text of {}", inst.refdes),
                            b,
                            inst.refdes.clone(),
                            Kind::PinText,
                        ));
                    }
                }
            }
            let (rp, vp) = field_anchors(inst);
            items.push((
                format!("field \"{}\"", inst.refdes),
                field_box(rp.at, rp.justify, text_width(&inst.refdes)),
                inst.refdes.clone(),
                Kind::Text,
            ));
            items.push((
                format!("value \"{}\" of {}", inst.value, inst.refdes),
                field_box(vp.at, vp.justify, text_width(&inst.value)),
                inst.refdes.clone(),
                Kind::Text,
            ));
        }
        for label in &self.labels {
            let b = label_box(label.at, label.dir, text_width(&label.net));
            let owner = label
                .uuid_key
                .split(':')
                .next()
                .unwrap_or("")
                .to_string();
            items.push((
                format!("label \"{}\" at {:?}", label.net, label.at),
                b,
                owner,
                Kind::Text,
            ));
        }
        let mut warnings = Vec::new();
        // Wire through an IC body: a wire segment running strictly inside a chip's
        // package box (the pin-tip bbox shrunk past the pin stubs onto the body
        // rectangle — the same geometry the placement cost's `count_ic_body_crossings`
        // prices). This reads as a connection straight through the chip — the defect
        // the eye most often misses — and the soft cost term alone can be OVERRUN (a
        // rigid `layout:` grid forcing a part to the far side of its anchor), so it
        // must LINT too, not just nudge the search.
        const BODY_EPS: f64 = 1e-6;
        const BODY_INSET: f64 = 2.0; // shrink the pin-tip bbox onto the body rectangle
        for inst in &self.instances {
            if inst.refdes.starts_with('#') {
                continue;
            }
            let Some(pins) = self.sym_pins.get(&inst.lib_id).filter(|p| p.len() >= 3) else {
                continue; // power graphics + 2-pin parts have no package box
            };
            // Pin-tip bounding box in sheet coords, inset past the pin stubs onto the
            // body rect — the EXACT geometry the cost's `count_ic_body_crossings` uses.
            let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
            for pg in pins {
                let off = transform_offset(pg.at, inst.angle, inst.mirror);
                let p = [inst.at[0] + off[0], inst.at[1] + off[1]];
                lo[0] = lo[0].min(p[0]);
                lo[1] = lo[1].min(p[1]);
                hi[0] = hi[0].max(p[0]);
                hi[1] = hi[1].max(p[1]);
            }
            let r = [lo[0] + BODY_INSET, lo[1] + BODY_INSET, hi[0] - BODY_INSET, hi[1] - BODY_INSET];
            if r[2] - r[0] < BODY_EPS || r[3] - r[1] < BODY_EPS {
                continue;
            }
            for wire in &self.wires {
                let (w1, w2) = (wire.a, wire.b);
                let cross = if (w1[0] - w2[0]).abs() < BODY_EPS {
                    let x = w1[0];
                    let (ylo, yhi) = (w1[1].min(w2[1]), w1[1].max(w2[1]));
                    r[0] + BODY_EPS < x && x < r[2] - BODY_EPS && ylo.max(r[1]) < yhi.min(r[3]) - BODY_EPS
                } else {
                    let y = w1[1];
                    let (xlo, xhi) = (w1[0].min(w2[0]), w1[0].max(w2[0]));
                    r[1] + BODY_EPS < y && y < r[3] - BODY_EPS && xlo.max(r[0]) < xhi.min(r[2]) - BODY_EPS
                };
                if cross {
                    warnings.push(format!("wire crosses body of {}", inst.refdes));
                    break; // one warning per chip is enough
                }
            }
        }
        for i in 0..items.len() {
            for j in (i + 1)..items.len() {
                let same_owner = items[i].2 == items[j].2;
                let either_body = items[i].3 == Kind::Body || items[j].3 == Kind::Body;
                let both_pin_text =
                    items[i].3 == Kind::PinText && items[j].3 == Kind::PinText;
                // Exempt own-body pairs and intra-symbol pin-text pairs.
                if same_owner && (either_body || both_pin_text) {
                    continue;
                }
                // Skip an intentional same-bank adjacency for EVERY item kind:
                // bank members are packed at BANK_PITCH on purpose, so their
                // bodies, pin text, and fields all interleave by design. The
                // allowlist is structured (sorted refdes pairs), so only those
                // specific adjacencies are exempted.
                let (a, b) = (items[i].2.clone(), items[j].2.clone());
                let pair = if a <= b { (a, b) } else { (b, a) };
                if ignore_pairs.contains(&pair) {
                    continue;
                }
                if boxes_overlap(&items[i].1, &items[j].1) {
                    warnings.push(format!("{} overlaps {}", items[i].0, items[j].0));
                }
            }
        }
        warnings.sort();
        warnings
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `add_symbol` needs a real symbol library to resolve geometry, so these
    /// tests SKIP-gracefully when no KiCAD environment is detected.
    fn detect_env() -> Option<KicadEnv> {
        match KicadEnv::detect() {
            Some(env) => Some(env),
            None => {
                eprintln!("SKIP: no KiCAD environment detected");
                None
            }
        }
    }

    #[test]
    fn escapes_free_form_strings_in_output() {
        let Some(env) = detect_env() else { return };

        // A value containing a double-quote (e.g. inches) must be escaped so the
        // emitted S-expr stays well-formed. LLM-derived values make this real.
        let mut w = SchematicWriter::new();
        w.add_symbol(&env, "Device:R", "R1", "4.7\"", [127.0, 63.5], 0.0)
            .unwrap();
        let text = w.finish();

        // The quote inside the value must be backslash-escaped in the output.
        assert!(
            text.contains("4.7\\\""),
            "value quote must be escaped (expected `4.7\\\"`):\n{text}"
        );

        // And the result must still parse as a valid KiCAD schematic.
        let tmp = tempfile::Builder::new()
            .suffix(".kicad_sch")
            .tempfile()
            .unwrap();
        std::fs::write(tmp.path(), &text).unwrap();
        kiutils_kicad::SchematicFile::read(tmp.path())
            .expect("kiutils must parse output with an escaped value");
    }

    #[test]
    fn escape_sexpr_string_backslash_then_quote() {
        // Backslash is escaped first, then quote — order matters so that an
        // escaped quote's backslash is not itself re-escaped.
        assert_eq!(escape_sexpr_string("a"), "a");
        assert_eq!(escape_sexpr_string("4.7\""), "4.7\\\"");
        assert_eq!(escape_sexpr_string("a\\b"), "a\\\\b");
        // `\"` in the input becomes `\\\"` (backslash escaped, then quote escaped).
        assert_eq!(escape_sexpr_string("\\\""), "\\\\\\\"");
    }

    /// A PinGeom with only the fields the endpoint transform reads.
    fn pin_at(x: f64, y: f64) -> PinGeom {
        PinGeom {
            number: "1".to_string(),
            name: "~".to_string(),
            at: [x, y],
            angle: 0.0,
            length: 1.27,
            unit: 1,
        }
    }

    /// Assert two points are equal within grid-snap floating-point dust.
    fn pt_close(got: [f64; 2], want: [f64; 2]) {
        let eps = 1e-6;
        assert!(
            (got[0] - want[0]).abs() < eps && (got[1] - want[1]).abs() < eps,
            "got {got:?}, want {want:?}"
        );
    }

    #[test]
    fn pin_endpoint_angle0_matches_spike() {
        // Device:R pin 1 local (0, 3.81) at instance (127, 63.5) angle 0:
        // the spike's proven sheet endpoint is (127.0, 59.69) — inst_y - 3.81.
        let p = pin_at(0.0, 3.81);
        pt_close(pin_endpoint(&p, [127.0, 63.5], 0.0, false), [127.0, 59.69]);
        // Pin 2 local (0, -3.81) -> (127.0, 67.31).
        let p2 = pin_at(0.0, -3.81);
        pt_close(pin_endpoint(&p2, [127.0, 63.5], 0.0, false), [127.0, 67.31]);
    }

    #[test]
    fn pin_endpoint_rotations() {
        // Local (0, 3.81). Sheet form is (inst_x + rx, inst_y - ry) where
        // (rx, ry) is the local point rotated CCW by the instance angle.
        let p = pin_at(0.0, 3.81);
        let inst = [127.0, 63.5];
        // 90 deg: (rx, ry) = (-3.81, 0) -> sheet (123.19, 63.5).
        pt_close(pin_endpoint(&p, inst, 90.0, false), [123.19, 63.5]);
        // 180 deg: (rx, ry) = (0, -3.81) -> sheet (127.0, 67.31).
        pt_close(pin_endpoint(&p, inst, 180.0, false), [127.0, 67.31]);
        // 270 deg: (rx, ry) = (3.81, 0) -> sheet (130.81, 63.5).
        pt_close(pin_endpoint(&p, inst, 270.0, false), [130.81, 63.5]);
    }

    #[test]
    fn pin_endpoint_mirror_negates_local_x() {
        // A pin offset in X: mirror negates local x before rotation. At angle 0,
        // local (2.54, 0) mirrors to (-2.54, 0) -> sheet (124.46, 63.5).
        let p = pin_at(2.54, 0.0);
        let inst = [127.0, 63.5];
        pt_close(pin_endpoint(&p, inst, 0.0, false), [129.54, 63.5]);
        pt_close(pin_endpoint(&p, inst, 0.0, true), [124.46, 63.5]);
    }

    #[test]
    fn reemit_is_byte_identical() {
        let Some(env) = detect_env() else { return };

        let build = || {
            let mut w = SchematicWriter::new();
            w.add_symbol(&env, "Device:R", "R1", "1k", [127.0, 63.5], 0.0)
                .unwrap();
            w.add_symbol(&env, "Device:R", "R2", "4.7k", [101.6, 63.5], 90.0)
                .unwrap();
            w.finish()
        };

        assert_eq!(build(), build(), "re-emit must be byte-identical");
    }

    #[test]
    fn label_orientation_per_direction() {
        let mk = |dir| PinLabel {
            net: "X".into(),
            at: [0.0, 0.0],
            uuid_key: "k".into(),
            dir,
            stub: None,
            global: false,
        };
        assert!(render_label(&mk(Dir::East)).contains("(at 0 0 0)"));
        assert!(render_label(&mk(Dir::East)).contains("justify left"));
        assert!(render_label(&mk(Dir::West)).contains("(at 0 0 180)"));
        assert!(render_label(&mk(Dir::West)).contains("justify right"));
        assert!(render_label(&mk(Dir::North)).contains("(at 0 0 90)"));
        assert!(render_label(&mk(Dir::South)).contains("(at 0 0 270)"));
        assert!(render_label(&mk(Dir::North)).contains("justify left"));
        assert!(render_label(&mk(Dir::South)).contains("justify right"));
    }

    #[test]
    fn layout_lint_flags_overlapping_text() {
        let Some(env) = detect_env() else { return };
        let mut w = SchematicWriter::new();
        // Two symbols stacked nearly on top of each other -> collision.
        w.add_symbol(&env, "Device:R", "R1", "1k", [127.0, 63.5], 0.0).unwrap();
        w.add_symbol(&env, "Device:R", "R2", "2k", [127.0, 64.77], 0.0).unwrap();
        let warnings = w.layout_warnings();
        assert!(
            warnings.iter().any(|s| s.contains("R1") && s.contains("R2")),
            "expected an R1/R2 overlap warning, got {warnings:?}"
        );

        // Far apart -> clean.
        let mut w = SchematicWriter::new();
        w.add_symbol(&env, "Device:R", "R1", "1k", [127.0, 63.5], 0.0).unwrap();
        w.add_symbol(&env, "Device:R", "R2", "2k", [177.8, 63.5], 0.0).unwrap();
        assert!(w.layout_warnings().is_empty());
    }

    #[test]
    fn junctions_render_sorted_and_deduped() {
        let mut w = SchematicWriter::new();
        w.add_junction([50.8, 25.4]);
        w.add_junction([25.4, 25.4]);
        w.add_junction([50.8, 25.4]); // duplicate -> dropped
        let sch = w.finish();
        let count = sch.matches("(junction").count();
        assert_eq!(count, 2);
        let first = sch.find("(at 25.4 25.4)").unwrap();
        let second = sch.find("(at 50.8 25.4)").unwrap();
        assert!(first < second, "junctions sorted by uuid_key");
    }

    #[test]
    fn pin_outward_directions_quantize_per_rotation() {
        let Some(env) = detect_env() else { return };
        let mut w = SchematicWriter::new();
        w.add_symbol(&env, "Device:R", "R1", "1k", [127.0, 63.5], 0.0).unwrap();
        w.add_symbol(&env, "Device:R", "R2", "1k", [101.6, 63.5], 90.0).unwrap();
        let d1 = w.pin_dirs(&env, "R1", "1").unwrap();
        // Device:R pin 1 (local (0, 3.81), angle 270) at instance angle 0:
        // endpoint is above the body, outward points up -> North on the sheet.
        assert_eq!(d1[0].1, Dir::North, "R1 pin 1 stub should point North (up)");
        let d2 = w.pin_dirs(&env, "R2", "1").unwrap();
        // At instance angle 90 the same pin rotates to point West.
        assert_eq!(d2[0].1, Dir::West, "R2 pin 1 stub should point West");
    }

    #[test]
    fn lint_flags_text_on_pin_names() {
        let Some(env) = detect_env() else { return };
        let mut w = SchematicWriter::new();
        w.add_symbol(&env, "Timer:NE555P", "U1", "NE555P", [152.4, 101.6], 0.0).unwrap();
        // A fixed cluster label parked on a west-side pin endpoint, reading
        // East: the text runs back across the pin line over the pin name.
        // (This is the legacy retracted-label shape the solver now avoids —
        // the lint must SEE it.)
        let (ep, _dir) = w.pin_dirs(&env, "U1", "2").unwrap()[0];
        w.add_cluster_label("X", ep, Dir::East, false);
        let warnings = w.layout_warnings();
        assert!(
            warnings.iter().any(|s| s.contains("pin text") && s.contains("U1")),
            "expected a pin-text overlap warning, got {warnings:?}"
        );
    }

    #[test]
    fn rotated_symbol_fields_render_horizontal() {
        // KiCAD field angles are relative to the symbol rotation; a 90-degree
        // symbol must carry 270-degree fields so the text reads horizontal.
        let Some(env) = detect_env() else { return };
        let mut w = SchematicWriter::new();
        w.add_symbol(&env, "Device:R", "R1", "1k", [101.6, 101.6], 90.0).unwrap();
        let sch = w.finish();
        let seg = sch.split("(property \"Reference\" \"R1\"").nth(1).unwrap();
        let at_line = seg.lines().nth(1).unwrap();
        assert!(
            at_line.trim_end().ends_with(" 270)"),
            "90-degree symbol fields must compensate to 270, got {at_line:?}"
        );
    }

    #[test]
    fn lint_uses_rotated_body_extents() {
        let Some(env) = detect_env() else { return };
        // Two 90-degree resistors stacked vertically 10.16 apart: with angle-
        // blind extents (half-height 6.35) their boxes overlap; with rotated
        // extents (half-height 5.08) they exactly touch -> no overlap.
        let mut w = SchematicWriter::new();
        w.add_symbol(&env, "Device:R", "R1", "1k", [101.6, 101.6], 90.0).unwrap();
        w.add_symbol(&env, "Device:R", "R2", "2k", [101.6, 111.76], 90.0).unwrap();
        let warnings = w.layout_warnings();
        assert!(
            !warnings.iter().any(|s| s.contains("symbol R1") && s.contains("symbol R2")),
            "rotated bodies must use rotated extents, got {warnings:?}"
        );
    }

    #[test]
    fn solver_is_idempotent_across_finish() {
        let Some(env) = detect_env() else { return };
        let build = |presolve: bool| {
            let mut w = SchematicWriter::new();
            w.add_symbol(&env, "Device:R", "R1", "1k", [101.6, 101.6], 0.0).unwrap();
            w.add_symbol(&env, "Device:R", "R2", "2k", [111.76, 101.6], 0.0).unwrap();
            w.add_signal_label(&env, "R1", "1", "SIG").unwrap();
            if presolve {
                w.retract_colliding_stubs();
                w.solve_text_positions();
            }
            w.finish()
        };
        assert_eq!(build(false), build(true), "pre-solving must not change output");
    }

    #[test]
    fn retracted_label_keeps_outward_direction() {
        // Device:R pin 1 at angle 0 points North; a foreign wire across the
        // stub end forces retraction. The label must land on the pin endpoint
        // KEEPING dir North (angle 90 in the rendered label) so the text still
        // reads away from the body — not reset to East across the pin line.
        let Some(env) = detect_env() else { return };
        let mut w = SchematicWriter::new();
        w.add_symbol(&env, "Device:R", "R1", "1k", [127.0, 63.5], 0.0).unwrap();
        w.add_signal_label(&env, "R1", "1", "SIG").unwrap();
        // Foreign wire through the stub end (127.0, 55.88).
        w.add_wire_on_net([121.92, 55.88], [132.08, 55.88], "OTHER");
        let sch = w.finish();
        assert!(
            sch.contains("(label \"SIG\"\n\t\t(at 127 59.69 90)"),
            "retracted label keeps its North orientation:\n{sch}"
        );
    }

    #[test]
    fn same_net_wire_touch_survives_foreign_retracts() {
        // Device:R pin 1 at (127, 63.5) angle 0:
        //   pin endpoint = (127.0, 59.69)  (inst_y - 3.81)
        //   stub direction = North, STUB_MM = 3.81 -> stub end = (127.0, 55.88)
        //
        // Device:R pin 1 at (177.8, 63.5) angle 0:
        //   pin endpoint = (177.8, 59.69)
        //   stub end = (177.8, 55.88)
        //
        // A horizontal SIG cluster wire running through R1's stub end (127.0, 55.88)
        // is same-net -> R1's stub must survive (label stays at stub end, not pin
        // endpoint). A horizontal OTHER cluster wire running through R2's stub end
        // (177.8, 55.88) is foreign -> R2's stub retracts (label snaps to pin ep).
        let Some(env) = KicadEnv::detect() else {
            eprintln!("SKIP: no KiCAD environment detected");
            return;
        };

        let mut w = SchematicWriter::new();
        w.add_symbol(&env, "Device:R", "R1", "1k", [127.0, 63.5], 0.0).unwrap();
        w.add_symbol(&env, "Device:R", "R2", "1k", [177.8, 63.5], 0.0).unwrap();
        w.add_signal_label(&env, "R1", "1", "SIG").unwrap();
        w.add_signal_label(&env, "R2", "1", "SIG").unwrap();

        // SIG wire spans R1's stub end at y=55.88 -> same-net, stub survives.
        w.add_wire_on_net([121.92, 55.88], [132.08, 55.88], "SIG");
        // OTHER wire spans R2's stub end at y=55.88 -> foreign, stub retracts.
        w.add_wire_on_net([172.72, 55.88], [182.88, 55.88], "OTHER");

        let sch = w.finish();

        // R2's stub retracted: its SIG label must now sit at R2's pin endpoint (177.8, 59.69).
        assert!(
            sch.contains("(label \"SIG\"\n\t\t(at 177.8 59.69"),
            "R2 label must retract to its pin endpoint (177.8, 59.69):\n{sch}"
        );

        // R1's stub survived: its SIG label must NOT sit at R1's pin endpoint (127, 59.69).
        // (It should be at the stub end (127, 55.88) instead.)
        assert!(
            !sch.contains("(label \"SIG\"\n\t\t(at 127 59.69"),
            "R1 label must NOT retract to pin endpoint (127, 59.69) — same-net wire should allow the stub to survive:\n{sch}"
        );
    }
}

#[cfg(test)]
mod repro_tests {
    use super::*;
    use kicad_bridge::env::KicadEnv;

    #[test]
    fn u1_fields_dodge_out_label() {
        let Some(env) = KicadEnv::detect() else { eprintln!("SKIP"); return };
        let mut w = SchematicWriter::new();
        w.add_symbol(&env, "Timer:NE555P", "U1", "", [45.72, 45.72], 0.0).unwrap();
        // KiCad 9's Timer:NE555P names the output pin "Q" (older libs used "OUT").
        w.add_signal_label(&env, "U1", "Q", "N_Q").unwrap();
        let sch = w.finish();
        let seg = sch.split("(property \"Reference\" \"U1\"").nth(1).unwrap();
        let at = seg.lines().nth(1).unwrap();
        println!("U1 ref at: {at}");
        assert!(!at.contains("(at 59.69"), "U1 ref must not sit on the N_Q label:\n{at}");
    }
}
