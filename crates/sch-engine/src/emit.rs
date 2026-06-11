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
//! in KiCAD 10 by `crates/kicad-bridge/examples/emit_spike.rs`. The
//! load-bearing details:
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
}

/// One net-name label emitted at a pin's sheet-space connection endpoint.
///
/// A label whose `(at …)` coincides with a pin endpoint binds that pin to the
/// named net; two pins carrying labels with the same net name are joined by
/// KiCAD with no wires (proven in `emit_spike.rs`). The label uuid is derived
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
}

/// A retractable stub wire backing a signal label: the pin endpoint the stub
/// starts at. The stub end is the owning [`PinLabel`]'s `at`. If retracted, the
/// wire is dropped and the label is moved back to `pin_at`.
#[derive(Clone, Copy)]
struct Stub {
    pin_at: [f64; 2],
}

/// One `(wire …)` segment between two grid-snapped sheet points.
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
        });
        Ok(())
    }

    /// Place a net-name label at the connection endpoint of one pin.
    ///
    /// This is the connectivity mechanism: a label whose position coincides with
    /// a pin's sheet-space connection point binds that pin to the named net, and
    /// two pins carrying labels with the *same* net name are joined by KiCAD with
    /// no wires (proven in `crates/kicad-bridge/examples/emit_spike.rs`). Power
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
    ) -> io::Result<()> {
        self.add_symbol(env, "power:PWR_FLAG", refdes, "PWR_FLAG", at, 0.0)
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

    /// Add a junction dot at a wire join. Deduplicated by position.
    pub fn add_junction(&mut self, at: [f64; 2]) {
        let at = snap_point(at);
        let uuid_key = format!("{}:{}", at[0], at[1]);
        if self.junctions.iter().any(|j| j.uuid_key == uuid_key) {
            return;
        }
        self.junctions.push(Junction { at, uuid_key });
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
        let (inst_at, inst_angle, inst_mirror) = (inst.at, inst.angle, inst.mirror);
        let geom = SymbolGeometry::load(env, &inst.lib_id)?;

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
                format!("no pin {pin:?} on {}", inst.lib_id),
            ));
        }
        Ok(matches
            .into_iter()
            .map(|pg| {
                let ep = pin_endpoint(pg, inst_at, inst_angle, inst_mirror);
                // Outward direction in symbol space: angle+180 from the pin line.
                // The pin `angle` in the symbol file points from the connection
                // tip INTO the body; outward (away from body) is angle+180.
                let theta = (pg.angle + 180.0).to_radians();
                let (mut dx, dy) = (theta.cos(), theta.sin());
                if inst_mirror {
                    dx = -dx;
                }
                let phi = inst_angle.to_radians();
                let (s, c) = phi.sin_cos();
                let rx = dx * c - dy * s;
                let ry = dx * s + dy * c;
                // Sheet flip: sheet-space y component is -ry (symbol Y up, sheet Y down).
                let sy = -ry;
                let dir = if rx.abs() >= sy.abs() {
                    if rx >= 0.0 { Dir::East } else { Dir::West }
                } else if sy >= 0.0 {
                    Dir::South
                } else {
                    Dir::North
                };
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
    /// `None` on any that retract; a *surviving* stub keeps its `Some(..)` but its
    /// emitted wire is `add_wire`-deduped and its endpoint/segment already occupy
    /// the foreign sets, so re-running collides with nothing new. A second call is
    /// therefore a no-op. This lets a caller run it early (e.g. to lint the
    /// post-retraction geometry) and have `finish` run it again harmlessly.
    ///
    /// **Foreign geometry** at pass start = every *fixed* connection point (power
    /// symbol pins — origin, net = the Value; no-connect markers — a reserved
    /// sentinel net; legacy labels; and every signal stub's own pin endpoint,
    /// always safe) plus every existing wire **segment** (all wires present here
    /// are power stubs/risers — a signal net never coincides with a power net, so
    /// any touch is foreign).
    ///
    /// Signal stubs are then walked in deterministic `uuid_key` order. A stub is
    /// **retracted** — its label snapped back onto its always-safe pin endpoint
    /// (orientation reset to `East`), no wire emitted — when its end coincides
    /// with a foreign point, its end lies on a foreign segment, or its segment
    /// passes through a foreign point. A *surviving* stub registers its endpoint
    /// and segment as occupancy so a later differing-net stub cannot then collide
    /// with it. The pin-endpoint fallback reproduces the proven pre-stub
    /// connectivity, so retraction only ever removes an accidental merge.
    pub(crate) fn retract_colliding_stubs(&mut self) {
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
                self.labels[i].at = pin_at;
                self.labels[i].dir = Dir::East;
                self.labels[i].stub = None;
            } else {
                self.add_wire(pin_at, end);
                add_point(end, &net, &mut points);
                segments.push((pin_at, end, net));
            }
        }
    }

    /// Assemble the complete `.kicad_sch` document as a deterministic string.
    ///
    /// `lib_symbols` are emitted sorted by `lib_id` (via the backing
    /// `BTreeMap`); symbol instances are emitted sorted by refdes. All uuids are
    /// content-derived, so the same placements always produce identical bytes.
    pub fn finish(mut self) -> String {
        // Resolve signal-stub collisions and materialize the surviving stub wires
        // before any rendering, so labels/wires below render the reconciled state.
        self.retract_colliding_stubs();

        let root_uuid = stable_uuid("sheet", ROOT_SHEET_KEY);

        let mut out = String::new();
        out.push_str("(kicad_sch\n");
        out.push_str("\t(version 20250114)\n");
        out.push_str("\t(generator \"auto-pcb\")\n");
        out.push_str("\t(generator_version \"0.1\")\n");
        let _ = writeln!(out, "\t(uuid \"{root_uuid}\")");
        out.push_str("\t(paper \"A4\")\n");

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
fn fmt_coord(v: f64) -> f64 {
    if v == 0.0 { 0.0 } else { v }
}

/// Compute the sheet-space connection endpoint of a pin on a placed instance.
///
/// ## What "connection endpoint" means
///
/// In a `.kicad_sym`, a pin's `(at x y angle)` is the pin's **connection point**
/// — the tip where wires/labels attach — and the pin line extends `length` mm
/// *into the symbol body* along `angle`. So the connection point is exactly the
/// pin's local `at`; no `length` projection is applied (projecting by `length`
/// would land inside the body, off the connection). This matches the proven
/// `emit_spike.rs`, where Device:R pin 1 at local `(0, 3.81)` maps to sheet
/// `(inst_x, inst_y - 3.81)`.
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
fn pin_endpoint(pin: &PinGeom, inst_at: [f64; 2], inst_angle: f64, mirror: bool) -> [f64; 2] {
    let (mut lx, ly) = (pin.at[0], pin.at[1]);
    if mirror {
        lx = -lx;
    }

    let theta = inst_angle.to_radians();
    let (s, c) = theta.sin_cos();
    let rx = lx * c - ly * s;
    let ry = lx * s + ly * c;

    let sheet = [inst_at[0] + rx, inst_at[1] - ry];
    snap_point(sheet)
}

/// Whether point `p` lies on the axis-aligned segment `a`–`b` (endpoints
/// included), within grid-snap floating-point dust.
///
/// All stub/power wires are horizontal or vertical, so the test reduces to: `p`
/// is collinear with the segment's constant axis and within its varying-axis
/// span. Endpoints count as "on" — a stub end meeting a foreign wire's endpoint
/// is just as much a connection as meeting its middle.
fn point_on_segment(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> bool {
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
    let sym_uuid = inst
        .uuid
        .clone()
        .unwrap_or_else(|| stable_uuid("symbol", &inst.refdes));
    // Push the Reference/Value field text clear of the symbol body using the
    // instance's per-symbol half-extent, so the text never overlaps the glyph.
    let ref_x = fmt_coord(x + inst.half_extents[0] + 1.27);
    let ref_y = fmt_coord(y - 1.27);
    let val_x = fmt_coord(x + inst.half_extents[0] + 1.27);
    let val_y = fmt_coord(y + 1.27);

    // Hide Reference for power/flag symbols whose refdes is `#`-prefixed
    // (KiCAD convention: #PWR…, #FLG…) — they must not appear in the netlist
    // component list or on the visible schematic.
    let hide_ref = inst.refdes.starts_with('#');
    // Hide the Value of PWR_FLAG symbols (keyed on lib_id) — the graphic makes
    // the flag self-evident and the "PWR_FLAG" string would clutter power rail
    // junctions.
    let hide_val = inst.lib_id == "power:PWR_FLAG";

    let mut s = String::new();
    s.push_str("\t(symbol\n");
    let _ = writeln!(s, "\t\t(lib_id \"{lib_id}\")");
    let _ = writeln!(s, "\t\t(at {x} {y} {angle})");
    s.push_str("\t\t(unit 1)\n");
    s.push_str("\t\t(exclude_from_sim no)\n");
    s.push_str("\t\t(in_bom yes)\n");
    s.push_str("\t\t(on_board yes)\n");
    s.push_str("\t\t(dnp no)\n");
    let _ = writeln!(s, "\t\t(uuid \"{sym_uuid}\")");
    let _ = writeln!(s, "\t\t(property \"Reference\" \"{refdes}\"");
    let _ = writeln!(s, "\t\t\t(at {ref_x} {ref_y} 0)");
    if hide_ref {
        s.push_str("\t\t\t(effects (font (size 1.27 1.27)) (justify left) (hide yes))\n");
    } else {
        s.push_str("\t\t\t(effects (font (size 1.27 1.27)) (justify left))\n");
    }
    s.push_str("\t\t)\n");
    let _ = writeln!(s, "\t\t(property \"Value\" \"{value}\"");
    let _ = writeln!(s, "\t\t\t(at {val_x} {val_y} 0)");
    if hide_val {
        s.push_str("\t\t\t(effects (font (size 1.27 1.27)) (justify left) (hide yes))\n");
    } else {
        s.push_str("\t\t\t(effects (font (size 1.27 1.27)) (justify left))\n");
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
        "\t\t(instances\n\t\t\t(project \"\"\n\t\t\t\t(path \"/{root_uuid}\"\n\t\t\t\t\t(reference \"{refdes}\")\n\t\t\t\t\t(unit 1)\n\t\t\t\t)\n\t\t\t)\n\t\t)"
    );
    s.push('\n');
    s.push_str("\t)\n");
    s
}

/// An axis-aligned bbox: [min_x, min_y, max_x, max_y].
type BBox = [f64; 4];

/// Whether two axis-aligned boxes overlap (open intervals, so edge-touching is
/// not a collision — symbols flush against a frame don't trip the lint).
fn boxes_overlap(a: &BBox, b: &BBox) -> bool {
    a[0] < b[2] && b[0] < a[2] && a[1] < b[3] && b[1] < a[3]
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
        // Each item carries its owning refdes so a label is never flagged against
        // the symbol body it belongs to (its stub emerges from that body, and
        // post-retraction it sits right on that symbol's pin — both legitimate).
        // Symbol items own themselves; label items own the refdes parsed from the
        // `"<refdes>:<pin>:<net>:<idx>"` uuid_key (substring before the first ':').
        // A power-flag/legacy label without a real refdes prefix simply won't
        // match any symbol's refdes, which is harmless.
        let mut items: Vec<(String, BBox, String)> = Vec::new();
        for inst in &self.instances {
            if inst.refdes.starts_with('#') {
                continue;
            }
            let h = inst.half_extents;
            items.push((
                format!("symbol {}", inst.refdes),
                [
                    inst.at[0] - h[0],
                    inst.at[1] - h[1],
                    inst.at[0] + h[0],
                    inst.at[1] + h[1],
                ],
                inst.refdes.clone(),
            ));
        }
        for label in &self.labels {
            let len = label.net.chars().count() as f64 * 1.1;
            let b = match label.dir {
                Dir::East => [label.at[0], label.at[1] - 1.6, label.at[0] + len, label.at[1]],
                Dir::West => [label.at[0] - len, label.at[1] - 1.6, label.at[0], label.at[1]],
                Dir::North => [label.at[0] - 1.6, label.at[1] - len, label.at[0], label.at[1]],
                Dir::South => [label.at[0], label.at[1], label.at[0] + 1.6, label.at[1] + len],
            };
            let owner = label
                .uuid_key
                .split(':')
                .next()
                .unwrap_or("")
                .to_string();
            items.push((format!("label \"{}\" at {:?}", label.net, label.at), b, owner));
        }
        let mut warnings = Vec::new();
        for i in 0..items.len() {
            for j in (i + 1)..items.len() {
                // Exempt a label from its OWN symbol's body: skip the pair when one
                // item's owning refdes equals the other's. Distinct refdes (e.g.
                // R1's label over R2) and label-vs-label / symbol-vs-symbol are
                // unaffected (their owners differ).
                if items[i].2 == items[j].2 {
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
