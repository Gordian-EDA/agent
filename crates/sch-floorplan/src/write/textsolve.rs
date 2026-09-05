//! The field/label placement solver and the geometry-finalizing passes.
//!
//! Everything here mutates the placed scene into its final, collision-resolved
//! form: stub retraction, wire splitting at taps, the greedy candidate solver
//! for movable text ([`SchematicWriter::solve_text_positions`]), reframing onto
//! the content-fit page, and the deterministic readability lint
//! ([`SchematicWriter::layout_warnings`]). All passes are idempotent so they may
//! run early (to lint final geometry) and again in `finish` harmlessly.

use std::collections::{BTreeMap, BTreeSet};

use geom::{Dir, EPS, GRID_50_MIL, Point2, Rect, Segment};

use super::{
    Anchor, Justify, SchematicWriter, TextPos, Wire, field_anchors, field_box, label_rect,
};

/// What [`SchematicWriter::solve_text_positions`] mutates once the greedy solver
/// has picked a candidate for the parallel [`sch_model::text::Movable`].
/// The reading directions a swivel label may take, best first: its emitted home
/// direction, then the reverse, then the two perpendiculars.
fn swivel_poses(home: Dir) -> [Dir; 4] {
    let (a, b) = match home {
        Dir::East | Dir::West => (Dir::North, Dir::South),
        Dir::North | Dir::South => (Dir::East, Dir::West),
    };
    [home, home.opposite(), a, b]
}

/// A grid-snapped point as an exact map key.
fn bits(p: Point2) -> (u64, u64) {
    let p = GRID_50_MIL.snap_point(p);
    (p[0].to_bits(), p[1].to_bits())
}

/// The stub every pin label rides out of its pin unless something is in the way —
/// long enough to read as a wire, short enough to keep the text beside its pin.
pub(crate) const DEFAULT_STUB_MM: f64 = 3.81;

/// How many rings of candidate seats a power-symbol rail name may try before the
/// solver falls back to burying it: the tip-and-two-sides ring, repeated at
/// one-grid-step increments outward from the glyph. Past this the name has
/// drifted far enough that the reader can no longer tell which glyph it names.
const RAIL_NAME_RINGS: u8 = 6;

/// The candidate that covers the least foreign ink, for a rail name no tier
/// could seat clear. Overlap AREA, not a count: clipping the corner of one pin
/// name still reads, sitting square on three of them does not.
fn least_buried(seats: &[(TextPos, Rect)], scene: &[sch_model::text::Obstacle]) -> usize {
    let buried = |b: &Rect| -> f64 {
        scene
            .iter()
            .filter_map(|o| b.intersection(&o.bbox))
            .map(|hit| hit.width() * hit.height())
            .sum()
    };
    (0..seats.len())
        .min_by(|&a, &b| buried(&seats[a].1).total_cmp(&buried(&seats[b].1)))
        .unwrap_or(0)
}

/// Sentinel "net" for no-connect anchors: a stub on a no-connect pin is still a
/// wrong attachment, so it counts as a foreign net.
const NC: &str = "\0no_connect";

/// How much room a symbol claims from solved text: the ink it draws, or the
/// padded cell `approx_size` reserves around it for placement.
#[derive(Clone, Copy)]
enum Bodies {
    Ink,
    Cell,
}

enum Apply {
    /// labels[i]: candidate 1 retracts onto the pin endpoint.
    StubLabel(usize),
    /// labels[i]: per-candidate reading direction, the anchor held fixed.
    SwivelLabel(usize, Vec<Dir>),
    /// instances[i]: per-candidate (Reference, Value) anchors.
    Fields(usize, Vec<(TextPos, TextPos)>),
}

impl SchematicWriter {
    /// Every fixed connection point on the sheet keyed to the net(s) that own it,
    /// and every wire segment tagged with the net it was drawn for.
    ///
    /// This is the one model of "what a label may not land on": power-symbol pin
    /// origins, no-connect markers, each label's own anchor (a stub label's PIN
    /// endpoint, not its retractable far end), wire endpoints, and the same from
    /// the sheet a block is being drawn beside. A touch on the SAME net is a
    /// deliberate join; a touch on another net is a short.
    fn anchor_model(
        &self,
    ) -> (
        BTreeMap<(u64, u64), std::collections::BTreeSet<String>>,
        Vec<sch_model::route::NetSegment>,
    ) {
        let mut points: BTreeMap<(u64, u64), std::collections::BTreeSet<String>> = BTreeMap::new();
        let mut add = |p: Point2, net: &str| {
            points.entry(bits(p)).or_default().insert(net.to_string());
        };
        for inst in &self.instances {
            if inst.lib_id.starts_with("power:") {
                add(inst.at, &inst.value);
            }
        }
        for nc in &self.no_connects {
            add(nc.at, NC);
        }
        for label in &self.labels {
            match label.anchor {
                Anchor::Stub(pin_at) => add(pin_at, &label.net),
                _ => add(label.at, &label.net),
            }
        }
        let mut segments: Vec<sch_model::route::NetSegment> = Vec::new();
        for w in &self.wires {
            segments.push(sch_model::route::NetSegment::new(w.a, w.b, w.net.clone()));
            add(w.a, &w.net);
            add(w.b, &w.net);
        }
        for (p, net) in &self.beside.points {
            add(*p, net);
        }
        segments.extend(self.beside.segments.iter().cloned());
        (points, segments)
    }

    /// Seat every stub label leaving one symbol on one side at a SINGLE stub
    /// length, so their text starts on one line: a connector's pin labels then
    /// read as a datasheet column rather than a ragged fringe.
    ///
    /// The router picks each label's stub independently — the shortest rung of
    /// its ladder that clears whatever that one pin faces — so a header's twenty
    /// labels come out at four different offsets. This pass finds the ONE length
    /// that seats the most of them clear of foreign anchors, foreign wires and
    /// neighbouring ink — the same tests [`Self::retract_colliding_stubs`] and
    /// [`Self::label_landing_clear`] apply, so an aligned stub is never one
    /// retraction then drops — and moves those onto it. A member no length can
    /// seat keeps what the router chose rather than dragging the column to it.
    ///
    /// Candidates are tried at [`DEFAULT_STUB_MM`] first and then outward-and-up,
    /// because the length is a *drawing* choice, not a clearance minimum: a
    /// column at the length everything else on the sheet uses is what makes the
    /// stubs read as one gesture. Only when nothing from the default up seats the
    /// whole group does it fall back to the shorter rungs.
    ///
    /// Idempotent: a second call re-derives the same group and re-picks the same
    /// length, which the members already sit at.
    fn align_stub_columns(&mut self) {
        let (points, segments) = self.anchor_model();
        let side = |dir: Dir| match dir {
            Dir::East => 0u8,
            Dir::West => 1,
            Dir::North => 2,
            Dir::South => 3,
        };
        let mut groups: BTreeMap<(String, u8), Vec<usize>> = BTreeMap::new();
        for (i, label) in self.labels.iter().enumerate() {
            if !matches!(label.anchor, Anchor::Stub(_)) {
                continue;
            }
            let Some(refdes) = label.uuid_key.split(':').next() else {
                continue;
            };
            groups
                .entry((refdes.to_string(), side(label.dir)))
                .or_default()
                .push(i);
        }
        let pitch = GRID_50_MIL.pitch();
        for ((refdes, _), members) in groups {
            if members.len() < 2 {
                continue;
            }
            let seat = |i: usize, len: f64| {
                let label = &self.labels[i];
                let Anchor::Stub(pin_at) = label.anchor else {
                    unreachable!("grouped on Anchor::Stub")
                };
                let v = label.dir.vec();
                let end =
                    GRID_50_MIL.snap_point(Point2::new(pin_at.x + v.x * len, pin_at.y + v.y * len));
                (pin_at, end)
            };
            let clear = |i: usize, len: f64| {
                let label = &self.labels[i];
                let (pin_at, end) = seat(i, len);
                let net = label.net.as_str();
                let foreign_at = |p: Point2| {
                    points
                        .get(&bits(p))
                        .is_some_and(|nets| nets.iter().any(|n| n.as_str() != net))
                };
                if foreign_at(end) {
                    return false;
                }
                if segments
                    .iter()
                    .any(|seg| seg.net != net && seg.segment.contains_point(end))
                {
                    return false;
                }
                let span = Segment::new(pin_at, end);
                if points.iter().any(|(&(xb, yb), nets)| {
                    let p = Point2::new(f64::from_bits(xb), f64::from_bits(yb));
                    nets.iter().any(|n| n.as_str() != net) && span.contains_point(p)
                }) {
                    return false;
                }
                self.label_landing_clear_excluding(end, label.dir, net, &refdes, &members)
            };
            // Rungs to try, best first: the default, then longer, then shorter. The
            // one that seats the MOST of the group wins, so a single pin with a
            // wire in its face costs that pin its column place rather than
            // dragging the other nineteen labels in with it.
            let longest = members
                .iter()
                .map(|&i| {
                    let (pin_at, _) = seat(i, 0.0);
                    (self.labels[i].at.x - pin_at.x).abs() + (self.labels[i].at.y - pin_at.y).abs()
                })
                .fold(0.0f64, f64::max)
                .max(11.0 * pitch);
            let rungs = (longest / pitch).round().max(1.0) as usize;
            let default = (DEFAULT_STUB_MM / pitch).round() as usize;
            let Some((_, shared, seats)) = (default..=rungs.max(default))
                .chain((1..default).rev())
                .enumerate()
                .map(|(rank, k)| {
                    let len = k as f64 * pitch;
                    let seats: Vec<usize> =
                        members.iter().copied().filter(|&i| clear(i, len)).collect();
                    (rank, len, seats)
                })
                // Ties keep the earlier — and therefore more preferred — rung.
                .max_by_key(|(rank, _, seats)| (seats.len(), std::cmp::Reverse(*rank)))
            else {
                continue;
            };
            // One member alone is not a column, and the router's own choice for it
            // is at least as good as anything this pass would pick.
            if seats.len() < 2 {
                continue;
            }
            let seated: Vec<(usize, Point2)> =
                seats.iter().map(|&i| (i, seat(i, shared).1)).collect();
            for (i, at) in seated {
                self.labels[i].at = at;
            }
        }
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
    /// The pass only processes labels with `stub.is_some()` and clears `stub` when
    /// the label retracts. A covered stub keeps its attachment metadata while
    /// junctions bind any interior pin/label anchors to the covering wire. Every
    /// emitted wire is registered on the stub's own net, so a second call is a
    /// no-op. This lets a caller run it early (e.g. to lint the post-retraction
    /// geometry) and have `finish` run it again harmlessly.
    ///
    /// **Foreign geometry** at pass start = every *fixed* connection point (power
    /// symbol pins — origin, net = the Value; no-connect markers — a reserved
    /// sentinel net; direct labels; and every signal stub's own pin endpoint,
    /// recorded as its own-net anchor) plus every existing wire **segment and
    /// endpoint**, under the net that wire was drawn for. A stub touching a wire
    /// of the *same* net is a deliberate join and survives; only a touch with a
    /// *different* net is foreign.
    ///
    /// Signal stubs are then walked in deterministic `uuid_key` order. A stub is
    /// **retracted** — its label snapped back onto its pin endpoint (keeping its
    /// outward orientation, so the text reads away from the body), no wire
    /// emitted — when either endpoint touches foreign geometry or its segment
    /// passes through a foreign point. A *surviving* stub registers its endpoint
    /// and segment as occupancy so a later differing-net stub cannot then collide
    /// with it. It replaces same-net route segments contained in its span, so the
    /// later wire splitter cannot expose their overlap as reversed duplicates.
    /// When the pin itself touches a foreign segment, retraction avoids adding
    /// geometry and the post-graft net audit refuses the inherently invalid
    /// placement.
    pub fn retract_colliding_stubs(&mut self) {
        let (mut points, mut segments) = self.anchor_model();
        let add_point =
            |p: Point2,
             net: &str,
             m: &mut BTreeMap<(u64, u64), std::collections::BTreeSet<String>>| {
                m.entry(bits(p)).or_default().insert(net.to_string());
            };

        // Deterministic processing order for stub labels.
        let mut order: Vec<usize> = (0..self.labels.len())
            .filter(|&i| matches!(self.labels[i].anchor, Anchor::Stub(_)))
            .collect();
        order.sort_by(|&a, &b| self.labels[a].uuid_key.cmp(&self.labels[b].uuid_key));

        for i in order {
            let net = self.labels[i].net.clone();
            let end = self.labels[i].at;
            let Anchor::Stub(pin_at) = self.labels[i].anchor else {
                unreachable!()
            };

            // Collision if either endpoint touches foreign geometry, or if the
            // stub segment passes through a foreign point. A pin on a foreign
            // segment must retract before covered-span junctions are emitted:
            // the label-on-pin fallback remains scoped by name, while a rendered
            // junction there would make the invalid placement harder to diagnose.
            // The post-graft net audit remains responsible for refusing a symbol
            // whose pin itself landed on that foreign segment.
            let end_on_point = points
                .get(&bits(end))
                .is_some_and(|nets| nets.iter().any(|n| *n != net));
            let end_on_seg = segments
                .iter()
                .any(|seg| seg.net != net && seg.segment.contains_point(end));
            let pin_on_foreign_seg = segments
                .iter()
                .any(|seg| seg.net != net && seg.segment.contains_point(pin_at));
            let seg_thru_point = points.iter().any(|(&(xb, yb), nets)| {
                let p = Point2::new(f64::from_bits(xb), f64::from_bits(yb));
                nets.iter().any(|n| *n != net) && Segment::new(pin_at, end).contains_point(p)
            });
            let covering = segments
                .iter()
                .find(|seg| {
                    seg.net == net
                        && seg.segment.contains_point(pin_at)
                        && seg.segment.contains_point(end)
                })
                .map(|seg| seg.segment);

            if end_on_point || end_on_seg || seg_thru_point || pin_on_foreign_seg {
                // Keep the outward dir: the text still reads away from the
                // body (an East reset would run a west-side pin's text back
                // across the pin line, over the pin name).
                self.labels[i].at = pin_at;
                self.labels[i].anchor = Anchor::Fixed;
            } else if let Some(covering) = covering {
                for at in [pin_at, end] {
                    let is_endpoint = at.near_eq(covering.a, EPS) || at.near_eq(covering.b, EPS);
                    if !is_endpoint {
                        self.add_junction_on_net(at, &net);
                    }
                }
                add_point(end, &net, &mut points);
            } else {
                // The stub wire is attributed to its own net so a re-run reads it
                // as a deliberate same-net join and does not retract every
                // survivor onto its pin.
                let stub_span = Segment::new(pin_at, end);
                self.wires.retain(|wire| {
                    wire.net != net
                        || !stub_span.contains_point(wire.a)
                        || !stub_span.contains_point(wire.b)
                });
                self.add_wire_on_net(pin_at, end, &net);
                add_point(end, &net, &mut points);
                segments.push(sch_model::route::NetSegment::new(pin_at, end, net));
            }
        }
    }

    /// Assign collision-free positions to all movable text via the greedy
    /// candidate solver [`crate::label::GreedyText`].
    ///
    /// Builds the obstacle scene ([`Self::build_obstacles`]), seats the power
    /// rail names first ([`Self::seat_rail_names`] — they outrank every other
    /// movable and are never hidden), then solves the rest in
    /// most-constrained-first order: stub signal labels
    /// ([`Self::stub_label_movables`]) ahead of the refdes-ordered field pass
    /// ([`Self::field_movables`]).
    ///
    /// Idempotent: every assignment is recomputed from scratch on each call
    /// (a retract-chosen label has no stub on the re-run and becomes a fixed
    /// obstacle at the same position), so reconcile may run it early to lint
    /// solved geometry and `finish`'s own call is a harmless re-run.
    pub fn solve_text_positions(&mut self) {
        use crate::label::GreedyText;
        use sch_model::text::TextSolver;

        let seated = self.seat_rail_names(&self.build_obstacles(Bodies::Ink));
        let mut obstacles = self.build_obstacles(Bodies::Cell);
        obstacles.extend(seated);

        let (mut movables, mut applies) = self.stub_label_movables();
        for (m, a) in [self.swivel_label_movables(), self.field_movables()] {
            movables.extend(m);
            applies.extend(a);
        }

        let picks = GreedyText.solve(&obstacles, &movables);
        for (
            apply,
            sch_model::text::Pick {
                candidate: pick, ..
            },
        ) in applies.into_iter().zip(picks)
        {
            match apply {
                Apply::StubLabel(i) => {
                    if pick == 1 {
                        let Anchor::Stub(pin_at) = self.labels[i].anchor else {
                            unreachable!()
                        };
                        let end = self.labels[i].at;
                        // Drop the now-unneeded stub wire retract_colliding_stubs
                        // materialized — but ONLY if its far end DANGLES. When the
                        // stub end is a routing junction (a bridge label on a pin
                        // the MST also wires, whose route `split_wires_at_nodes`
                        // fragmented at the stub end), deleting it severs the route
                        // and floats every downstream pin. A surviving stub is
                        // already same-net and foreign-clear, so keeping it is safe.
                        let a = GRID_50_MIL.snap_point(pin_at);
                        let b = GRID_50_MIL.snap_point(end);
                        let key = format!("{}:{}:{}:{}", a.x, a.y, b.x, b.y);
                        let end_is_junction = self
                            .wires
                            .iter()
                            .any(|w| w.uuid_key != key && (w.a == b || w.b == b));
                        if !end_is_junction {
                            self.wires.retain(|w| w.uuid_key != key);
                        }
                        self.labels[i].at = pin_at;
                        self.labels[i].anchor = Anchor::Fixed;
                    }
                }
                Apply::SwivelLabel(i, dirs) => self.labels[i].dir = dirs[pick],
                Apply::Fields(i, cands) => {
                    let (r, v) = cands[pick];
                    self.instances[i].ref_pos = Some(r);
                    self.instances[i].val_pos = Some(v);
                }
            }
        }
    }

    /// Everything solved text must avoid: symbol bodies (angle-aware, exempt
    /// for their own refdes), pin name/number text, wires, no-connect markers,
    /// and fixed (stub-less) labels.
    ///
    /// `bodies` picks how much room a symbol claims. A rail name belongs in the
    /// padding beside the part it serves, so it is solved against
    /// [`Bodies::Ink`] — what the symbol actually draws, and what
    /// [`crate::visual::measure`] scores. Field pairs and net labels are still
    /// held out of the whole placement cell ([`Bodies::Cell`]); the cell is a
    /// placement clearance rather than ink, so that is stricter than it needs to
    /// be, but relaxing it moves every field on every sheet and belongs to the
    /// lane that owns them.
    fn build_obstacles(&self, bodies: Bodies) -> Vec<sch_model::text::Obstacle> {
        use sch_model::text::{Obstacle, Owner, pin_text_boxes, wire_box};
        let mut obstacles: Vec<Obstacle> = Vec::new();
        for inst in &self.instances {
            let h = inst.half_extents.rotated_half_extents(inst.angle);
            obstacles.push(Obstacle {
                bbox: match bodies {
                    Bodies::Ink => crate::write::build::ink_box(inst),
                    Bodies::Cell => [
                        inst.at[0] - h[0],
                        inst.at[1] - h[1],
                        inst.at[0] + h[0],
                        inst.at[1] + h[1],
                    ]
                    .into(),
                },
                owner: Some(Owner::Symbol(inst.refdes.clone())),
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
                            owner: None,
                        });
                    }
                }
            }
        }
        for w in &self.wires {
            obstacles.push(Obstacle {
                bbox: wire_box(w.a, w.b),
                owner: Some(Owner::Net(w.net.clone())),
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
                owner: None,
            });
        }
        // Only fixed labels are obstacles; stub and swivel labels become movables.
        for l in &self.labels {
            if matches!(l.anchor, Anchor::Fixed) {
                obstacles.push(Obstacle {
                    bbox: label_rect(l, l.at, l.dir),
                    owner: None,
                });
            }
        }
        obstacles
    }

    /// Stub signal labels (most constrained, solved first), in deterministic
    /// uuid_key order. Each has two candidates: stay at the stub end, or retract
    /// onto the always-safe pin endpoint keeping the outward direction (the stub
    /// wire is dropped when retraction wins).
    fn stub_label_movables(&self) -> (Vec<sch_model::text::Movable>, Vec<Apply>) {
        use sch_model::text::Movable;
        let mut movables: Vec<Movable> = Vec::new();
        let mut applies: Vec<Apply> = Vec::new();
        let mut stub_idx: Vec<usize> = (0..self.labels.len())
            .filter(|&i| matches!(self.labels[i].anchor, Anchor::Stub(_)))
            .collect();
        stub_idx.sort_by(|&a, &b| self.labels[a].uuid_key.cmp(&self.labels[b].uuid_key));
        for &i in &stub_idx {
            let l = &self.labels[i];
            let Anchor::Stub(pin_at) = l.anchor else {
                unreachable!()
            };
            let owner = l.uuid_key.split(':').next().unwrap_or("").to_string();
            movables.push(Movable {
                owner: Some(sch_model::text::Owner::Symbol(owner)),
                candidates: vec![label_rect(l, l.at, l.dir), label_rect(l, pin_at, l.dir)],
            });
            applies.push(Apply::StubLabel(i));
        }
        (movables, applies)
    }

    /// Cluster/port labels, in deterministic uuid_key order. The tap point is
    /// part of the netlist and never moves; the reading direction is not, so the
    /// candidates are the four poses about that anchor — the emitted one first,
    /// then its reverse, then the two perpendiculars.
    ///
    /// Without this pass a port pentagon simply landed wherever its side said to
    /// read, which is how one came to be drawn straight over a neighbouring
    /// symbol's body. The label's OWN net's wires are exempt (the anchor sits on
    /// them by construction); a foreign wire still blocks, since a pentagon lying
    /// across one reads as a connection.
    fn swivel_label_movables(&self) -> (Vec<sch_model::text::Movable>, Vec<Apply>) {
        use sch_model::text::{Movable, Owner};
        let mut idx: Vec<usize> = (0..self.labels.len())
            .filter(|&i| matches!(self.labels[i].anchor, Anchor::Swivel(_)))
            .collect();
        idx.sort_by(|&a, &b| self.labels[a].uuid_key.cmp(&self.labels[b].uuid_key));
        idx.into_iter()
            .map(|i| {
                let l = &self.labels[i];
                let Anchor::Swivel(home) = l.anchor else {
                    unreachable!()
                };
                let dirs = swivel_poses(home);
                let movable = Movable {
                    owner: Some(Owner::Net(l.net.clone())),
                    candidates: dirs.iter().map(|&dir| label_rect(l, l.at, dir)).collect(),
                };
                (movable, Apply::SwivelLabel(i, dirs.to_vec()))
            })
            .unzip()
    }

    /// Reference+Value field pairs, in deterministic refdes order so the greedy
    /// solve order is stable. Power symbols carry no such pair — their rail name
    /// is seated ahead of everything by [`Self::seat_rail_names`].
    fn field_movables(&self) -> (Vec<sch_model::text::Movable>, Vec<Apply>) {
        let mut order: Vec<usize> = (0..self.instances.len())
            .filter(|&i| !self.instances[i].refdes.starts_with('#'))
            .collect();
        order.sort_by(|&a, &b| self.instances[a].refdes.cmp(&self.instances[b].refdes));
        order
            .into_iter()
            .map(|i| self.field_pair_movable(i))
            .unzip()
    }

    /// Seat every power-symbol rail name, ahead of all other movable text.
    ///
    /// A rail glyph with no name beside it is a power connection the reader
    /// cannot identify, so this text is not optional: it is never hidden, and it
    /// is solved FIRST — the boxes returned here become obstacles, so net labels
    /// and field text move around a rail name rather than the reverse.
    ///
    /// Acceptance relaxes in tiers, nearest spot first within each, until every
    /// name is seated. The tiers are exactly the obstacle classes, told apart by
    /// their [`sch_model::text::Owner`]:
    ///
    /// 1. clear of everything;
    /// 2. crossing a WIRE ([`Owner::Net`]) allowed — a name over a wire still reads;
    /// 3. crossing a BODY ([`Owner::Symbol`]) allowed too, leaving only unowned
    ///    ink — pin text, no-connects, fixed labels — to dodge. Two names merged
    ///    into one unreadable run is the artifact worth avoiding longest.
    ///
    /// A name that clears nothing even then takes its nearest candidate anyway.
    fn seat_rail_names(
        &mut self,
        obstacles: &[sch_model::text::Obstacle],
    ) -> Vec<sch_model::text::Obstacle> {
        use crate::label::GreedyText;
        use sch_model::text::{Obstacle, Owner, TextSolver};

        let mut order: Vec<usize> = (0..self.instances.len())
            .filter(|&i| {
                self.instances[i].refdes.starts_with('#')
                    && self.instances[i].lib_id != "power:PWR_FLAG"
            })
            .collect();
        order.sort_by(|&a, &b| self.instances[a].refdes.cmp(&self.instances[b].refdes));
        let seats: Vec<Vec<(TextPos, Rect)>> =
            order.iter().map(|&i| self.rail_name_seats(i)).collect();

        let tiers: [fn(&Owner) -> bool; 3] = [
            |_| true,
            |o| !matches!(o, Owner::Net(_)),
            |o| !matches!(o, Owner::Net(_) | Owner::Symbol(_)),
        ];
        let mut pending: Vec<usize> = (0..order.len()).collect();
        let mut placed: Vec<Obstacle> = Vec::new();
        for (tier, &keeps) in tiers.iter().enumerate() {
            if pending.is_empty() {
                break;
            }
            let scene: Vec<Obstacle> = obstacles
                .iter()
                .filter(|o| o.owner.as_ref().is_none_or(keeps))
                .chain(placed.iter())
                .cloned()
                .collect();
            let movables: Vec<sch_model::text::Movable> = pending
                .iter()
                .map(|&k| sch_model::text::Movable {
                    owner: Some(Owner::Symbol(self.instances[order[k]].refdes.clone())),
                    candidates: seats[k].iter().map(|c| c.1).collect(),
                })
                .collect();
            let picks = GreedyText.solve(&scene, &movables);
            let last = tier + 1 == tiers.len();
            let mut still = Vec::new();
            for (&k, pick) in pending.iter().zip(picks) {
                if !pick.fits && !last {
                    still.push(k);
                    continue;
                }
                // Out of tiers: no candidate clears the remaining ink, so take
                // the one that hides the least of it rather than the first.
                let candidate = match pick.fits {
                    true => pick.candidate,
                    false => least_buried(&seats[k], &scene),
                };
                let (pos, bbox) = seats[k][candidate];
                self.instances[order[k]].val_pos = Some(pos);
                placed.push(Obstacle { bbox, owner: None });
            }
            pending = still;
        }
        placed
    }

    /// Which way a power symbol's GLYPH points out of its anchor: the opposite
    /// of the direction its single pin connects.
    ///
    /// `power:GND` draws below its pin and `power:+3V3` above, both at instance
    /// angle 0 — the instance angle alone cannot tell them apart, and guessing
    /// from it seats half the rail names straight through the part they hang off.
    fn rail_glyph_dir(&self, inst: &super::Instance) -> Dir {
        use sch_model::geometry::quantize_dir;
        self.sym_pins
            .get(&inst.lib_id)
            .and_then(|pins| pins.first())
            .map(|pg| quantize_dir(pg.angle, inst.angle, inst.mirror).opposite())
            .unwrap_or(Dir::North)
    }

    /// Candidate seats for one power symbol's rail name, nearest first: beyond
    /// the glyph tip, then to either side of it, that ring repeated at two
    /// further removes so a glyph in a crowd still has somewhere legible to put
    /// its name.
    ///
    /// Offsets are measured from the DRAWN glyph — a stubby ~2.5 mm wedge on one
    /// side of the anchor — not from the symbol's placement cell, which
    /// `approx_size` floors to 10 mm square: seating off the cell strands the
    /// name 5 mm out in open space and leaves two rails a hand's width apart
    /// declaring each other blocked.
    ///
    /// Each candidate is the anchor the writer will emit, boxed by the model that
    /// measures what KiCAD then draws there — so a spot the solver approves is a
    /// spot the readability lint clears.
    fn rail_name_seats(&self, i: usize) -> Vec<(TextPos, Rect)> {
        let r2 = |v: f64| (v * 100.0).round() / 100.0;
        let inst = &self.instances[i];
        let seat = |at: [f64; 2], justify: Justify| {
            let pos = TextPos {
                at: [r2(at[0]), r2(at[1])],
                justify,
            };
            (pos, field_box(pos.at, justify, &inst.value))
        };
        let (cx, cy) = (inst.at[0], inst.at[1]);
        // The wedge reaches GLYPH_MM along its direction and half that across.
        const GLYPH_MM: f64 = 2.54;
        let out = self.rail_glyph_dir(inst);
        // KiCAD anchors text on its BASELINE, so a name below its glyph needs the
        // taller drop and one above it only the ascender's worth of clearance.
        let tip = |d: f64| match out {
            Dir::North => seat([cx, cy - GLYPH_MM - 0.64 - d], Justify::Center),
            Dir::South => seat([cx, cy + GLYPH_MM + 2.24 + d], Justify::Center),
            Dir::East => seat([cx + GLYPH_MM + 0.64 + d, cy + 0.8], Justify::Left),
            Dir::West => seat([cx - GLYPH_MM - 0.64 - d, cy + 0.8], Justify::Right),
        };
        let side = |d: f64| match out {
            Dir::North | Dir::South => [
                seat([cx + 1.27 + d, cy + 0.8], Justify::Left),
                seat([cx - 1.27 - d, cy + 0.8], Justify::Right),
            ],
            Dir::East | Dir::West => [
                seat([cx, cy - 1.27 - d - 0.64], Justify::Center),
                seat([cx, cy + 1.27 + d + 2.24], Justify::Center),
            ],
        };
        let mut cands = Vec::new();
        for ring in 0..RAIL_NAME_RINGS {
            let step = f64::from(ring) * GRID_50_MIL.pitch();
            cands.push(tip(step));
            cands.extend(side(step));
        }
        cands
    }

    /// Reference+Value field pair for a non-power instance: right / left / above
    /// / below of the body, with corner and far-band fallbacks for crowded
    /// symbols. Wide (rotated passive) bodies prefer above/below; ICs carry the
    /// pair on the horizontal band least overlapping their own pin text.
    fn field_pair_movable(&self, i: usize) -> (sch_model::text::Movable, Apply) {
        use sch_model::text::{Movable, pin_text_boxes};
        let r2 = |v: f64| (v * 100.0).round() / 100.0;
        let inst = &self.instances[i];
        let h = inst.half_extents.rotated_half_extents(inst.angle);
        let (cx, cy) = (inst.at[0], inst.at[1]);
        let (minx, miny, maxx, maxy) = (cx - h[0], cy - h[1], cx + h[0], cy + h[1]);
        // Each candidate is the pair of anchors the writer will emit, boxed by
        // the model that measures what KiCAD then draws there — so a spot the
        // solver approves is a spot the readability lint clears.
        let seat = |ref_at: [f64; 2], val_at: [f64; 2], justify: Justify| {
            let (rp, vp) = (
                TextPos {
                    at: [r2(ref_at[0]), r2(ref_at[1])],
                    justify,
                },
                TextPos {
                    at: [r2(val_at[0]), r2(val_at[1])],
                    justify,
                },
            );
            let (a, b) = (
                field_box(rp.at, justify, &inst.refdes),
                field_box(vp.at, justify, &inst.value),
            );
            let union = Rect::new(
                a.min_x.min(b.min_x),
                a.min_y.min(b.min_y),
                a.max_x.max(b.max_x),
                a.max_y.max(b.max_y),
            );
            (rp, vp, union)
        };
        let right = seat(
            [maxx + 1.27, cy - 1.27],
            [maxx + 1.27, cy + 1.27],
            Justify::Left,
        );
        let left = seat(
            [minx - 1.27, cy - 1.27],
            [minx - 1.27, cy + 1.27],
            Justify::Right,
        );
        let above = seat([cx, miny - 3.18], [cx, miny - 0.64], Justify::Center);
        let below = seat([cx, maxy + 2.24], [cx, maxy + 4.78], Justify::Center);
        // Corner fallbacks for crowded symbols (an IC whose four sides all
        // carry labels/power): the field pair tucks against a body corner.
        let above_left = seat([minx, miny - 3.18], [minx, miny - 0.64], Justify::Left);
        let above_right = seat([maxx, miny - 3.18], [maxx, miny - 0.64], Justify::Right);
        let below_left = seat([minx, maxy + 2.24], [minx, maxy + 4.78], Justify::Left);
        let below_right = seat([maxx, maxy + 2.24], [maxx, maxy + 4.78], Justify::Right);
        // Last-resort FAR bands (pushed ~5 mm further out): when a body is
        // ringed by packed neighbours — a tight decoupling cluster on a dense
        // board — every near spot is blocked and the solver would fall onto a
        // sibling's label/field. A far band clears it (the text reads a touch
        // detached but never overlaps). Appended LAST for both ICs and passives,
        // so a part with any near free spot is unaffected.
        let above_far = seat([cx, miny - 8.18], [cx, miny - 5.64], Justify::Center);
        let below_far = seat([cx, maxy + 5.64], [cx, maxy + 8.18], Justify::Center);
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
            let pin_boxes: Vec<Rect> = self
                .sym_pins
                .get(&inst.lib_id)
                .map(|pins| {
                    pins.iter()
                        .flat_map(|pg| pin_text_boxes(pg, inst.at, inst.angle, inst.mirror))
                        .collect()
                })
                .unwrap_or_default();
            let hits = |c: &(TextPos, TextPos, Rect)| {
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
            // IC side fields sit on the dense pin-name/number band and can look
            // valid to the approximate boxes while visibly smearing over pins.
            // Keep IC fields on horizontal bands only, with far bands as the
            // detached fallback.
            bands.push(above_far);
            bands.push(below_far);
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
        let movable = Movable {
            owner: Some(sch_model::text::Owner::Symbol(inst.refdes.clone())),
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
    /// at finalize.
    ///
    /// A wire splits only at a node of its OWN net. Splitting is what turns a
    /// touch into a connection, so splitting at a foreign node would be this pass
    /// welding two nets together — the last place a short can be manufactured
    /// after the router and the rails have cleared their geometry.
    fn split_wires_at_nodes(&mut self) {
        let same = |p: Point2, q: Point2| (p[0] - q[0]).abs() < EPS && (p[1] - q[1]).abs() < EPS;
        // Candidate split points, each tagged with the net that owns it: every
        // junction dot + every wire endpoint.
        let mut pts: Vec<(Point2, String)> = self
            .junctions
            .iter()
            .map(|j| (j.at, j.net.clone()))
            .collect();
        for w in &self.wires {
            pts.push((w.a, w.net.clone()));
            pts.push((w.b, w.net.clone()));
        }
        let mk = |a: Point2, b: Point2, net: String| Wire {
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
                for (p, net) in &pts {
                    let p = *p;
                    if *net != w.net
                        || same(p, w.a)
                        || same(p, w.b)
                        || !Segment::new(w.a, w.b).contains_point(p)
                    {
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

    /// Join two segments that only continue one another into the one wire they draw.
    ///
    /// The router lays a run down in pieces — a lead-out, a riser, a tap split — and
    /// the boundary between two pieces of a straight run is a seam a reader has to
    /// work out is not a branch. Where no CONDUCTOR lands on that boundary — no pin,
    /// no drawn dot, no third wire ending on or passing through it — the two pieces
    /// are one segment, and saying so takes the seam off the sheet. A seam something
    /// does meet is a real T and stays; the dots are decided before this runs, so a
    /// drawn dot is what says which.
    fn coalesce_collinear_runs(&mut self) {
        let key = super::build::point_key;
        let mut blocked: BTreeSet<(i64, i64)> = BTreeSet::new();
        blocked.extend(self.pin_points().iter().map(|p| key(*p)));
        blocked.extend(self.junctions.iter().filter(|j| j.dot).map(|j| key(j.at)));
        let beside = self.beside_wires();
        for (segment, _) in &beside {
            blocked.insert(key(segment.a));
            blocked.insert(key(segment.b));
        }
        while let Some((i, j, at)) = self.next_seam(&blocked, &beside) {
            let (a, b) = (self.wires[i].clone(), self.wires[j].clone());
            let far = |w: &Wire| if key(w.a) == key(at) { w.b } else { w.a };
            let (p, q) = (far(&a), far(&b));
            self.wires[i] = Wire {
                a: p,
                b: q,
                uuid_key: format!("{}:{}:{}:{}", p[0], p[1], q[0], q[1]),
                net: a.net.clone(),
            };
            self.wires.remove(j);
            // The recorded tap that split the run here is not a dot and no longer a
            // wire end; leaving it would have the next `prepare` split the run again.
            self.junctions
                .retain(|junction| key(junction.at) != key(at));
        }
    }

    /// The first seam [`Self::coalesce_collinear_runs`] may close: the two wires that
    /// meet there and the point, in the sheet's own order so the pass is deterministic.
    fn next_seam(
        &self,
        blocked: &BTreeSet<(i64, i64)>,
        beside: &[(Segment, String)],
    ) -> Option<(usize, usize, Point2)> {
        let key = super::build::point_key;
        let mut ends: BTreeMap<(i64, i64), (Point2, Vec<usize>)> = BTreeMap::new();
        for (i, w) in self.wires.iter().enumerate() {
            for end in [w.a, w.b] {
                ends.entry(key(end)).or_insert((end, Vec::new())).1.push(i);
            }
        }
        ends.into_iter().find_map(|(k, (at, meeting))| {
            let [i, j] = meeting[..] else { return None };
            if blocked.contains(&k) || self.wires[i].net != self.wires[j].net {
                return None;
            }
            let far = |w: &Wire| if key(w.a) == k { w.b } else { w.a };
            let (p, q) = (far(&self.wires[i]), far(&self.wires[j]));
            let (u, v) = ((p.x - at.x, p.y - at.y), (q.x - at.x, q.y - at.y));
            let straight = (u.0 * v.1 - u.1 * v.0).abs() < EPS && u.0 * v.0 + u.1 * v.1 < 0.0;
            let crossed = self
                .wires
                .iter()
                .enumerate()
                .filter(|(k, _)| *k != i && *k != j)
                .any(|(_, w)| Segment::new(w.a, w.b).contains_point(at))
                || beside.iter().any(|(segment, _)| segment.contains_point(at));
            (straight && !crossed).then_some((i, j, at))
        })
    }

    /// Decide which taps are DRAWN as junction dots, from the FINAL sheet geometry.
    ///
    /// KiCAD's rule, applied once everything that can meet at a point has been
    /// written: a point's degree counts one per wire END and per pin tip that lands
    /// on it, plus two for every wire whose INTERIOR it splits, and a dot belongs
    /// exactly where that reaches three. Two segments meeting at a bend, or a wire
    /// ending on a pin, are degree two — a dot there reads to an engineer as a
    /// branch that is not on the sheet. The router cannot decide this while it
    /// routes: its own tap splits and the label stubs land afterwards, so a bend
    /// looked like a join and a real three-way join looked like a bend.
    ///
    /// Candidates are the points something could END at — wire endpoints, pin tips,
    /// and the recorded taps. Two wires merely crossing mid-span is not one of them:
    /// KiCAD leaves such a crossing unconnected and so do we.
    fn place_junction_dots(&mut self) {
        let key = super::build::point_key;
        let mut degree: BTreeMap<(i64, i64), usize> = BTreeMap::new();
        let mut candidates: BTreeMap<(i64, i64), Point2> = BTreeMap::new();
        for w in &self.wires {
            for end in [w.a, w.b] {
                *degree.entry(key(end)).or_default() += 1;
                candidates.insert(key(end), end);
            }
        }
        for p in self.pin_points() {
            *degree.entry(key(p)).or_default() += 1;
            candidates.insert(key(p), p);
        }
        for j in &self.junctions {
            candidates.insert(key(j.at), j.at);
        }
        // The neighbouring sheet's wires are conductors too: a block whose route was
        // already covered by one draws no wire of its own and attaches to it instead.
        let beside = self.beside_wires();
        let ours = self
            .wires
            .iter()
            .map(|w| (Segment::new(w.a, w.b), w.net.clone()));
        let conductors: Vec<(Segment, String)> = ours.chain(beside.iter().cloned()).collect();
        for (seg, _) in &beside {
            for end in [seg.a, seg.b] {
                *degree.entry(key(end)).or_default() += 1;
            }
        }
        // Nets touching each candidate, and the two degrees an interior split adds.
        let interior = |seg: &Segment, p: Point2| {
            seg.contains_point(p)
                && (p[0] - seg.a[0]).abs() + (p[1] - seg.a[1]).abs() > EPS
                && (p[0] - seg.b[0]).abs() + (p[1] - seg.b[1]).abs() > EPS
        };
        let mut nets: BTreeMap<(i64, i64), std::collections::BTreeSet<&str>> = BTreeMap::new();
        for (k, p) in &candidates {
            for (seg, net) in &conductors {
                if !seg.contains_point(*p) {
                    continue;
                }
                nets.entry(*k).or_default().insert(net.as_str());
                if interior(seg, *p) {
                    *degree.entry(*k).or_default() += 2;
                }
            }
        }
        // A dot welds every conductor through it, so a point where a second net's wire
        // runs never gets one — the tap split already keeps each net whole there.
        let alone = |k: &(i64, i64)| nets.get(k).is_some_and(|n| n.len() == 1);
        let draws = |k: &(i64, i64)| degree.get(k).copied().unwrap_or(0) >= 3 && alone(k);
        for j in &mut self.junctions {
            // A tap landing inside a NEIGHBOUR's wire is an attachment: this writer
            // holds only its block, so the block terminal that meets the sheet there is
            // geometry it cannot see, and the join is real however few of its own
            // conductors show up.
            let attaches = beside
                .iter()
                .any(|(seg, net)| *net == j.net && interior(seg, j.at));
            let k = key(j.at);
            j.dot = draws(&k) || (attaches && alone(&k));
        }
        // A join no tap was recorded for still needs its dot: a wire ending on another
        // wire's interior reads as a crossing without one, which silently changes the
        // netlist an engineer reads off the sheet.
        let tapped: std::collections::BTreeSet<(i64, i64)> =
            self.junctions.iter().map(|j| key(j.at)).collect();
        for (k, p) in &candidates {
            if tapped.contains(k) || !draws(k) {
                continue;
            }
            self.junctions.push(super::Junction {
                at: *p,
                uuid_key: format!("{}:{}", p.x, p.y),
                net: nets[k].iter().next().unwrap().to_string(),
                dot: true,
            });
        }
    }

    /// Shift the whole drawing so its true minimum corner — including the rail
    /// power symbols, edge port labels, and solved field text that extend beyond
    /// the symbol bodies — lands at the page margin. The corner is
    /// [`Self::content_bbox`]'s, so the page the sheet is framed on and the page
    /// it is sized for are measured the same way. The floorplan's `normalize`
    /// only shifts symbol bodies, and it runs *before* wiring adds those edge
    /// elements, so a left/top port label can otherwise sit at a negative
    /// coordinate and be clipped off the content-fit page. Run last, after text is
    /// solved, so field positions move with their symbols.
    fn reframe(&mut self) {
        const M: f64 = 12.7;
        let Some(content) = self.content_bbox() else {
            return;
        };
        let (minx, miny) = (content.min_x, content.min_y);
        // Snap the shift to the grid: all wire/pin geometry is grid-aligned, so a
        // grid-multiple shift keeps it grid-aligned (KiCAD ERCs off-grid endpoints).
        // `minx`/`miny` include off-grid text extents, so an unsnapped shift would
        // knock the whole sheet off the 1.27 mm grid.
        let (dx, dy) = (
            geom::GRID_50_MIL.snap(M - minx),
            geom::GRID_50_MIL.snap(M - miny),
        );
        if dx.abs() < EPS && dy.abs() < EPS {
            return;
        }
        self.translate(dx, dy);
    }

    /// The rendered content bounding box over every drawn element — symbol bodies (rotated
    /// half-extents), reference/value fields, wires, labels (text width both ways), junctions,
    /// no-connects, texts and rects. Same geometry [`Self::reframe`] scans for its min corner,
    /// but BOTH corners, so a caller can size the rendered sheet AFTER `prepare` (e.g. to gate
    /// a placement on its true post-text-solve extent, edge label-columns included). `None`
    /// for an empty sheet.
    pub fn content_bbox(&self) -> Option<geom::Rect> {
        let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        let mut acc = |lx: f64, ly: f64, hx: f64, hy: f64| {
            x0 = x0.min(lx);
            y0 = y0.min(ly);
            x1 = x1.max(hx);
            y1 = y1.max(hy);
        };

        for i in &self.instances {
            let h = i.half_extents.rotated_half_extents(i.angle);
            acc(
                i.at[0] - h[0],
                i.at[1] - h[1],
                i.at[0] + h[0],
                i.at[1] + h[1],
            );
            // Fields are boxed exactly as they render: a long MPN value overhangs
            // a fixed allowance and then falls off the page.
            for (p, text) in [(i.ref_pos, &i.refdes), (i.val_pos, &i.value)] {
                let Some(p) = p else { continue };
                let b = super::field_box(p.at, p.justify, text);
                acc(b.min_x, b.min_y, b.max_x, b.max_y);
            }
        }
        for w in &self.wires {
            acc(
                w.a[0].min(w.b[0]),
                w.a[1].min(w.b[1]),
                w.a[0].max(w.b[0]),
                w.a[1].max(w.b[1]),
            );
        }
        for l in &self.labels {
            let b = label_rect(l, l.at, l.dir);
            acc(b.min_x, b.min_y, b.max_x, b.max_y);
        }
        for j in &self.junctions {
            acc(j.at[0], j.at[1], j.at[0], j.at[1]);
        }
        for nc in &self.no_connects {
            acc(nc.at[0], nc.at[1], nc.at[0], nc.at[1]);
        }
        for t in &self.texts {
            let b = super::sheet_text_box(t);
            acc(b.min_x, b.min_y, b.max_x, b.max_y);
        }
        for r in &self.rects {
            acc(
                r.start[0].min(r.end[0]),
                r.start[1].min(r.end[1]),
                r.start[0].max(r.end[0]),
                r.start[1].max(r.end[1]),
            );
        }
        (x0 != f64::MAX).then(|| geom::Rect::new(x0, y0, x1, y1))
    }

    /// Run every geometry-finalizing pass: stub retraction, wire splitting at
    /// taps, text placement, and reframing. All four are idempotent, so calling
    /// this before [`Self::layout_warnings`] (to lint the *final* geometry) and
    /// then [`Self::finish`] (which re-runs it harmlessly) is safe and is how
    /// `sch-floorplan` reports truthful, post-solve warnings.
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
        // Seat each symbol side's stub labels on one line before the collisions are
        // resolved, so retraction judges the geometry the sheet will actually show.
        self.align_stub_columns();
        // Resolve signal-stub collisions and materialize the surviving stub wires
        // before any rendering, so labels/wires below render the reconciled state.
        self.retract_colliding_stubs();
        // Split through-wires at their taps so every junction actually connects in
        // the netlist (KiCAD won't connect a mid-span tap on an unsplit wire).
        self.split_wires_at_nodes();
        // Only now, on geometry nothing else will move, decide which taps are dots.
        self.place_junction_dots();
        // A run the router drew in pieces is one wire. The dots are decided first
        // because a drawn dot is what says a seam is a real branch.
        self.coalesce_collinear_runs();
        // Then place movable text (fields, stub labels) collision-free against
        // the final geometry.
        self.solve_text_positions();
        // Finally reframe so nothing (edge port labels, rail symbols) is clipped
        // off the content-fit page (floorplan path only).
        if self.frame {
            self.reframe();
        }
    }

    /// Enable [`Self::reframe`] on the final geometry (whole-sheet emit only).
    pub fn set_frame(&mut self, on: bool) {
        self.frame = on;
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

    /// Would a net label anchored at `at` facing `dir` read CLEAR of every
    /// symbol body, pin text, field, and existing label?
    ///
    /// `own_refdes` exempts only the label's own BODY — a stub label legitimately
    /// hugs the pin it names, and the body's bbox is generous enough to swallow
    /// the pin tip. Its own symbol's PIN TEXT is not exempt: a net label printed
    /// over the pin number it is meant to explain is exactly what the lint
    /// reports, and the caller has a ladder of longer stubs to reach past it.
    ///
    /// Boxed by the one as-drawn model, like the lint it must agree with: a
    /// landing this approves never trips `layout_warnings`.
    pub fn label_landing_clear(
        &self,
        at: geom::Point2,
        dir: geom::Dir,
        net: &str,
        own_refdes: &str,
    ) -> bool {
        self.label_landing_clear_excluding(at, dir, net, own_refdes, &[])
    }

    /// [`Self::label_landing_clear`] blind to the labels at `moving` — the ones the
    /// caller is about to re-seat, whose current positions are not the geometry the
    /// landing has to live beside.
    fn label_landing_clear_excluding(
        &self,
        at: geom::Point2,
        dir: geom::Dir,
        net: &str,
        own_refdes: &str,
        moving: &[usize],
    ) -> bool {
        use sch_model::text::{label_box, pin_text_boxes};
        let b = label_box(at, dir, net);
        for inst in &self.instances {
            if inst.refdes.starts_with('#') {
                continue;
            }
            if inst.refdes != own_refdes {
                let h = inst.half_extents.rotated_half_extents(inst.angle);
                let body: Rect = [
                    inst.at[0] - h[0],
                    inst.at[1] - h[1],
                    inst.at[0] + h[0],
                    inst.at[1] + h[1],
                ]
                .into();
                if body.overlaps(&b) {
                    return false;
                }
            }
            if let Some(pins) = self.sym_pins.get(&inst.lib_id) {
                for pg in pins {
                    for pb in pin_text_boxes(pg, inst.at, inst.angle, inst.mirror) {
                        if pb.overlaps(&b) {
                            return false;
                        }
                    }
                }
            }
        }
        for (i, label) in self.labels.iter().enumerate() {
            if label.net == net || moving.contains(&i) {
                continue;
            }
            let lb = label_rect(label, label.at, label.dir);
            if lb.overlaps(&b) {
                return false;
            }
        }
        true
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
        use sch_model::text::pin_text_boxes;

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
        let mut items: Vec<(String, Rect, String, Kind)> = Vec::new();
        for inst in &self.instances {
            if inst.refdes.starts_with('#') {
                // Power/flag graphics are exempt as bodies (they legitimately
                // touch the pins they serve), but their visible Value text
                // (the rail name) must not collide with anything: adjacent
                // rails merging their names is a real artifact class.
                if inst.lib_id != "power:PWR_FLAG" {
                    let (_, vp) = field_anchors(inst);
                    items.push((
                        format!("value \"{}\" of {}", inst.value, inst.refdes),
                        field_box(vp.at, vp.justify, &inst.value),
                        inst.refdes.clone(),
                        Kind::Text,
                    ));
                }
                continue;
            }
            items.push((
                format!("symbol {}", inst.refdes),
                crate::write::build::ink_box(inst),
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
                field_box(rp.at, rp.justify, &inst.refdes),
                inst.refdes.clone(),
                Kind::Text,
            ));
            items.push((
                format!("value \"{}\" of {}", inst.value, inst.refdes),
                field_box(vp.at, vp.justify, &inst.value),
                inst.refdes.clone(),
                Kind::Text,
            ));
        }
        for label in &self.labels {
            let b = label_rect(label, label.at, label.dir);
            let owner = label.uuid_key.split(':').next().unwrap_or("").to_string();
            items.push((
                format!("label \"{}\" at {:?}", label.net, label.at),
                b,
                owner,
                Kind::Text,
            ));
        }
        // Block captions (title/note) are decoration the realiser fully controls, so a
        // caption over the drawing is a bug it can always avoid — lint it like any
        // other text. Each owns a unique key so no exemption ever applies to it.
        for t in &self.texts {
            items.push((
                format!("caption {:?}", t.text.lines().next().unwrap_or("")),
                super::sheet_text_box(t),
                format!("\0caption:{}", t.uuid_key),
                Kind::Text,
            ));
        }
        let mut warnings = Vec::new();
        // A caption over a wire is never legitimate — unlike a pin's own text, no
        // wire has any business under one — so it is linted where other text is not.
        for t in &self.texts {
            let b = super::sheet_text_box(t);
            if self
                .wires
                .iter()
                .any(|w| b.overlaps(&sch_model::text::wire_box(w.a, w.b)))
            {
                warnings.push(format!(
                    "caption {:?} crosses a wire",
                    t.text.lines().next().unwrap_or("")
                ));
            }
        }
        // Wire through an IC body: a wire segment running strictly inside a chip's
        // package box (the pin-tip bbox shrunk past the pin stubs onto the body
        // rectangle — the same geometry `count_ic_body_crossings` measures). This reads
        // as a connection straight through the chip — the defect the eye most often
        // misses — and an authored `layout:` tree can still produce it (a rigid grid
        // forcing a part to the far side of its anchor), so it must LINT here too, not
        // just be counted.
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
    use crate::write::{Anchor, Dir, Instance, PinLabel};
    use geom::Point2;
    use kicad::KicadInstallation;
    use kicad_symbol::geometry::PinGeom;

    /// KiCAD's junction rule on finished geometry: a corner joins nothing, a tap
    /// on a trunk and a wire landing on another wire's interior each join three.
    #[test]
    fn dots_only_where_three_conductors_meet() {
        let dots = |w: &mut SchematicWriter| {
            w.prepare();
            w.junction_positions()
        };

        let mut bend = SchematicWriter::new();
        bend.add_wire_on_net([25.4, 25.4], [50.8, 25.4], "N");
        bend.add_wire_on_net([50.8, 25.4], [50.8, 50.8], "N");
        assert!(dots(&mut bend).is_empty(), "an L-bend is not a join");

        let mut tee = SchematicWriter::new();
        tee.add_wire_on_net([25.4, 25.4], [50.8, 25.4], "N");
        tee.add_wire_on_net([50.8, 25.4], [76.2, 25.4], "N");
        tee.add_wire_on_net([50.8, 25.4], [50.8, 50.8], "N");
        assert_eq!(dots(&mut tee), vec![[50.8, 25.4]]);

        // The end lands mid-trunk: the tap splits the trunk and the dot says so.
        let mut tap = SchematicWriter::new();
        tap.add_wire_on_net([25.4, 25.4], [76.2, 25.4], "N");
        tap.add_wire_on_net([50.8, 25.4], [50.8, 50.8], "N");
        assert_eq!(dots(&mut tap), vec![[50.8, 25.4]]);
    }

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

    #[test]
    fn ic_far_bands_precede_side_fields() {
        let mut w = SchematicWriter::new();
        w.instances.push(Instance {
            lib_id: "Test:IC".into(),
            refdes: "U1".into(),
            value: "STM32F103C8Tx".into(),
            footprint: None,
            at: Point2::new(100.0, 100.0),
            angle: 0.0,
            mirror: false,
            extra_props: Vec::new(),
            uuid: None,
            half_extents: Point2::new(10.0, 20.0),
            ref_pos: None,
            val_pos: None,
            unit: 1,
        });
        w.sym_pins.insert(
            "Test:IC".into(),
            vec![
                PinGeom {
                    number: "1".into(),
                    name: "LEFT".into(),
                    at: Point2::new(-10.0, 0.0),
                    angle: 0.0,
                    length: 2.54,
                    unit: 1,
                    text: Default::default(),
                },
                PinGeom {
                    number: "2".into(),
                    name: "RIGHT".into(),
                    at: Point2::new(10.0, 0.0),
                    angle: 180.0,
                    length: 2.54,
                    unit: 1,
                    text: Default::default(),
                },
                PinGeom {
                    number: "3".into(),
                    name: "TOP".into(),
                    at: Point2::new(0.0, 20.0),
                    angle: 270.0,
                    length: 2.54,
                    unit: 1,
                    text: Default::default(),
                },
            ],
        );

        let (movable, _) = w.field_pair_movable(0);
        let first_after_near_bands = movable.candidates[6];

        assert!(
            first_after_near_bands.max_y < 80.0 || first_after_near_bands.min_y > 120.0,
            "ICs should try far above/below bands before side fields; got {first_after_near_bands:?}"
        );
        // The body spans y 80..120; a field band may graze its edge by the
        // stroke the glyphs are painted with, but never ride the pin text.
        assert!(
            movable
                .candidates
                .iter()
                .all(|c| c.max_y < 80.1 || c.min_y > 119.9),
            "IC fields should never use side bands over pin text: {:?}",
            movable.candidates
        );
    }

    /// A rail name reads on the far side of its glyph, whichever way that glyph
    /// points — `power:GND` draws below its pin and `power:+3V3` above, both at
    /// instance angle 0, so the seat cannot be read off the instance angle.
    #[test]
    fn rail_names_seat_beyond_the_glyph() {
        let Some(env) = detect_env() else { return };
        let mut w = SchematicWriter::new();
        w.add_symbol(&env, "power:GND", "#PWR_GND_0", "GND", [127.0, 63.5], 0.0)
            .unwrap();
        w.add_symbol(&env, "power:+3V3", "#PWR_3V3_0", "+3V3", [190.5, 63.5], 0.0)
            .unwrap();
        w.solve_text_positions();

        let gnd = w.instances[0].val_pos.expect("GND name seated");
        assert!(
            gnd.at[1] > 63.5,
            "a ground name reads BELOW the bar: {:?}",
            gnd.at
        );
        let rail = w.instances[1].val_pos.expect("+3V3 name seated");
        assert!(
            rail.at[1] < 63.5,
            "an up-arrow rail name reads ABOVE the glyph: {:?}",
            rail.at
        );
    }

    /// Rail names crowded onto one pitch still ALL get drawn: the seat degrades
    /// outward, but a glyph is never left unnamed.
    #[test]
    fn crowded_rail_names_are_all_seated() {
        let Some(env) = detect_env() else { return };
        let mut w = SchematicWriter::new();
        for i in 0..8 {
            let x = 127.0 + f64::from(i) * 2.54;
            w.add_symbol(
                &env,
                "power:GND",
                &format!("#PWR_GND_{i}"),
                "GND",
                [x, 63.5],
                0.0,
            )
            .unwrap();
        }
        w.solve_text_positions();
        assert!(
            w.instances.iter().all(|i| i.val_pos.is_some()),
            "every crowded rail keeps a seated name"
        );
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
    fn same_net_route_prefix_is_replaced_by_label_stub() {
        let mut w = SchematicWriter::new();
        let pin_at = Point2::new(10.16, 10.16);
        let label_at = Point2::new(13.97, 10.16);
        w.labels.push(PinLabel {
            net: "SIG".into(),
            at: label_at,
            uuid_key: "U1:1:SIG:0".into(),
            dir: Dir::East,
            anchor: Anchor::Stub(pin_at),
        });
        w.add_wire_on_net([11.43, 10.16], pin_at, "SIG");
        w.add_wire_on_net([11.43, 8.89], [11.43, 10.16], "SIG");

        w.prepare();

        let once: Vec<_> = w
            .wires
            .iter()
            .map(|wire| (wire.uuid_key.clone(), wire.net.clone()))
            .collect();
        w.prepare();

        let mut segments = std::collections::BTreeSet::new();
        for wire in &w.wires {
            let a = crate::write::point_key(wire.a);
            let b = crate::write::point_key(wire.b);
            assert!(segments.insert(if a <= b { (a, b) } else { (b, a) }));
        }
        assert_eq!(w.wires.len(), 3);
        assert!(matches!(w.labels[0].anchor, Anchor::Stub(_)));
        assert_eq!(w.labels[0].at, label_at);
        assert_eq!(
            w.wires
                .iter()
                .map(|wire| (wire.uuid_key.clone(), wire.net.clone()))
                .collect::<Vec<_>>(),
            once
        );
    }

    #[test]
    fn same_net_through_wire_attaches_to_stub_pin() {
        let Some(env) = detect_env() else { return };
        let mut w = SchematicWriter::new();
        w.add_symbol(&env, "Device:R", "R1", "1k", [127.0, 63.5], 0.0)
            .unwrap();
        w.add_signal_label(&env, "R1", "1", "SIG").unwrap();
        w.add_wire_on_net([127.0, 54.61], [127.0, 60.96], "SIG");

        let doc = sch_doc::SchDoc::parse(&w.finish()).unwrap();
        let netlist = sch_doc::connect::extract(&doc);

        assert!(netlist.nets.iter().any(|net| {
            net.name == "SIG"
                && net
                    .pins
                    .iter()
                    .any(|pin| pin.refdes == "R1" && pin.pin == "1")
        }));
    }

    #[test]
    fn covered_stub_pin_crossing_foreign_wire_retracts_without_geometry() {
        let Some(env) = detect_env() else { return };
        let mut existing = SchematicWriter::new();
        existing.add_wire_on_net([127.0, 54.61], [127.0, 60.96], "SIG");
        existing.add_cluster_label("SIG", [127.0, 54.61], Dir::South);
        existing.add_wire_on_net([121.92, 59.69], [132.08, 59.69], "OTHER");
        existing.add_cluster_label("OTHER", [121.92, 59.69], Dir::East);
        let beside = existing.route_scene();

        let mut added = SchematicWriter::new();
        added.set_beside(beside);
        added
            .add_symbol(&env, "Device:R", "R1", "1k", [127.0, 63.5], 0.0)
            .unwrap();
        added.add_signal_label(&env, "R1", "1", "SIG").unwrap();
        added.prepare();

        assert!(added.labels.iter().any(|label| {
            label.net == "SIG"
                && label.at.near_eq(Point2::new(127.0, 59.69), EPS)
                && matches!(label.anchor, Anchor::Fixed)
        }));
        assert!(
            added
                .junctions
                .iter()
                .all(|junction| !junction.at.near_eq(Point2::new(127.0, 59.69), EPS))
        );
        assert!(added.wires.is_empty());
    }

    #[test]
    fn lint_flags_text_on_pin_names() {
        let Some(env) = detect_env() else { return };
        let mut w = SchematicWriter::new();
        w.add_symbol(&env, "Timer:NE555P", "U1", "NE555P", [152.4, 101.6], 0.0)
            .unwrap();
        // A fixed cluster label parked on a west-side pin endpoint, reading
        // East: the text runs back across the pin line over the pin name.
        // (This is the fallback retracted-label shape the solver now avoids —
        // the lint must SEE it.)
        let (ep, _dir) = w.pin_dirs(&env, "U1", "2").unwrap()[0];
        w.add_cluster_label("X", ep, Dir::East);
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
