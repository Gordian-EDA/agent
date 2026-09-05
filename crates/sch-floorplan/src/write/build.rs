//! Building the schematic document: placing symbols, labels, wires, junctions,
//! and free graphics, plus the pin-endpoint geometry the connectivity helpers
//! resolve against and the truthfulness-count accessors over the placed scene.

use std::collections::BTreeSet;
use std::io;

use geom::{GRID_50_MIL, Point2, Rect, Segment};
use kicad::KicadInstallation;
use kicad_symbol::geometry::{PinGeom, SymbolGeometry};
use sch_model::geometry::{pin_endpoint, quantize_dir};

use sch_model::route::{DrawnSegment, NetSegment, SymbolInk};

use super::{
    Anchor, Dir, Instance, Junction, NoConnect, PinLabel, SchematicWriter, SheetRect, SheetText,
    Wire,
};

/// Where a pin connects and which way a wire or label leaves it.
pub type PinSeat = ([f64; 2], Dir);

/// A sheet point as an exact, comparable key (µm), so two endpoints that coincide
/// compare equal without a float epsilon.
pub fn point_key(at: Point2) -> (i64, i64) {
    (
        (at.x * 1000.0).round() as i64,
        (at.y * 1000.0).round() as i64,
    )
}

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

    /// Take a symbol's definition and pin geometry from the caller instead of the
    /// installed libraries.
    ///
    /// A placement already carries the geometry of every part it moves, and an edited
    /// sheet may hold parts from a project-local library nothing else can resolve.
    /// Registering the geometry up front means the following `add_symbol_full` never
    /// reads a library at all.
    pub fn register(&mut self, geom: &SymbolGeometry) {
        if self.lib_symbols.contains_key(&geom.lib_id) {
            return;
        }
        self.sym_sizes
            .insert(geom.lib_id.clone(), geom.approx_size());
        self.sym_pins.insert(geom.lib_id.clone(), geom.pins.clone());
        self.lib_symbols
            .insert(geom.lib_id.clone(), geom.raw_definition.clone());
    }

    /// Place one symbol instance with its identity tags.
    ///
    /// The fuller form of [`Self::add_symbol`]: in addition to the placement, it
    /// attaches `extra_props` (the hidden `ap_*` identity tags that make the
    /// emitted file self-describing) and an optional explicit instance `uuid` to
    /// reuse a surviving symbol's id, so re-emitting produces a minimal diff.
    /// `uuid = None` falls back to `stable_uuid("symbol", refdes)`.
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
            self.register(&SymbolGeometry::load(env.symbol_dir(), lib_id)?);
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
    /// [`Self::set_mirror_last`]). `build_writer` calls this when it places the
    /// units of a multi-unit part as separate instances sharing a refdes.
    pub fn set_unit_last(&mut self, unit: u8) {
        if let Some(i) = self.instances.last_mut() {
            i.unit = unit;
        }
    }

    /// Mirror the most recently added symbol left-to-right (`(mirror y)`). Used
    /// to flip an IC so the pins facing its neighbours (e.g. a translator's
    /// B-side toward the connector) point the right way.
    pub fn set_mirror_last(&mut self) {
        if let Some(i) = self.instances.last_mut() {
            i.mirror = true;
        }
    }

    /// Signal-net connectivity with breathing room: a stub wire out of the pin
    /// and the net label at the stub's far end, oriented along the stub so the
    /// text reads away from the symbol body.
    ///
    /// Displacing the label off the pin endpoint risks landing it on a *foreign*
    /// connection point (most often a horizontal power pin's power symbol, which
    /// `emit_rail` parks one row over via a riser): a label there would
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
        self.add_signal_label_stub(env, refdes, pin, net, super::DEFAULT_STUB_MM)
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
        self.add_signal_label_stub_scoped(env, refdes, pin, net, stub_mm)
    }

    fn add_signal_label_stub_scoped(
        &mut self,
        env: &KicadInstallation,
        refdes: &str,
        pin: &str,
        net: &str,
        stub_mm: f64,
    ) -> io::Result<()> {
        for (idx, (ep, dir)) in self
            .pin_dirs_noting(env, refdes, pin)?
            .into_iter()
            .enumerate()
        {
            let ep = GRID_50_MIL.snap_point(ep);
            let v = dir.vec();
            let end =
                GRID_50_MIL.snap_point(Point2::new(ep.x + v.x * stub_mm, ep.y + v.y * stub_mm));
            self.labels.push(PinLabel {
                net: net.to_string(),
                at: end,
                uuid_key: format!("{refdes}:{pin}:{net}:{idx}"),
                dir,
                anchor: Anchor::Stub(ep),
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

    /// Add a wire segment between two sheet points (snapped), on the net it is
    /// drawn for. Same-net touches against it are deliberate joins; a foreign-net
    /// touch is a short.
    ///
    /// If the two points are identical after snapping, the segment is silently
    /// dropped (a zero-length wire would clutter the schematic with no benefit).
    /// The `uuid_key` is content-derived so repeated calls with the same ordered
    /// endpoints produce one deterministic wire.
    pub fn add_wire_on_net(&mut self, a: impl Into<Point2>, b: impl Into<Point2>, net: &str) {
        self.push_wire(a.into(), b.into(), net.to_string());
    }

    fn push_wire(&mut self, a: Point2, b: Point2, net: String) {
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

    /// Assert in debug builds that every wire has a unique unordered endpoint pair.
    pub(super) fn debug_assert_unique_wire_segments(&self) {
        #[cfg(debug_assertions)]
        {
            let mut seen = std::collections::BTreeSet::new();
            for wire in &self.wires {
                let a = point_key(wire.a);
                let b = point_key(wire.b);
                let pair = if a <= b { (a, b) } else { (b, a) };
                debug_assert!(
                    seen.insert(pair),
                    "wire segments must have unique unordered endpoint pairs; repeated {pair:?} on `{}`",
                    wire.net
                );
            }
        }
    }

    /// Place a cluster net label at `at`, oriented `dir`.
    ///
    /// Thin entry point for cluster decoration: a cluster emits exactly one
    /// label per externally-visible net at the net's tap point, so connectivity
    /// joins to the rest of the sheet without per-pin label spam. The label is
    /// keyed on `cluster:{net}:{x}:{y}` (position-derived) and carries no stub —
    /// it sits directly on the cluster wire it labels.
    pub fn add_cluster_label(&mut self, net: &str, at: impl Into<Point2>, dir: Dir) {
        let at = GRID_50_MIL.snap_point(at);
        self.labels.push(PinLabel {
            net: net.to_string(),
            at,
            uuid_key: format!("cluster:{net}:{}:{}", at.x, at.y),
            dir,
            anchor: Anchor::Swivel(dir),
        });
    }

    /// Record a tap where `net`'s own wires meet. Deduplicated by position.
    ///
    /// The tap always splits this net's through-wire at `at` (which is what makes the
    /// join real in the netlist). Whether it is also DRAWN as a junction dot is NOT
    /// decided here — the caller cannot see the sheet it is about to finish, so the
    /// decision belongs to `place_junction_dots`, which runs over the final geometry
    /// and dots exactly the points where three conductors meet. A tap whose point never
    /// becomes a real join is simply not drawn; a join nobody recorded a tap for is
    /// drawn anyway.
    pub fn add_junction_on_net(&mut self, at: impl Into<Point2>, net: &str) {
        let at = GRID_50_MIL.snap_point(at.into());
        let uuid_key = format!("{}:{}", at.x, at.y);
        // Keyed on (point, NET): two nets wanting a tap at one point is a short the audit
        // reports, but dropping the second one would cost that net its split as well and
        // open it. Only one dot is ever DRAWN there (`finish` keeps the first).
        if self
            .junctions
            .iter()
            .any(|j| j.uuid_key == uuid_key && j.net == net)
        {
            return;
        }
        self.junctions.push(Junction {
            at,
            uuid_key,
            net: net.to_string(),
            dot: false,
        });
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

    /// Add a graphic rectangle annotation.
    pub fn add_rect(&mut self, start: impl Into<Point2>, end: impl Into<Point2>, key: &str) {
        self.rects.push(SheetRect {
            start: GRID_50_MIL.snap_point(start.into()),
            end: GRID_50_MIL.snap_point(end.into()),
            uuid_key: key.to_string(),
        });
    }

    /// Every `(refdes, unit)` a pin was asked for but no instance draws.
    ///
    /// Reported beside the layout warnings so a design whose payload connects a
    /// unit the typesetter never seated says so, instead of silently losing the
    /// pin from the drawing.
    pub fn unplaced_unit_warnings(&self) -> Vec<String> {
        self.unplaced_units
            .iter()
            .map(|(refdes, unit)| {
                format!("{refdes}: pins on unit {unit} are connected but no unit-{unit} symbol is placed, so they are joined by name only")
            })
            .collect()
    }

    /// Resolve a pin to its endpoint(s) and outward direction(s), and report any
    /// unit the pin needed that no placed instance draws.
    ///
    /// A multi-unit part places one instance per unit it uses, all sharing a refdes,
    /// and pin geometry is unit-local: a unit-2 pin resolved against unit 1's
    /// placement lands inside unit 1's body pointing into it — the netlist stays
    /// truthful, the drawing does not. So a pin whose unit nothing draws is left
    /// UNDRAWN (its net is joined by name at the units that are placed) and its unit
    /// comes back as missing.
    ///
    /// A pin's local `angle` points from the connection point INTO the body, so
    /// outward is `angle + 180°`, transformed exactly like the endpoint itself
    /// (mirror -> instance rotation -> sheet Y-flip) and quantized to an axis.
    fn resolve_pin(
        &self,
        env: &KicadInstallation,
        refdes: &str,
        pin: &str,
    ) -> io::Result<(Vec<PinSeat>, BTreeSet<u8>)> {
        // Units share a lib_id, so any instance of this refdes is the geometry source.
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
        // Pins are cached per lib_id when the symbol is first added, so this hot
        // path (called once per net-pin during routing) never re-reads the
        // `.kicad_sym` from disk. Fall back to a load only if somehow uncached.
        let pins: Vec<PinGeom> = match self.sym_pins.get(&any.lib_id) {
            Some(p) => p.clone(),
            None => SymbolGeometry::load(env.symbol_dir(), &any.lib_id)?.pins,
        };

        // Number first, then name. A NAME may match several physical pins (an MCU's
        // four VSS), and each gets its own endpoint.
        let matches: Vec<&PinGeom> = {
            let by_number: Vec<&PinGeom> = pins.iter().filter(|p| p.number == pin).collect();
            match by_number.is_empty() {
                false => by_number,
                true => pins.iter().filter(|p| p.name == pin).collect(),
            }
        };
        if matches.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no pin {pin:?} (by number or name) on {}", any.lib_id),
            ));
        }

        let mut drawn = Vec::new();
        let mut missing = BTreeSet::new();
        for pg in matches {
            let unit = pg.unit.max(1);
            match self
                .instances
                .iter()
                .find(|i| i.refdes == refdes && i.unit == unit)
            {
                Some(inst) => drawn.push((
                    pin_endpoint(pg, inst.at, inst.angle, inst.mirror),
                    quantize_dir(pg.angle, inst.angle, inst.mirror),
                )),
                None => {
                    missing.insert(unit);
                }
            }
        }
        Ok((drawn, missing))
    }

    /// [`Self::resolve_pin`]'s endpoints and outward directions.
    pub fn pin_dirs(
        &self,
        env: &KicadInstallation,
        refdes: &str,
        pin: &str,
    ) -> io::Result<Vec<PinSeat>> {
        Ok(self.resolve_pin(env, refdes, pin)?.0)
    }

    /// [`Self::pin_dirs`], recording every unit the pin needed that nothing draws.
    ///
    /// Each emitter that puts ink on a pin goes through this, so a connected pin
    /// dropped for want of its unit always reaches
    /// [`Self::unplaced_unit_warnings`].
    fn pin_dirs_noting(
        &mut self,
        env: &KicadInstallation,
        refdes: &str,
        pin: &str,
    ) -> io::Result<Vec<PinSeat>> {
        let (drawn, missing) = self.resolve_pin(env, refdes, pin)?;
        for unit in missing {
            self.unplaced_units.insert((refdes.to_string(), unit));
        }
        Ok(drawn)
    }

    /// Reserve the endpoints of every `(refdes, pin)` the design put on a NET, so no
    /// later no-connect marker can claim one.
    ///
    /// A marker declares a POINT unconnected, not a pin. Symbols stack their duplicate
    /// power pins on ONE endpoint (an ESP32's four GNDs, a USB-C receptacle's two
    /// VBUS), so a pin the payload never mentioned — auto-no-connected by the kernel —
    /// can share its point with a pin that is wired. Marking it severs the point for
    /// KiCAD's netlister and the rail comes back as islands: an OPEN, invisible on the
    /// rendered sheet.
    ///
    /// Call once, before any [`Self::add_no_connect`].
    pub fn declare_connected<'a>(
        &mut self,
        env: &KicadInstallation,
        pins: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> io::Result<()> {
        for (refdes, pin) in pins {
            for (at, _) in self.pin_dirs_noting(env, refdes, pin)? {
                self.connected.insert(point_key(Point2::from(at)));
            }
        }
        Ok(())
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
    ///
    /// A marker is refused on a point [`Self::declare_connected`] reserved — see
    /// that method for why.
    pub fn add_no_connect(
        &mut self,
        env: &KicadInstallation,
        refdes: &str,
        pin: &str,
    ) -> io::Result<()> {
        let endpoints = self.pin_dirs_noting(env, refdes, pin)?;
        for (idx, (at, _)) in endpoints.into_iter().enumerate() {
            let at = Point2::from(at);
            if self.connected.contains(&point_key(at)) {
                continue;
            }
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
            anchor: Anchor::Fixed,
        });
        Ok(())
    }

    /// Resolve a pin reference to its sheet-space connection endpoint(s).
    ///
    /// [`Self::resolve_pin`] without the directions — shared by label, no-connect
    /// and power-flag emission so they always agree on where a pin's connection
    /// point lands.
    pub fn pin_endpoints(
        &self,
        env: &KicadInstallation,
        refdes: &str,
        pin: &str,
    ) -> io::Result<Vec<Point2>> {
        Ok(self
            .pin_dirs(env, refdes, pin)?
            .into_iter()
            .map(|(at, _)| Point2::from(at))
            .collect())
    }

    /// The sheet-space connection point of every pin the placed instances draw.
    ///
    /// A multi-unit part places one instance per unit, so each instance
    /// contributes only its own unit's pins. Power symbols contribute their
    /// single origin pin like any other.
    pub(super) fn pin_points(&self) -> Vec<Point2> {
        let mut out = Vec::new();
        for inst in &self.instances {
            let Some(pins) = self.sym_pins.get(&inst.lib_id) else {
                continue;
            };
            for pg in pins.iter().filter(|p| p.unit.max(1) == inst.unit) {
                out.push(Point2::from(pin_endpoint(
                    pg,
                    inst.at,
                    inst.angle,
                    inst.mirror,
                )));
            }
        }
        out
    }


    /// The INK one placed instance draws, as a routing obstacle: its own unit's body
    /// graphics and its own unit's pin name/number text, with its pin connection points.
    ///
    /// The body comes from the definition the writer will embed, posed onto the sheet —
    /// the same shape `sch_doc::body_rect` hands the visual audit, so a route the router
    /// calls clear is one the render shows clear. [`ink_box`] cannot stand in for it: it
    /// is a box CENTRED on the placement origin, and a symbol whose graphics sit off that
    /// origin (a crystal's plates, a regulator's tab) leaves real ink outside it — which
    /// is where every measured wire-through-body ran.
    ///
    /// Field text is deliberately absent: `solve_text_positions` seats it after the
    /// wires are drawn, so a box reserved here would guard paper the text has left.
    fn symbol_ink(&self, inst: &Instance) -> SymbolInk {
        let mut boxes = vec![self.definition_ink(inst)];
        if !inst.refdes.starts_with('#') {
            boxes.extend(self.unit_pin_text(inst));
        }
        SymbolInk {
            boxes,
            pins: self.unit_pin_points(inst),
        }
    }

    /// `inst`'s unit box read off its embedded definition and posed onto the sheet,
    /// falling back to [`ink_box`] when the definition does not parse.
    ///
    /// Memoized per `(lib_id, unit)`: a routing pass asks for the scene once per net,
    /// and the definition of a 121-pin FPGA is tens of kilobytes.
    pub(super) fn definition_ink(&self, inst: &Instance) -> Rect {
        thread_local! {
            static LOCAL_BOX: std::cell::RefCell<
                std::collections::BTreeMap<(String, u8), Option<Rect>>,
            > = const { std::cell::RefCell::new(std::collections::BTreeMap::new()) };
        }
        let local = LOCAL_BOX.with(|memo| {
            *memo
                .borrow_mut()
                .entry((inst.lib_id.clone(), inst.unit))
                .or_insert_with(|| {
                    sch_doc::definition_unit_box(self.lib_symbols.get(&inst.lib_id)?, inst.unit)
                })
        });
        let Some(local) = local else {
            return ink_box(inst);
        };
        let corners = [
            Point2::new(local.min_x, local.min_y),
            Point2::new(local.max_x, local.min_y),
            Point2::new(local.min_x, local.max_y),
            Point2::new(local.max_x, local.max_y),
        ]
        .map(|p| {
            let off = p.transform_offset(inst.angle, inst.mirror);
            Point2::new(inst.at.x + off.x, inst.at.y + off.y)
        });
        Rect::bounding(&corners).unwrap_or_else(|| ink_box(inst))
    }

    /// The name/number text `inst`'s OWN unit draws. `sym_pins` flattens every unit's
    /// pins together, so boxing them all at one instance walls off the paper around
    /// every placement of a multi-unit part.
    pub(super) fn unit_pin_text(&self, inst: &Instance) -> Vec<Rect> {
        self.unit_pins(inst)
            .flat_map(|pg| sch_model::text::pin_text_boxes(pg, inst.at, inst.angle, inst.mirror))
            .collect()
    }

    /// Where `inst`'s own unit's pins connect — the points its own wires leave from.
    fn unit_pin_points(&self, inst: &Instance) -> Vec<Point2> {
        self.unit_pins(inst)
            .map(|pg| pin_endpoint(pg, inst.at, inst.angle, inst.mirror).into())
            .collect()
    }

    fn unit_pins(&self, inst: &Instance) -> impl Iterator<Item = &PinGeom> {
        self.sym_pins
            .get(&inst.lib_id)
            .into_iter()
            .flatten()
            .filter(move |pg| pg.unit.max(1) == inst.unit.max(1))
    }

    /// Build the routing obstacle scene from everything placed so far.
    ///
    /// Solids are symbol bodies SHRUNK by 2.54 mm per side: `approx_size` pads
    /// 2.54 beyond the pin endpoints, so shrinking puts pin connection points
    /// exactly ON the solid boundary (open-interval checks let wires depart
    /// from them) while the glyph stays protected. That box is CENTRED on the
    /// placement origin, though, so it is a placement clearance and not the
    /// drawing: [`Self::symbol_ink`] states the ink itself alongside it. Points
    /// carry the same foreign-anchor model as `retract_colliding_stubs` (power
    /// origins, no-connects, label anchors); wire segments carry the net they
    /// were drawn for.
    pub fn route_scene(&self) -> sch_model::route::RouteScene {
        const NC: &str = "\0no_connect";
        let mut scene = sch_model::route::RouteScene {
            solids: Vec::new(),
            points: Vec::new(),
            segments: Vec::new(),
            label_solids: Vec::new(),
            ink: Vec::new(),
        };
        for inst in &self.instances {
            scene.ink.push(self.symbol_ink(inst));
            if inst.refdes.starts_with('#') {
                // Power symbols: the single pin at the origin is the anchor, and the GLYPH
                // itself is ink with real extent. A foreign wire drawn 2.54 mm off the
                // anchor misses the pin and slices the triangle — a clean netlist that
                // reads as a short, and the value text drops out to make room. So the
                // glyph gets the same keepout the no-connect X gets: tagged with its own
                // net, which its stub may reach and every other wire detours around.
                scene.points.push((inst.at, inst.value.clone()));
                // The keepout is the drawn triangle plus a hair, not the symbol's padded
                // placement box, so it never walls off the channel beside a rail.
                scene.label_solids.push((ink_box(inst), inst.value.clone()));
                continue;
            }
            scene.solids.push(ink_box(inst));
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
            if let Anchor::Stub(pin_at) = l.anchor {
                scene.points.push((pin_at, l.net.clone()));
            }
        }
        for w in &self.wires {
            scene
                .segments
                .push(NetSegment::new(w.a, w.b, w.net.clone()));
        }
        scene.points.extend(self.beside.points.iter().cloned());
        scene.segments.extend(self.beside.segments.iter().cloned());
        scene.solids.extend(self.beside.solids.iter().copied());
        scene.ink.extend(self.beside.ink.iter().cloned());
        scene
    }

    /// Whether this writer is drawing a block INTO a sheet that already has content,
    /// rather than composing a whole sheet of its own.
    pub fn joins_existing_content(&self) -> bool {
        !self.beside.points.is_empty() || !self.beside.segments.is_empty()
    }

    /// The neighbouring sheet's terminals, tagged with the nets they already carry.
    pub fn beside_terminals(&self) -> Vec<(Point2, String)> {
        self.beside.points.clone()
    }

    /// The neighbouring sheet's wire segments, tagged with the nets they already carry.
    pub fn beside_wires(&self) -> Vec<(Segment, String)> {
        self.beside
            .segments
            .iter()
            .map(|s| (s.segment, s.net.clone()))
            .collect()
    }

    /// Declare the drawing this block is being added beside: the existing sheet's pins,
    /// wire ends and label anchors with the nets they already carry.
    ///
    /// A block placed onto a populated sheet is routed by a writer that holds only the
    /// block; without this it draws its wires straight across the sheet's pins and
    /// welds nets it never heard of. Whole-sheet builds pass nothing.
    pub fn set_beside(&mut self, beside: sch_model::route::RouteScene) {
        self.beside = beside;
    }

    /// Wire segments attributed to `net` (for junction counting at taps).
    pub fn wire_segments_on_net(&self, net: &str) -> Vec<Segment> {
        self.wires
            .iter()
            .filter(|w| w.net == net)
            .map(|w| Segment::new(w.a, w.b))
            .collect()
    }

    /// Positions of the junction dots the sheet DRAWS, which
    /// `place_junction_dots` decides over the final geometry — so this reports the
    /// shipped dots only on a writer that has been through [`Self::prepare`].
    pub fn junction_positions(&self) -> Vec<[f64; 2]> {
        let mut seen = std::collections::BTreeSet::new();
        self.junctions
            .iter()
            .filter(|j| j.dot && seen.insert(j.uuid_key.clone()))
            .map(|j| j.at.into())
            .collect()
    }

    /// The connection point and net of every power symbol (`power:` graphic port),
    /// whose single pin sits at the symbol origin and whose Value names the net.
    /// `PWR_FLAG` is excluded: it drives whatever it is attached to rather than a
    /// net of its own.
    pub fn power_pins(&self) -> Vec<([f64; 2], String)> {
        self.instances
            .iter()
            .filter(|i| i.lib_id.starts_with("power:") && i.lib_id != "power:PWR_FLAG")
            .map(|i| (i.at.into(), i.value.clone()))
            .collect()
    }

    /// The anchor point and net of every label — for a stub-mounted label, both the
    /// text anchor and the pin endpoint the stub runs from, since the stub binds
    /// both ends to the net.
    pub fn label_anchors(&self) -> Vec<([f64; 2], String)> {
        let mut out = Vec::new();
        for l in &self.labels {
            out.push((l.at.into(), l.net.clone()));
            if let Anchor::Stub(pin_at) = l.anchor {
                out.push((pin_at.into(), l.net.clone()));
            }
        }
        out
    }

    /// Count of pin-mounted signal labels — the nets the router could not wire and
    /// named instead. Port and orphan labels are welded to a tap rather than to a
    /// pin, so they are excluded: this is a direct "how many nets degraded" signal.
    pub fn signal_label_count(&self) -> usize {
        self.labels
            .iter()
            .filter(|l| !matches!(l.anchor, Anchor::Swivel(_)))
            .count()
    }

    /// Every drawn wire segment with its net, for `place::score`'s truthfulness counts
    /// (crossings, merges, foreign taps).
    pub fn wires_with_nets(&self) -> Vec<DrawnSegment> {
        self.wires
            .iter()
            .map(|w| DrawnSegment::new(w.a, w.b, Some(w.net.clone())))
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

    /// Rigidly shift EVERY drawn element (instances + their solved field text,
    /// wires, labels + stubs, junctions, no-connects, free text, rects) by
    /// `(dx, dy)`. The typed sibling of `reframe`'s shift block: connectivity is
    /// preserved (everything moves together), so the multi-block composer can
    /// translate a fully-laid-out group writer to its tile in mm — no string
    /// geometry math. Caller keeps the shift grid-aligned to stay on the KiCAD grid.
    ///
    /// Connection geometry re-snaps (it is grid-aligned already, so this only
    /// absorbs float drift), but solved field text does NOT: its anchors are
    /// deliberately OFF-grid sub-millimetre offsets from their body, and
    /// re-snapping each one independently slides it up to half a grid step
    /// relative to the symbols the text solver cleared it against — turning a
    /// collision-free layout into an overlap. Text moves by the shift, rigidly.
    pub fn translate(&mut self, dx: f64, dy: f64) {
        let sh = |p: &mut Point2| {
            *p = GRID_50_MIL.snap_point(Point2::new(p.x + dx, p.y + dy));
        };
        let sha = |p: &mut [f64; 2]| {
            *p = [p[0] + dx, p[1] + dy];
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
            if let Anchor::Stub(pin_at) = &mut l.anchor {
                sh(pin_at);
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

/// The box of INK an instance actually draws — body graphic plus pin lines —
/// as opposed to [`Instance::half_extents`], which is the PADDED placement
/// cell (`approx_size` adds 2.54 mm per side beyond the pin endpoints).
///
/// Two consumers need the ink, not the cell: the router (a wire may graze the
/// padding but never the glyph) and the text solver (a field is legible in the
/// padding; it is unreadable on top of a body). A power symbol's glyph is a
/// small triangle at its anchor and never fills the 10 mm cell `approx_size`
/// floors it to, so it is measured directly.
pub(crate) fn ink_box(inst: &Instance) -> Rect {
    let h = if inst.refdes.starts_with('#') {
        Point2::new(1.27, 3.175).rotated_half_extents(inst.angle)
    } else {
        let h = inst.half_extents.rotated_half_extents(inst.angle);
        Point2::new((h[0] - 2.54).max(1.27), (h[1] - 2.54).max(1.27))
    };
    Rect::new(
        inst.at[0] - h[0],
        inst.at[1] - h[1],
        inst.at[0] + h[0],
        inst.at[1] + h[1],
    )
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
            text: Default::default(),
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
    fn a_signal_label_needs_a_real_pin() {
        let Some(env) = detect_env() else { return };
        let mut w = SchematicWriter::new();
        w.add_symbol(&env, "Device:R", "R1", "1k", [127.0, 63.5], 0.0)
            .unwrap();

        w.add_signal_label_stub(&env, "R1", "1", "PORT", 3.81)
            .unwrap();
        assert_eq!(w.labels.len(), 1);
        assert!(
            matches!(w.labels[0].anchor, Anchor::Stub(_)),
            "the label must be wired to its pin"
        );

        let before = w.labels.len();
        assert!(
            w.add_signal_label_stub(&env, "R1", "missing", "ORPHAN", 3.81)
                .is_err()
        );
        assert_eq!(
            w.labels.len(),
            before,
            "a pin key that resolves to nothing must not emit a label"
        );
    }

    #[test]
    fn pin_endpoint_mirror_negates_sheet_x() {
        // A pin offset in X. At angle 0 reflecting the sheet x is the same as
        // reflecting the local x: local (2.54, 0) -> sheet (124.46, 63.5).
        let p = pin_at(2.54, 0.0);
        let inst = [127.0, 63.5];
        pt_close(pin_endpoint(&p, inst, 0.0, false), [129.54, 63.5]);
        pt_close(pin_endpoint(&p, inst, 0.0, true), [124.46, 63.5]);

        // At 90 deg it is NOT: Device:R pin 1, local (0, 3.81), lands unmirrored at
        // (123.19, 63.5) and mirrored at (130.81, 63.5) — pin 2's unmirrored place,
        // which is exactly the transposition that mis-wired a mirrored 2-pin part.
        let p1 = pin_at(0.0, 3.81);
        pt_close(pin_endpoint(&p1, inst, 90.0, false), [123.19, 63.5]);
        pt_close(pin_endpoint(&p1, inst, 90.0, true), [130.81, 63.5]);
    }

    #[test]
    fn translate_refreshes_coordinate_derived_keys() {
        let mut w = SchematicWriter::new();
        w.add_wire_on_net([1.27, 2.54], [3.81, 2.54], "SIG");
        w.add_junction_on_net([3.81, 2.54], "SIG");
        w.add_cluster_label("SIG", [3.81, 2.54], Dir::East);

        w.translate(12.7, 25.4);

        assert_eq!(w.wires[0].uuid_key, "13.97:27.94:16.51:27.94");
        assert_eq!(w.junctions[0].uuid_key, "16.51:27.94");
        assert_eq!(w.labels[0].uuid_key, "cluster:SIG:16.51:27.94");
    }

    /// A shift must not re-grid solved field text: its anchor is a deliberate
    /// off-grid offset from the body, and snapping it independently slides it
    /// into the neighbour the text solver had cleared it of.
    #[test]
    fn translate_keeps_field_text_rigid_with_its_body() {
        let Some(env) = detect_env() else { return };
        let mut w = SchematicWriter::new();
        w.add_symbol(&env, "Device:R", "R1", "1k", [101.6, 101.6], 0.0)
            .unwrap();
        w.instances[0].ref_pos = Some(crate::write::TextPos {
            at: [104.78, 98.42],
            justify: crate::write::Justify::Center,
        });

        w.translate(12.7, 25.4);

        assert_eq!(w.instances[0].at, Point2::new(114.3, 127.0));
        assert_eq!(w.instances[0].ref_pos.unwrap().at, [117.48, 123.82]);
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

    /// A pin belongs to the unit that draws it: with unit 2 of an LM324 seated
    /// well away from unit 1, unit 2's inverting input resolves at unit 2 — never
    /// at unit 1's placement with unit 2's own local offset.
    #[test]
    fn a_pin_resolves_at_the_instance_that_draws_its_unit() {
        let Some(env) = detect_env() else { return };
        let mut w = SchematicWriter::new();
        w.add_symbol(
            &env,
            "Amplifier_Operational:LM324",
            "U1",
            "LM324",
            [50.8, 50.8],
            0.0,
        )
        .unwrap();
        w.add_symbol(
            &env,
            "Amplifier_Operational:LM324",
            "U1",
            "LM324",
            [127.0, 101.6],
            0.0,
        )
        .unwrap();
        w.set_unit_last(2);

        let unit1 = w.pin_dirs(&env, "U1", "1").unwrap();
        let unit2 = w.pin_dirs(&env, "U1", "6").unwrap();
        assert_eq!(unit1.len(), 1);
        assert_eq!(unit2.len(), 1);
        assert!(
            (unit1[0].0[0] - 50.8).abs() < 12.7 && (unit2[0].0[0] - 127.0).abs() < 12.7,
            "each pin must sit beside its own unit: {unit1:?} {unit2:?}"
        );
        assert!(w.unplaced_unit_warnings().is_empty());
    }

    /// The other half of the same rule: a pin whose unit nothing draws is not
    /// drawn at a foreign unit's origin — it is not drawn at all, and the writer
    /// says which unit went missing.
    #[test]
    fn a_pin_on_an_unplaced_unit_is_not_drawn_at_another_unit() {
        let Some(env) = detect_env() else { return };
        let mut w = SchematicWriter::new();
        w.add_symbol(
            &env,
            "Amplifier_Operational:LM324",
            "U1",
            "LM324",
            [50.8, 50.8],
            0.0,
        )
        .unwrap();
        assert!(w.pin_dirs(&env, "U1", "6").unwrap().is_empty());
        w.add_signal_label(&env, "U1", "6", "FEEDBACK").unwrap();
        assert!(
            w.labels.iter().all(|l| l.net != "FEEDBACK"),
            "a label for an unplaced unit's pin must not be drawn"
        );
        let warnings = w.unplaced_unit_warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("U1") && warnings[0].contains("unit 2"),
            "{warnings:?}"
        );
    }
}
