//! The field/label placement solver and the geometry-finalizing passes.
//!
//! Everything here mutates the placed scene into its final, collision-resolved
//! form: stub retraction, wire splitting at taps, the greedy candidate solver
//! for movable text ([`SchematicWriter::solve_text_positions`]), reframing onto
//! the content-fit page, and the deterministic readability lint
//! ([`SchematicWriter::layout_warnings`]). All passes are idempotent so they may
//! run early (to lint final geometry) and again in `finish` harmlessly.

use std::collections::{BTreeMap, BTreeSet};

use geom::{EPS, Point2, Segment};

use crate::grid::snap_point;

use super::{BBox, Justify, SchematicWriter, TextPos, Wire, field_anchors, field_box};

/// What [`SchematicWriter::solve_text_positions`] mutates once the greedy solver
/// has picked a candidate for the parallel [`crate::label::Movable`].
enum Apply {
    /// labels[i]: candidate 1 retracts onto the pin endpoint.
    StubLabel(usize),
    /// instances[i]: per-candidate (Reference, Value) anchors.
    Fields(usize, Vec<(TextPos, TextPos)>),
    /// instances[i]: per-candidate Value anchor (power rail name).
    PowerVal(usize, Vec<TextPos>),
}

impl SchematicWriter {
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

        let bits = |p: Point2| {
            let p = snap_point(p);
            (p[0].to_bits(), p[1].to_bits())
        };

        // Foreign points: net name(s) at each occupied point.
        let mut points: BTreeMap<(u64, u64), std::collections::BTreeSet<String>> = BTreeMap::new();
        let add_point =
            |p: Point2,
             net: &str,
             m: &mut BTreeMap<(u64, u64), std::collections::BTreeSet<String>>| {
                m.entry(bits(p)).or_default().insert(net.to_string());
            };
        // Foreign axis-aligned segments: (a, b, net).
        let mut segments: Vec<(Point2, Point2, String)> = Vec::new();

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
                .any(|(a, b, n)| *n != net && Segment::new(*a, *b).contains_point(end));
            let seg_thru_point = points.iter().any(|(&(xb, yb), nets)| {
                let p = Point2::new(f64::from_bits(xb), f64::from_bits(yb));
                nets.iter().any(|n| *n != net) && Segment::new(pin_at, end).contains_point(p)
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
    /// candidate solver [`crate::label::choose`].
    ///
    /// Builds the obstacle scene ([`Self::build_obstacles`]) then the movables
    /// in most-constrained-first order — stub signal labels
    /// ([`Self::stub_label_movables`]) ahead of the refdes-ordered field/power
    /// pass ([`Self::field_movables`]) — solves, and applies each pick.
    ///
    /// Idempotent: every assignment is recomputed from scratch on each call
    /// (a retract-chosen label has no stub on the re-run and becomes a fixed
    /// obstacle at the same position), so reconcile may run it early to lint
    /// solved geometry and `finish`'s own call is a harmless re-run.
    pub fn solve_text_positions(&mut self) {
        use crate::label::choose;

        let obstacles = self.build_obstacles();
        let (mut movables, mut applies) = self.stub_label_movables();
        let (field_movables, field_applies) = self.field_movables();
        movables.extend(field_movables);
        applies.extend(field_applies);

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

    /// Everything solved text must avoid: symbol bodies (angle-aware, exempt
    /// for their own refdes), pin name/number text, wires, no-connect markers,
    /// and fixed (stub-less) labels.
    fn build_obstacles(&self) -> Vec<crate::label::Obstacle> {
        use crate::label::{
            ObKind, Obstacle, label_box, pin_text_boxes, rotated_half_extents, text_width, wire_box,
        };
        let mut obstacles: Vec<Obstacle> = Vec::new();
        for inst in &self.instances {
            let h = rotated_half_extents(inst.half_extents, inst.angle);
            obstacles.push(Obstacle {
                bbox: [
                    inst.at[0] - h[0],
                    inst.at[1] - h[1],
                    inst.at[0] + h[0],
                    inst.at[1] + h[1],
                ]
                .into(),
                kind: ObKind::OwnExempt(inst.refdes.clone()),
            });
            // Pin name/number text (skip power/flag graphics — single
            // unnamed pin, no meaningful pin text).
            if !inst.refdes.starts_with('#')
                && let Some(pins) = self.sym_pins.get(&inst.lib_id)
            {
                for pg in pins {
                    for b in pin_text_boxes(pg, inst.at, inst.angle, inst.mirror) {
                        obstacles.push(Obstacle {
                            bbox: b,
                            kind: ObKind::Hard,
                        });
                    }
                }
            }
        }
        for w in &self.wires {
            obstacles.push(Obstacle {
                bbox: wire_box(w.a, w.b),
                kind: ObKind::Hard,
            });
        }
        for nc in &self.no_connects {
            obstacles.push(Obstacle {
                bbox: [
                    nc.at[0] - 0.64,
                    nc.at[1] - 0.64,
                    nc.at[0] + 0.64,
                    nc.at[1] + 0.64,
                ]
                .into(),
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
        obstacles
    }

    /// Stub signal labels (most constrained, solved first), in deterministic
    /// uuid_key order. Each has two candidates: stay at the stub end, or retract
    /// onto the always-safe pin endpoint keeping the outward direction (the stub
    /// wire is dropped when retraction wins).
    fn stub_label_movables(&self) -> (Vec<crate::label::Movable>, Vec<Apply>) {
        use crate::label::{Movable, label_box, text_width};
        let mut movables: Vec<Movable> = Vec::new();
        let mut applies: Vec<Apply> = Vec::new();
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
        (movables, applies)
    }

    /// Reference+Value field pairs and power-symbol rail names, in deterministic
    /// refdes order (one shared pass, so greedy solve order is stable). Each
    /// instance dispatches to [`Self::power_value_movable`] (power symbols) or
    /// [`Self::field_pair_movable`] (everything else).
    fn field_movables(&self) -> (Vec<crate::label::Movable>, Vec<Apply>) {
        let mut movables = Vec::new();
        let mut applies = Vec::new();
        let mut order: Vec<usize> = (0..self.instances.len()).collect();
        order.sort_by(|&a, &b| self.instances[a].refdes.cmp(&self.instances[b].refdes));
        for &i in &order {
            let pair = if self.instances[i].refdes.starts_with('#') {
                self.power_value_movable(i)
            } else {
                Some(self.field_pair_movable(i))
            };
            if let Some((m, a)) = pair {
                movables.push(m);
                applies.push(a);
            }
        }
        (movables, applies)
    }

    /// Power-symbol Value (the rail name): beyond the symbol tip (below for
    /// down-pointing GND-family rails, above otherwise), else right / left — so
    /// adjacent rails never merge their names. `None` for `power:PWR_FLAG`,
    /// whose Value is hidden and has nothing to place.
    fn power_value_movable(&self, i: usize) -> Option<(crate::label::Movable, Apply)> {
        use crate::label::{Movable, rotated_half_extents, text_width};
        let r2 = |v: f64| (v * 100.0).round() / 100.0;
        let inst = &self.instances[i];
        if inst.lib_id == "power:PWR_FLAG" {
            return None;
        }
        let h = rotated_half_extents(inst.half_extents, inst.angle);
        let (cx, cy) = (inst.at[0], inst.at[1]);
        let (minx, miny, maxx, maxy) = (cx - h[0], cy - h[1], cx + h[0], cy + h[1]);
        let vw = text_width(&inst.value);
        let above = (
            TextPos {
                at: [r2(cx), r2(miny - 0.64)],
                justify: Justify::Center,
            },
            [cx - vw / 2.0, miny - 2.24, cx + vw / 2.0, miny - 0.64].into(),
        );
        let below = (
            TextPos {
                at: [r2(cx), r2(maxy + 2.24)],
                justify: Justify::Center,
            },
            [cx - vw / 2.0, maxy + 0.64, cx + vw / 2.0, maxy + 2.24].into(),
        );
        let right = (
            TextPos {
                at: [r2(maxx + 0.64), r2(cy + 0.8)],
                justify: Justify::Left,
            },
            [maxx + 0.64, cy - 0.8, maxx + 0.64 + vw, cy + 0.8].into(),
        );
        let left = (
            TextPos {
                at: [r2(minx - 0.64), r2(cy + 0.8)],
                justify: Justify::Right,
            },
            [minx - 0.64 - vw, cy - 0.8, minx - 0.64, cy + 0.8].into(),
        );
        // A 180-rotated power symbol points down (GND family): the
        // name goes below the graphic; otherwise above.
        let cands = if inst.angle == 180.0 {
            vec![below, right, left]
        } else {
            vec![above, right, left]
        };
        let movable = Movable {
            owner: Some(inst.refdes.clone()),
            candidates: cands.iter().map(|c| c.1).collect(),
        };
        Some((
            movable,
            Apply::PowerVal(i, cands.into_iter().map(|c| c.0).collect()),
        ))
    }

    /// Reference+Value field pair for a non-power instance: right / left / above
    /// / below of the body, with corner and far-band fallbacks for crowded
    /// symbols. Wide (rotated passive) bodies prefer above/below; ICs carry the
    /// pair on the horizontal band least overlapping their own pin text.
    fn field_pair_movable(&self, i: usize) -> (crate::label::Movable, Apply) {
        use crate::label::{Movable, pin_text_boxes, rotated_half_extents, text_width};
        let r2 = |v: f64| (v * 100.0).round() / 100.0;
        let inst = &self.instances[i];
        let h = rotated_half_extents(inst.half_extents, inst.angle);
        let (cx, cy) = (inst.at[0], inst.at[1]);
        let (minx, miny, maxx, maxy) = (cx - h[0], cy - h[1], cx + h[0], cy + h[1]);
        let vw = text_width(&inst.value);
        let rw = text_width(&inst.refdes);
        let wmax = rw.max(vw);
        // Each candidate: (ref anchor, val anchor, union bbox). Text is
        // bottom-anchored and 1.6 tall, so a line anchored at Y occupies
        // [Y-1.6, Y].
        let right = (
            TextPos {
                at: [r2(maxx + 1.27), r2(cy - 1.27)],
                justify: Justify::Left,
            },
            TextPos {
                at: [r2(maxx + 1.27), r2(cy + 1.27)],
                justify: Justify::Left,
            },
            [maxx + 1.27, cy - 2.87, maxx + 1.27 + wmax, cy + 1.27].into(),
        );
        let left = (
            TextPos {
                at: [r2(minx - 1.27), r2(cy - 1.27)],
                justify: Justify::Right,
            },
            TextPos {
                at: [r2(minx - 1.27), r2(cy + 1.27)],
                justify: Justify::Right,
            },
            [minx - 1.27 - wmax, cy - 2.87, minx - 1.27, cy + 1.27].into(),
        );
        let above = (
            TextPos {
                at: [r2(cx), r2(miny - 3.18)],
                justify: Justify::Center,
            },
            TextPos {
                at: [r2(cx), r2(miny - 0.64)],
                justify: Justify::Center,
            },
            [cx - wmax / 2.0, miny - 4.78, cx + wmax / 2.0, miny - 0.64].into(),
        );
        let below = (
            TextPos {
                at: [r2(cx), r2(maxy + 2.24)],
                justify: Justify::Center,
            },
            TextPos {
                at: [r2(cx), r2(maxy + 4.78)],
                justify: Justify::Center,
            },
            [cx - wmax / 2.0, maxy + 0.64, cx + wmax / 2.0, maxy + 4.78].into(),
        );
        // Corner fallbacks for crowded symbols (an IC whose four sides all
        // carry labels/power): the field pair tucks against a body corner.
        let above_left = (
            TextPos {
                at: [r2(minx), r2(miny - 3.18)],
                justify: Justify::Left,
            },
            TextPos {
                at: [r2(minx), r2(miny - 0.64)],
                justify: Justify::Left,
            },
            [minx, miny - 4.78, minx + wmax, miny - 0.64].into(),
        );
        let above_right = (
            TextPos {
                at: [r2(maxx), r2(miny - 3.18)],
                justify: Justify::Right,
            },
            TextPos {
                at: [r2(maxx), r2(miny - 0.64)],
                justify: Justify::Right,
            },
            [maxx - wmax, miny - 4.78, maxx, miny - 0.64].into(),
        );
        let below_left = (
            TextPos {
                at: [r2(minx), r2(maxy + 2.24)],
                justify: Justify::Left,
            },
            TextPos {
                at: [r2(minx), r2(maxy + 4.78)],
                justify: Justify::Left,
            },
            [minx, maxy + 0.64, minx + wmax, maxy + 4.78].into(),
        );
        let below_right = (
            TextPos {
                at: [r2(maxx), r2(maxy + 2.24)],
                justify: Justify::Right,
            },
            TextPos {
                at: [r2(maxx), r2(maxy + 4.78)],
                justify: Justify::Right,
            },
            [maxx - wmax, maxy + 0.64, maxx, maxy + 4.78].into(),
        );
        // Last-resort FAR bands (pushed ~5 mm further out): when a body is
        // ringed by packed neighbours — a tight decoupling cluster on a dense
        // board — every near spot is blocked and the solver would fall onto a
        // sibling's label/field. A far band clears it (the text reads a touch
        // detached but never overlaps). Appended LAST for both ICs and passives,
        // so a part with any near free spot is unaffected.
        let above_far = (
            TextPos {
                at: [r2(cx), r2(miny - 8.18)],
                justify: Justify::Center,
            },
            TextPos {
                at: [r2(cx), r2(miny - 5.64)],
                justify: Justify::Center,
            },
            [cx - wmax / 2.0, miny - 9.78, cx + wmax / 2.0, miny - 5.64].into(),
        );
        let below_far = (
            TextPos {
                at: [r2(cx), r2(maxy + 5.64)],
                justify: Justify::Center,
            },
            TextPos {
                at: [r2(cx), r2(maxy + 8.18)],
                justify: Justify::Center,
            },
            [cx - wmax / 2.0, maxy + 5.64, cx + wmax / 2.0, maxy + 9.78].into(),
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
        let is_ic = self
            .sym_pins
            .get(&inst.lib_id)
            .is_some_and(|p| p.len() >= 3);
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
                pin_boxes.iter().filter(|pb| c.2.overlaps(pb)).count()
            };
            let mut bands = vec![
                below,
                above,
                below_left,
                below_right,
                above_left,
                above_right,
            ];
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
                above,
                below,
                right,
                left,
                above_left,
                above_right,
                below_left,
                below_right,
                above_far,
                below_far,
            ]
        } else {
            vec![
                right,
                left,
                above,
                below,
                above_left,
                above_right,
                below_left,
                below_right,
                above_far,
                below_far,
            ]
        };
        // A flagged low-side FET (its down-facing source pin hangs a rotated
        // SHUNT port label) prefers its fields ABOVE the body: stable-partition
        // the candidate list so every above-the-body band comes first, before
        // the solver's first-fit reaches a below-body spot that would crowd the
        // port label's vertical strip. Stable so the existing tie-break order
        // within "above" and within "the rest" is preserved.
        let cands = if self.fields_above.contains(&inst.refdes) {
            let (mut up, mut rest): (Vec<_>, Vec<_>) =
                cands.into_iter().partition(|c| c.2[3] <= cy);
            up.append(&mut rest);
            up
        } else {
            cands
        };
        let movable = Movable {
            owner: Some(inst.refdes.clone()),
            candidates: cands.iter().map(|c| c.2).collect(),
        };
        (
            movable,
            Apply::Fields(i, cands.into_iter().map(|c| (c.0, c.1)).collect()),
        )
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
        let same = |p: Point2, q: Point2| (p[0] - q[0]).abs() < EPS && (p[1] - q[1]).abs() < EPS;
        // Candidate split points: every junction position + every wire endpoint.
        let mut pts: Vec<Point2> = self.junctions.iter().map(|j| j.at).collect();
        for w in &self.wires {
            pts.push(w.a);
            pts.push(w.b);
        }
        let mk = |a: Point2, b: Point2, net: Option<String>| Wire {
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
                let mut best: Option<Point2> = None;
                let mut best_d = f64::INFINITY;
                for &p in &pts {
                    if same(p, w.a) || same(p, w.b) || !Segment::new(w.a, w.b).contains_point(p) {
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
        use crate::label::{rotated_half_extents, text_width};
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
        if dx.abs() < EPS && dy.abs() < EPS {
            return;
        }
        self.translate(dx, dy);
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

    /// Mark `refdes` as preferring its Reference/Value fields ABOVE the body (see
    /// [`SchematicWriter::fields_above`]). Called by the floorplan engine for the
    /// low-side half-bridge FETs after `align_repeated_columns`, so their fields
    /// don't crowd the rotated SHUNT port label hanging below the source pin.
    pub fn prefer_fields_above(&mut self, refdes: &BTreeSet<String>) {
        self.fields_above.extend(refdes.iter().cloned());
    }

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
        use crate::label::{label_box, pin_text_boxes, rotated_half_extents, text_width};

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
                ]
                .into(),
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
            let owner = label.uuid_key.split(':').next().unwrap_or("").to_string();
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
                let off = pg.at.transform_offset(inst.angle, inst.mirror);
                let p = [inst.at[0] + off[0], inst.at[1] + off[1]];
                lo[0] = lo[0].min(p[0]);
                lo[1] = lo[1].min(p[1]);
                hi[0] = hi[0].max(p[0]);
                hi[1] = hi[1].max(p[1]);
            }
            let r = [
                lo[0] + BODY_INSET,
                lo[1] + BODY_INSET,
                hi[0] - BODY_INSET,
                hi[1] - BODY_INSET,
            ];
            if r[2] - r[0] < EPS || r[3] - r[1] < EPS {
                continue;
            }
            for wire in &self.wires {
                let (w1, w2) = (wire.a, wire.b);
                let cross = if (w1[0] - w2[0]).abs() < EPS {
                    let x = w1[0];
                    let (ylo, yhi) = (w1[1].min(w2[1]), w1[1].max(w2[1]));
                    r[0] + EPS < x && x < r[2] - EPS && ylo.max(r[1]) < yhi.min(r[3]) - EPS
                } else {
                    let y = w1[1];
                    let (xlo, xhi) = (w1[0].min(w2[0]), w1[0].max(w2[0]));
                    r[1] + EPS < y && y < r[3] - EPS && xlo.max(r[0]) < xhi.min(r[2]) - EPS
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
                let both_pin_text = items[i].3 == Kind::PinText && items[j].3 == Kind::PinText;
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
                if items[i].1.overlaps(&items[j].1) {
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
    use crate::write::Dir;
    use kicad_cli::env::KicadEnv;

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
    fn layout_lint_flags_overlapping_text() {
        let Some(env) = detect_env() else { return };
        let mut w = SchematicWriter::new();
        // Two symbols stacked nearly on top of each other -> collision.
        w.add_symbol(&env, "Device:R", "R1", "1k", [127.0, 63.5], 0.0)
            .unwrap();
        w.add_symbol(&env, "Device:R", "R2", "2k", [127.0, 64.77], 0.0)
            .unwrap();
        let warnings = w.layout_warnings();
        assert!(
            warnings
                .iter()
                .any(|s| s.contains("R1") && s.contains("R2")),
            "expected an R1/R2 overlap warning, got {warnings:?}"
        );

        // Far apart -> clean.
        let mut w = SchematicWriter::new();
        w.add_symbol(&env, "Device:R", "R1", "1k", [127.0, 63.5], 0.0)
            .unwrap();
        w.add_symbol(&env, "Device:R", "R2", "2k", [177.8, 63.5], 0.0)
            .unwrap();
        assert!(w.layout_warnings().is_empty());
    }

    #[test]
    fn lint_flags_text_on_pin_names() {
        let Some(env) = detect_env() else { return };
        let mut w = SchematicWriter::new();
        w.add_symbol(&env, "Timer:NE555P", "U1", "NE555P", [152.4, 101.6], 0.0)
            .unwrap();
        // A fixed cluster label parked on a west-side pin endpoint, reading
        // East: the text runs back across the pin line over the pin name.
        // (This is the legacy retracted-label shape the solver now avoids —
        // the lint must SEE it.)
        let (ep, _dir) = w.pin_dirs(&env, "U1", "2").unwrap()[0];
        w.add_cluster_label("X", ep, Dir::East, false);
        let warnings = w.layout_warnings();
        assert!(
            warnings
                .iter()
                .any(|s| s.contains("pin text") && s.contains("U1")),
            "expected a pin-text overlap warning, got {warnings:?}"
        );
    }

    #[test]
    fn lint_uses_rotated_body_extents() {
        let Some(env) = detect_env() else { return };
        // Two 90-degree resistors stacked vertically 10.16 apart: with angle-
        // blind extents (half-height 6.35) their boxes overlap; with rotated
        // extents (half-height 5.08) they exactly touch -> no overlap.
        let mut w = SchematicWriter::new();
        w.add_symbol(&env, "Device:R", "R1", "1k", [101.6, 101.6], 90.0)
            .unwrap();
        w.add_symbol(&env, "Device:R", "R2", "2k", [101.6, 111.76], 90.0)
            .unwrap();
        let warnings = w.layout_warnings();
        assert!(
            !warnings
                .iter()
                .any(|s| s.contains("symbol R1") && s.contains("symbol R2")),
            "rotated bodies must use rotated extents, got {warnings:?}"
        );
    }
}
