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
        // Register the lib_symbol body once per lib_id (dedup).
        if !self.lib_symbols.contains_key(lib_id) {
            let geom = SymbolGeometry::load(env, lib_id)?;
            self.lib_symbols
                .insert(lib_id.to_string(), geom.raw_definition);
        }

        self.instances.push(Instance {
            lib_id: lib_id.to_string(),
            refdes: refdes.to_string(),
            value: value.to_string(),
            at: snap_point(at),
            angle,
            mirror: false,
            extra_props: extra_props.to_vec(),
            uuid,
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

    /// Assemble the complete `.kicad_sch` document as a deterministic string.
    ///
    /// `lib_symbols` are emitted sorted by `lib_id` (via the backing
    /// `BTreeMap`); symbol instances are emitted sorted by refdes. All uuids are
    /// content-derived, so the same placements always produce identical bytes.
    pub fn finish(self) -> String {
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

/// Render one net-name label at a pin endpoint into a `(label …)` block.
///
/// The net name is free-form (LLM-/user-derived), so it is escaped before
/// embedding. The label rotation is fixed at 0: a label connects to whatever pin
/// shares its `(at …)` position regardless of label text orientation, so the
/// rotation only affects how the text reads, not connectivity. The uuid is
/// content-derived from the label's stable key for byte-identical re-emission.
fn render_label(label: &PinLabel) -> String {
    let x = fmt_coord(label.at[0]);
    let y = fmt_coord(label.at[1]);
    let net = escape_sexpr_string(&label.net);
    let uuid = stable_uuid("label", &label.uuid_key);

    let mut s = String::new();
    let _ = writeln!(s, "\t(label \"{net}\"");
    let _ = writeln!(s, "\t\t(at {x} {y} 0)");
    s.push_str("\t\t(effects (font (size 1.27 1.27)) (justify left bottom))\n");
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
    // Property text offsets mirror the spike's working layout.
    let ref_x = fmt_coord(x + 2.54);
    let ref_y = fmt_coord(y - 1.27);
    let val_x = fmt_coord(x + 2.54);
    let val_y = fmt_coord(y + 1.27);

    // Hide Reference for power/flag symbols whose refdes is `#`-prefixed
    // (KiCAD convention: #PWR…, #FLG…) — they must not appear in the netlist
    // component list or on the visible schematic.
    let hide_ref = inst.refdes.starts_with('#');
    // Hide the Value text for PWR_FLAG instances — the graphic makes the flag
    // self-evident and the "PWR_FLAG" string would clutter power rail junctions.
    let hide_val = inst.value == "PWR_FLAG";

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
}
