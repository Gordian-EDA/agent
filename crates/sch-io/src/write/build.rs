//! Building the schematic document: placing symbols, labels, wires, junctions,
//! and free graphics, plus the pin-endpoint geometry the connectivity helpers
//! resolve against and the refinement-scorer accessors over the placed scene.

use std::io;

use geom::{GRID_50_MIL, Point2, Rect, Segment};
use kicad::KicadInstallation;
use kicad_symbol::geometry::{PinGeom, SymbolGeometry};

use crate::wire::{DrawnSegment, NetSegment};

use super::{
    Dir, Instance, Junction, NoConnect, PinLabel, SchematicWriter, SheetRect, SheetText, Stub, Wire,
};

impl SchematicWriter {
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
        env: &KicadInstallation,
        lib_id: &str,
        refdes: &str,
        value: &str,
        at: impl Into<Point2>,
        angle: f64,
    ) -> io::Result<()> {
        self.add_symbol_full(
            env,
            lib_id,
            refdes,
            value,
            at.into(),
            angle,
            None,
            &[],
            None,
        )
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
        env: &KicadInstallation,
        lib_id: &str,
        refdes: &str,
        value: &str,
        at: impl Into<Point2>,
        angle: f64,
        footprint: Option<&str>,
        extra_props: &[(String, String)],
        uuid: Option<String>,
    ) -> io::Result<()> {
        let at = at.into();
        // Register the lib_symbol body once per lib_id (dedup). The same branch
        // caches the symbol's approximate size by lib_id so field placement need
        // not reload geometry per instance.
        if !self.lib_symbols.contains_key(lib_id) {
            let geom = SymbolGeometry::load(env.symbol_dir(), lib_id)?;
            self.sym_sizes
                .insert(lib_id.to_string(), geom.approx_size());
            self.sym_pins.insert(lib_id.to_string(), geom.pins.clone());
            self.lib_symbols
                .insert(lib_id.to_string(), geom.raw_definition);
        }

        // Cached above on the first instance of this lib_id; reused for the rest.
        let size = self
            .sym_sizes
            .get(lib_id)
            .copied()
            .unwrap_or(Point2::new(0.0, 0.0));
        let half_extents = Point2::new(size.x / 2.0, size.y / 2.0);

        self.instances.push(Instance {
            lib_id: lib_id.to_string(),
            refdes: refdes.to_string(),
            value: value.to_string(),
            footprint: footprint.map(str::to_string),
            at: GRID_50_MIL.snap_point(at),
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
        env: &KicadInstallation,
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
                // Direct/no-stub path: East -> angle 0, justify left bottom,
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
        env: &KicadInstallation,
        refdes: &str,
        pin: &str,
        net: &str,
    ) -> io::Result<()> {
        self.add_signal_label_stub(env, refdes, pin, net, 3.81)
    }

    /// [`add_signal_label`] with a caller-chosen stub length — the router
    /// extends the stub past whatever body the default landing would cover.
    pub fn add_signal_label_stub(
        &mut self,
        env: &KicadInstallation,
        refdes: &str,
        pin: &str,
        net: &str,
        stub_mm: f64,
    ) -> io::Result<()> {
        for (idx, (ep, dir)) in self.pin_dirs(env, refdes, pin)?.into_iter().enumerate() {
            let ep = GRID_50_MIL.snap_point(ep);
            let v = dir.vec();
            let end =
                GRID_50_MIL.snap_point(Point2::new(ep.x + v.x * stub_mm, ep.y + v.y * stub_mm));
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
        env: &KicadInstallation,
        lib_id: &str,
        refdes: &str,
        net: &str,
        at: impl Into<Point2>,
        angle: f64,
    ) -> io::Result<()> {
        debug_assert!(
            refdes.starts_with('#'),
            "power symbol refdes must be #-prefixed, got {refdes:?}"
        );
        self.add_symbol(env, lib_id, refdes, net, at.into(), angle)
    }

    /// Place a `PWR_FLAG` whose pin is **pin-coincident** with `at`.
    ///
    /// Power nets are joined by global power ports, and a *local* label does
    /// not merge with a global net — so the flag attaches by position, not by
    /// label: its pin (at the symbol origin) lands exactly on an existing
    /// power-port connection point.
    pub fn add_power_flag_at(
        &mut self,
        env: &KicadInstallation,
        refdes: &str,
        at: impl Into<Point2>,
        angle: f64,
    ) -> io::Result<()> {
        self.add_symbol(env, "power:PWR_FLAG", refdes, "PWR_FLAG", at.into(), angle)
    }

    /// Add a wire segment between two sheet points (snapped).
    ///
    /// If the two points are identical after snapping, the segment is silently
    /// dropped (a zero-length wire would clutter the schematic with no benefit).
    /// The `uuid_key` is content-derived so repeated calls with the same
    /// endpoints produce one deterministic wire.
    pub fn add_wire(&mut self, a: impl Into<Point2>, b: impl Into<Point2>) {
        self.push_wire(a.into(), b.into(), None);
    }

    /// Add a wire that belongs to a known net (cluster geometry). Same-net
    /// touches against it are deliberate joins, not collisions.
    pub fn add_wire_on_net(&mut self, a: impl Into<Point2>, b: impl Into<Point2>, net: &str) {
        self.push_wire(a.into(), b.into(), Some(net.to_string()));
    }

    fn push_wire(&mut self, a: Point2, b: Point2, net: Option<String>) {
        let a = GRID_50_MIL.snap_point(a);
        let b = GRID_50_MIL.snap_point(b);
        if a == b {
            return;
        }
        let uuid_key = format!("{}:{}:{}:{}", a.x, a.y, b.x, b.y);
        if self.wires.iter().any(|w| w.uuid_key == uuid_key) {
            return;
        }
        self.wires.push(Wire {
            a,
            b,
            uuid_key,
            net,
        });
    }

    /// Place a cluster net label at `at`, oriented `dir`.
    ///
    /// Thin entry point for cluster decoration: a cluster emits exactly one
    /// label per externally-visible net at the net's tap point, so connectivity
    /// joins to the rest of the sheet without per-pin label spam. The label is
    /// keyed on `cluster:{net}:{x}:{y}` (position-derived) and carries no stub —
    /// it sits directly on the cluster wire it labels.
    pub fn add_cluster_label(&mut self, net: &str, at: impl Into<Point2>, dir: Dir, global: bool) {
        let at = GRID_50_MIL.snap_point(at);
        self.labels.push(PinLabel {
            net: net.to_string(),
            at,
            uuid_key: format!("cluster:{net}:{}:{}", at.x, at.y),
            dir,
            stub: None,
            global,
        });
    }

    /// Add a junction dot at a wire join. Deduplicated by position.
    pub fn add_junction(&mut self, at: impl Into<Point2>) {
        let at = GRID_50_MIL.snap_point(at.into());
        let uuid_key = format!("{}:{}", at.x, at.y);
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
    pub fn add_text(
        &mut self,
        text: &str,
        at: impl Into<Point2>,
        size: f64,
        bold: bool,
        key: &str,
    ) {
        self.texts.push(SheetText {
            text: text.to_string(),
            at: GRID_50_MIL.snap_point(at.into()),
            size,
            bold,
            uuid_key: key.to_string(),
        });
    }

    /// Add a graphic rectangle (no fill, dashed) to the sheet.
    pub fn add_rect(&mut self, start: impl Into<Point2>, end: impl Into<Point2>, key: &str) {
        self.rects.push(SheetRect {
            start: GRID_50_MIL.snap_point(start.into()),
            end: GRID_50_MIL.snap_point(end.into()),
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
        env: &KicadInstallation,
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
            None => SymbolGeometry::load(env.symbol_dir(), &lib_id)?.pins,
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
    pub fn add_no_connect(
        &mut self,
        env: &KicadInstallation,
        refdes: &str,
        pin: &str,
    ) -> io::Result<()> {
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
        env: &KicadInstallation,
        net: &str,
        refdes: &str,
        at: [f64; 2],
    ) -> io::Result<()> {
        let at = GRID_50_MIL.snap_point(at);
        self.add_symbol(env, "power:PWR_FLAG", refdes, "PWR_FLAG", at, 0.0)?;
        // A PWR_FLAG's only pin is exactly at the instance origin. Attach the
        // label directly to this instance rather than resolving by refdes: two
        // independently built groups (or a malformed caller) may temporarily
        // reuse the same hidden ref before composition namespaces it.
        self.labels.push(PinLabel {
            net: net.to_owned(),
            at,
            uuid_key: format!("{refdes}:1:{net}:0"),
            dir: Dir::East,
            stub: None,
            global: false,
        });
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
    fn pin_endpoints(
        &self,
        env: &KicadInstallation,
        refdes: &str,
        pin: &str,
    ) -> io::Result<Vec<Point2>> {
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

        let geom = SymbolGeometry::load(env.symbol_dir(), &any.lib_id)?;

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
                format!("no pin {pin:?} (by number or name) on {}", any.lib_id),
            ));
        }

        // Each matched pin lives at the instance that draws ITS unit — a multi-unit
        // part places one instance per unit, all sharing `refdes`. Resolving every pin
        // through `any` (unit 1) drops a unit-2 pin's label/no-connect onto unit 1's
        // body at the same geom offset (an LM358 unit-2 OUT lands on unit-1 OUT — the
        // misplaced no_connect over a connected feedback pin). Fall back to `any` for an
        // unplaced unit / single-unit part (byte-identical there). Mirrors `pin_dirs`.
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
                Point2::from(pin_endpoint(pg, inst.at, inst.angle, inst.mirror))
            })
            .collect())
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
    pub fn route_scene(&self) -> crate::wire::RouteScene {
        const NC: &str = "\0no_connect";
        const PWR: &str = "\0power_wire";
        let mut scene = crate::wire::RouteScene {
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
            let h = inst.half_extents.rotated_half_extents(inst.angle);
            let (hx, hy) = ((h[0] - 2.54).max(1.27), (h[1] - 2.54).max(1.27));
            scene.solids.push(Rect::new(
                inst.at[0] - hx,
                inst.at[1] - hy,
                inst.at[0] + hx,
                inst.at[1] + hy,
            ));
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
                Rect::new(
                    nc.at[0] - NC_KEEPOUT,
                    nc.at[1] - NC_KEEPOUT,
                    nc.at[0] + NC_KEEPOUT,
                    nc.at[1] + NC_KEEPOUT,
                ),
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
            scene.segments.push(NetSegment::new(w.a, w.b, net));
        }
        scene
    }

    /// Wire segments attributed to `net` (for junction counting at taps).
    pub fn wire_segments_on_net(&self, net: &str) -> Vec<Segment> {
        self.wires
            .iter()
            .filter(|w| w.net.as_deref() == Some(net))
            .map(|w| Segment::new(w.a, w.b))
            .collect()
    }

    /// Junction-dot count (a routing-quality signal for the refinement scorer).
    pub fn junction_count(&self) -> usize {
        self.junctions.len()
    }

    /// Junction-dot positions (for the scorer's merge check: a junction sitting
    /// on wires of two different nets fuses them).
    pub fn junction_positions(&self) -> Vec<[f64; 2]> {
        self.junctions.iter().map(|j| j.at.into()).collect()
    }

    /// Count of plain (non-global) labels — i.e. signal-label fallbacks where the
    /// router could not wire a net. Port pentagons are `global` and excluded, so
    /// this is a direct "how many nets degraded to labels" signal.
    pub fn signal_label_count(&self) -> usize {
        self.labels.iter().filter(|l| !l.global).count()
    }

    /// Bounding boxes of the global/port labels (the edge pentagons), for the
    /// refinement scorer to keep symbol bodies from colliding with a port label
    /// (the label is placed during routing, so it is not an `Item`).
    pub fn cluster_label_boxes(&self) -> Vec<Rect> {
        self.labels
            .iter()
            .filter(|l| l.global)
            .map(|l| {
                let w = crate::label::text_width(&l.net) + 2.54;
                Rect::new(l.at[0] - w, l.at[1] - 2.0, l.at[0] + w, l.at[1] + 2.0)
            })
            .collect()
    }

    /// Every drawn wire segment with its net (`None` for unattributed power
    /// stubs). For the refinement scorer's crossing / length / short metrics.
    pub fn wires_with_nets(&self) -> Vec<DrawnSegment> {
        self.wires
            .iter()
            .map(|w| DrawnSegment::new(w.a, w.b, w.net.clone()))
            .collect()
    }

    /// Absorb every drawn element of `other` into `self` (lib_symbols merged by
    /// `lib_id`, all geometry moved verbatim). The multi-block composer translates
    /// each group writer to its tile, then folds them all into one writer for a
    /// single `finish` — no per-sheet string splicing. `other`'s items already
    /// carry distinct refdes / content-derived uuid_keys, so no key collides.
    pub fn absorb(&mut self, other: SchematicWriter) {
        for (id, body) in other.lib_symbols {
            self.lib_symbols.entry(id.clone()).or_insert(body);
        }
        for (id, sz) in other.sym_sizes {
            self.sym_sizes.entry(id).or_insert(sz);
        }
        for (id, pins) in other.sym_pins {
            self.sym_pins.entry(id).or_insert(pins);
        }
        self.instances.extend(other.instances);
        self.labels.extend(other.labels);
        self.no_connects.extend(other.no_connects);
        self.wires.extend(other.wires);
        self.junctions.extend(other.junctions);
        self.texts.extend(other.texts);
        self.rects.extend(other.rects);
        self.fields_above.extend(other.fields_above);
    }

    /// Namespace generated hidden references before composing independent
    /// writers. Authored references are globally unique, but each group emits
    /// its own `#PWR_*`/`#FLG_*` identifiers; without a group namespace their
    /// symbol UUIDs collide in the combined document. Namespace/reference text
    /// is reduced to a KiCad-safe ASCII token, and repeated hidden references
    /// get deterministic numeric suffixes.
    pub fn namespace_hidden_references(&mut self, namespace: &str) {
        let token = |value: &str, fallback: &str| {
            let mut token: String = value
                .chars()
                .map(|ch| {
                    if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-') {
                        ch
                    } else {
                        '_'
                    }
                })
                .collect();
            if token.is_empty() || token.chars().all(|ch| ch == '_') {
                token = fallback.to_owned();
            }
            token
        };
        let namespace = token(namespace, "group");
        let mut renamed: std::collections::BTreeMap<String, Vec<(String, Point2)>> =
            std::collections::BTreeMap::new();
        let mut used = std::collections::HashSet::new();
        for inst in &mut self.instances {
            if !inst.refdes.starts_with('#') {
                continue;
            }
            let old = inst.refdes.clone();
            let hidden = token(old.trim_start_matches('#'), "hidden");
            let base = format!("#{namespace}_{hidden}");
            let mut new = base.clone();
            let mut suffix = 2usize;
            while !used.insert(new.clone()) {
                new = format!("{base}_{suffix}");
                suffix += 1;
            }
            inst.refdes = new.clone();
            renamed.entry(old).or_default().push((new, inst.at));
        }
        if renamed.is_empty() {
            return;
        }
        for label in &mut self.labels {
            if let Some((old, instances)) = renamed
                .iter()
                .find(|(old, _)| label.uuid_key.starts_with(&format!("{old}:")))
            {
                let new = instances
                    .iter()
                    .min_by(|(_, a), (_, b)| a.dist(label.at).total_cmp(&b.dist(label.at)))
                    .map(|(new, _)| new)
                    .expect("renamed hidden ref has at least one instance");
                label.uuid_key = format!("{new}:{}", &label.uuid_key[old.len() + 1..]);
            }
        }
        for nc in &mut self.no_connects {
            if let Some((old, instances)) = renamed
                .iter()
                .find(|(old, _)| nc.uuid_key.starts_with(&format!("{old}:")))
            {
                let new = instances
                    .iter()
                    .min_by(|(_, a), (_, b)| a.dist(nc.at).total_cmp(&b.dist(nc.at)))
                    .map(|(new, _)| new)
                    .expect("renamed hidden ref has at least one instance");
                nc.uuid_key = format!("{new}:{}", &nc.uuid_key[old.len() + 1..]);
            }
        }
        self.fields_above = self
            .fields_above
            .iter()
            .flat_map(|refdes| {
                renamed.get(refdes).map_or_else(
                    || vec![refdes.clone()],
                    |instances| instances.iter().map(|(new, _)| new.clone()).collect(),
                )
            })
            .collect();
    }

    /// The PWR_FLAG instances in this writer, as `(net, index)` pairs. The
    /// attached pin label is authoritative, with legacy `#FLG_<net>` references
    /// as a fallback for coincident flags that need no label. Used by the
    /// composer to dedup flags across groups; index lets it drop a duplicate.
    pub fn pwr_flag_nets(&self) -> Vec<(String, usize)> {
        self.instances
            .iter()
            .enumerate()
            .filter_map(|(i, inst)| {
                if inst.lib_id != "power:PWR_FLAG" {
                    return None;
                }
                let label_prefix = format!("{}:", inst.refdes);
                self.labels
                    .iter()
                    .find(|label| label.uuid_key.starts_with(&label_prefix) && label.at == inst.at)
                    .or_else(|| {
                        self.labels
                            .iter()
                            .find(|label| label.uuid_key.starts_with(&label_prefix))
                    })
                    .map(|label| (label.net.clone(), i))
                    .or_else(|| {
                        inst.refdes
                            .strip_prefix("#FLG_")
                            .map(|net| (net.to_owned(), i))
                    })
            })
            .collect()
    }

    /// Every net NAME referenced by a placed element on this writer (signal/port
    /// labels + power-symbol values), used to decide whether a flagged net is
    /// already DRIVEN elsewhere. A power symbol stores its rail in `value`.
    pub fn referenced_nets(&self) -> std::collections::HashSet<String> {
        let mut nets: std::collections::HashSet<String> =
            self.labels.iter().map(|l| l.net.clone()).collect();
        for inst in &self.instances {
            if inst.refdes.starts_with("#PWR") {
                nets.insert(inst.value.clone());
            }
        }
        nets
    }

    /// Remove the instances (and their pin label) at the given instance indices —
    /// the composer's flag-dedup drop. Indices are removed high-to-low so earlier
    /// ones stay valid.
    pub fn remove_instances(&mut self, mut idx: Vec<usize>) {
        idx.sort_unstable();
        idx.dedup();
        for &i in idx.iter().rev() {
            let inst = self.instances.remove(i);
            // Drop any pin label that drove this flag (keyed to its refdes' pin).
            let tag = format!("{}:", inst.refdes);
            if inst.lib_id == "power:PWR_FLAG"
                && let Some(label_idx) = self
                    .labels
                    .iter()
                    .position(|label| label.uuid_key.starts_with(&tag) && label.at == inst.at)
            {
                self.labels.remove(label_idx);
            } else if !self
                .instances
                .iter()
                .any(|remaining| remaining.refdes == inst.refdes)
            {
                self.labels.retain(|l| !l.uuid_key.starts_with(&tag));
            }
        }
    }

    /// In a composed multi-block sheet, a cross-block net is represented by global
    /// port labels. If the local router also fell back to plain labels on that
    /// same net, KiCAD warns that local and global labels share a name. Promote the
    /// local fallbacks so the net has one label scope.
    pub fn promote_local_labels_for_global_nets(&mut self) -> usize {
        let global_nets: std::collections::HashSet<String> = self
            .labels
            .iter()
            .filter(|label| label.global)
            .map(|label| label.net.clone())
            .collect();
        let mut promoted = 0usize;
        for label in &mut self.labels {
            if !label.global && global_nets.contains(&label.net) {
                label.global = true;
                promoted += 1;
            }
        }
        promoted
    }

    /// Rigidly shift EVERY drawn element (instances + their solved field text,
    /// wires, labels + stubs, junctions, no-connects, free text, rects) by
    /// `(dx, dy)`. The typed sibling of `reframe`'s shift block: connectivity is
    /// preserved (everything moves together), so the multi-block composer can
    /// translate a fully-laid-out group writer to its tile in mm — no string
    /// geometry math. Caller keeps the shift grid-aligned to stay on the KiCAD grid.
    pub fn translate(&mut self, dx: f64, dy: f64) {
        let sh = |p: &mut Point2| {
            *p = GRID_50_MIL.snap_point(Point2::new(p.x + dx, p.y + dy));
        };
        let sha = |p: &mut [f64; 2]| {
            *p = GRID_50_MIL
                .snap_point(Point2::new(p[0] + dx, p[1] + dy))
                .into();
        };
        for i in &mut self.instances {
            sh(&mut i.at);
            if let Some(p) = &mut i.ref_pos {
                sha(&mut p.at);
            }
            if let Some(p) = &mut i.val_pos {
                sha(&mut p.at);
            }
        }
        for w in &mut self.wires {
            sh(&mut w.a);
            sh(&mut w.b);
            w.uuid_key = format!("{}:{}:{}:{}", w.a.x, w.a.y, w.b.x, w.b.y);
        }
        for l in &mut self.labels {
            sh(&mut l.at);
            if let Some(s) = &mut l.stub {
                sh(&mut s.pin_at);
            }
            if l.uuid_key.starts_with("cluster:") {
                l.uuid_key = format!("cluster:{}:{}:{}", l.net, l.at.x, l.at.y);
            }
        }
        for j in &mut self.junctions {
            sh(&mut j.at);
            j.uuid_key = format!("{}:{}", j.at.x, j.at.y);
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
}

/// Angle-0 sheet-space pin-end offsets (relative to the symbol origin) of a
/// pin, resolved against `lib_id`'s geometry by number first then name.
///
/// The single source of truth for "where does this pin land at instance angle
/// 0" — used by cluster geometry's pin callback (which takes the first end) and
/// by anchor-pin slotting (offset + [`quantize_dir`]). A pin *name* can match
/// several physical pins, so a `Vec` is returned. The offset is
/// `pin.at.transform_offset(0.0, false)`, i.e. `[pin.x, -pin.y]`.
pub fn pin_end0(env: &KicadInstallation, lib_id: &str, pin: &str) -> io::Result<Vec<[f64; 2]>> {
    let geom = SymbolGeometry::load(env.symbol_dir(), lib_id)?;
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
        .map(|pg| <[f64; 2]>::from(pg.at.transform_offset(0.0, false)))
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
pub fn pin_endpoint(
    pin: &PinGeom,
    inst_at: impl Into<Point2>,
    inst_angle: f64,
    mirror: bool,
) -> [f64; 2] {
    let inst_at = inst_at.into();
    let off = pin.at.transform_offset(inst_angle, mirror);
    GRID_50_MIL
        .snap_point(Point2::new(inst_at.x + off[0], inst_at.y + off[1]))
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::write::Dir;

    /// `add_symbol` needs a real symbol library to resolve geometry, so these
    /// tests SKIP-gracefully when no KiCAD environment is detected.
    fn detect_env() -> Option<KicadInstallation> {
        match KicadInstallation::detect() {
            Some(env) => Some(env),
            None => {
                eprintln!("SKIP: no KiCAD environment detected");
                None
            }
        }
    }

    /// A PinGeom with only the fields the endpoint transform reads.
    fn pin_at(x: f64, y: f64) -> PinGeom {
        PinGeom {
            number: "1".to_string(),
            name: "~".to_string(),
            at: [x, y].into(),
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
    fn translate_refreshes_coordinate_derived_keys() {
        let mut w = SchematicWriter::new();
        w.add_wire_on_net([1.27, 2.54], [3.81, 2.54], "SIG");
        w.add_junction([3.81, 2.54]);
        w.add_cluster_label("SIG", [3.81, 2.54], Dir::East, false);

        w.translate(12.7, 25.4);

        assert_eq!(w.wires[0].uuid_key, "13.97:27.94:16.51:27.94");
        assert_eq!(w.junctions[0].uuid_key, "16.51:27.94");
        assert_eq!(w.labels[0].uuid_key, "cluster:SIG:16.51:27.94");
    }

    #[test]
    fn repeated_hidden_refs_relink_position_owned_metadata() {
        let Some(env) = detect_env() else { return };
        let mut w = SchematicWriter::new();
        w.add_power_flag(&env, "VCC", "#SHARED", [12.7, 12.7])
            .unwrap();
        w.add_power_flag(&env, "GND", "#SHARED", [38.1, 12.7])
            .unwrap();
        w.no_connects.push(NoConnect {
            at: [12.7, 12.7].into(),
            uuid_key: "#SHARED:1:0".to_owned(),
        });
        w.no_connects.push(NoConnect {
            at: [38.1, 12.7].into(),
            uuid_key: "#SHARED:1:0".to_owned(),
        });
        w.prefer_fields_above(&std::collections::BTreeSet::from(["#SHARED".to_owned()]));

        assert_eq!(
            w.pwr_flag_nets()
                .into_iter()
                .map(|(net, _)| net)
                .collect::<Vec<_>>(),
            vec!["VCC", "GND"]
        );
        w.namespace_hidden_references("a/b");

        assert_eq!(w.instances[0].refdes, "#a_b_SHARED");
        assert_eq!(w.instances[1].refdes, "#a_b_SHARED_2");
        assert!(
            w.labels
                .iter()
                .any(|label| { label.net == "VCC" && label.uuid_key.starts_with("#a_b_SHARED:") })
        );
        assert!(
            w.labels.iter().any(|label| {
                label.net == "GND" && label.uuid_key.starts_with("#a_b_SHARED_2:")
            })
        );
        assert!(w.no_connects[0].uuid_key.starts_with("#a_b_SHARED:"));
        assert!(w.no_connects[1].uuid_key.starts_with("#a_b_SHARED_2:"));
        assert_eq!(
            w.fields_above,
            std::collections::BTreeSet::from([
                "#a_b_SHARED".to_owned(),
                "#a_b_SHARED_2".to_owned(),
            ])
        );
    }

    #[test]
    fn pin_outward_directions_quantize_per_rotation() {
        let Some(env) = detect_env() else { return };
        let mut w = SchematicWriter::new();
        w.add_symbol(&env, "Device:R", "R1", "1k", [127.0, 63.5], 0.0)
            .unwrap();
        w.add_symbol(&env, "Device:R", "R2", "1k", [101.6, 63.5], 90.0)
            .unwrap();
        let d1 = w.pin_dirs(&env, "R1", "1").unwrap();
        // Device:R pin 1 (local (0, 3.81), angle 270) at instance angle 0:
        // endpoint is above the body, outward points up -> North on the sheet.
        assert_eq!(d1[0].1, Dir::North, "R1 pin 1 stub should point North (up)");
        let d2 = w.pin_dirs(&env, "R2", "1").unwrap();
        // At instance angle 90 the same pin rotates to point West.
        assert_eq!(d2[0].1, Dir::West, "R2 pin 1 stub should point West");
    }
}
