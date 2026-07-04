//! Module formation: each anchor (IC / connector / multi-pin part) plus the
//! chains that decorate it, typeset the way a human draws "the chip and its
//! passives" before wiring chips together. The organizing concept is the PIN
//! LADDER: every chain incident to an anchor pin's net (directly or through its
//! junction) lives in that pin's column — supply legs stack upward, ground legs
//! downward, and junction↔junction bridges run vertically between their two
//! pins (a 555's DIS→R→THR, a regulator's FB divider).
//!
//! Corpus laws applied (medians over 500 professional sheets): legs vertical at
//! the pin column; decoupling caps banked beside the anchor, supply pin up;
//! power glyphs at pins (realizer-drawn); 1.27 mm grid; anchors at rotation 0.

use std::collections::{BTreeMap, BTreeSet};

use geom::Point2;
use sch_place::ir::Orient;
use sch_place::item::{Incidence, Item};
use sch_place::netclass::{PinSide, is_connector_like as is_connector_like_part};

use crate::chain::{ChainRole, NodeKind, Reduced};
use crate::net::NetClass;

const GRID: f64 = 1.27;
/// Lead between an anchor pin endpoint and the first satellite pin (2 grid).
const LEAD: f64 = 2.54;
/// Column pitch when several legs fan off the same region.
const PITCH: f64 = 5.08;

fn snap(v: f64) -> f64 {
    (v / GRID).round() * GRID
}

/// One satellite item placed relative to its module origin (the anchor's `at`).
#[derive(Debug, Clone)]
pub struct SatPlace {
    pub item: usize,
    pub offset: Point2,
    pub angle: f64,
}

/// A typeset module: the anchor plus satellites, with a content envelope
/// relative to the anchor origin.
#[derive(Debug)]
pub struct ModulePlan {
    pub anchor: usize,
    pub sats: Vec<SatPlace>,
    /// Envelope (min, max) around anchor origin, satellites included.
    pub env_min: Point2,
    pub env_max: Point2,
}

/// Which chains were consumed into modules; the rest are spine content.
#[derive(Debug, Default)]
pub struct ModuleForm {
    pub modules: Vec<ModulePlan>,
    /// chain index → consumed by module (index into `modules`).
    pub consumed: BTreeMap<usize, usize>,
    /// Stub modules (2-pin islets nothing adopted): the arrange stage collects
    /// these into the dedicated strap column.
    pub strap_items: BTreeSet<usize>,
}

/// Orientation angle for a 2-pin `item` so that the pin on `first_net` points
/// `dir` (chain order decides which pin leads).
pub(crate) fn orient_for(item: &Item, first_net: &str, dir: Orient) -> f64 {
    let pin1_first = item
        .pins
        .iter()
        .find(|(_, _, n)| n.is_some())
        .is_some_and(|(_, _, n)| n.as_deref() == Some(first_net));
    let want = if pin1_first {
        dir
    } else {
        match dir {
            Orient::Up => Orient::Down,
            Orient::Down => Orient::Up,
            Orient::Left => Orient::Right,
            Orient::Right => Orient::Left,
        }
    };
    sch_floorplan::contract::orient_angle(&item.geom, want)
}

/// World offset of `pin` (by number) for an item rotated to `angle`, relative
/// to the item origin (sheet coords: +y down).
pub(crate) fn pin_offset(item: &Item, number: &str, angle: f64) -> Point2 {
    let Some(p) = item.geom.pins.iter().find(|p| p.number == number) else {
        return Point2::new(0.0, 0.0);
    };
    let off = p.at.transform_offset(angle, false);
    Point2::new(off[0], off[1])
}

/// The pin (number) of `item` lying on `net`.
fn pin_on(item: &Item, net: &str) -> Option<String> {
    item.pins
        .iter()
        .find(|(_, _, n)| n.as_deref() == Some(net))
        .map(|(num, _, _)| num.clone())
}

/// Approximate half-extents of an item's body for envelope math.
pub(crate) fn half_size(item: &Item, angle: f64) -> Point2 {
    let s = item.geom.approx_size();
    if (angle / 90.0).round() as i64 % 2 == 1 {
        Point2::new(s[1] / 2.0, s[0] / 2.0)
    } else {
        Point2::new(s[0] / 2.0, s[1] / 2.0)
    }
}

/// The FULL placement rect of a satellite at its offset — body plus the
/// side-mounted refdes/value text, exactly as the overlap wall measures it
/// (`contract::item_rect`), so envelopes never under-reserve.
pub(crate) fn placed_rect(items: &[Item], s: &SatPlace) -> geom::Rect {
    let mut tmp = items[s.item].clone();
    tmp.angle = s.angle;
    sch_floorplan::contract::item_rect(&tmp, [s.offset.x, s.offset.y])
}

/// How one consumed chain hangs off its module.
enum Attach {
    /// Vertical leg in the pin's column: `up` toward a supply, else down to ground.
    /// `sig_net` is the signal-side net (the junction the leg hangs from).
    Ladder { chain: usize, anchor: usize, pin: String, a_near: bool, up: bool, sig_net: String },
    /// Vertical run between two pins of the SAME anchor (chain `a` end at `pin_a`).
    Bridge { chain: usize, anchor: usize, pin_a: String, pin_b: String },
    /// One-part series chain wired at ONE anchor pin; the other end is free
    /// (labeled). The part sits beside the pin instead of floating as a
    /// two-label islet. `a_near` = chain terminal `a` is the anchor end.
    /// `via_junction` = the near side resolved THROUGH a junction (its net):
    /// that junction's shunt legs defer and hang from the tail's wire.
    Tail { chain: usize, anchor: usize, pin: String, a_near: bool, via_junction: Option<String> },
}

/// The vertical span of a chain part along a leg, entry pin to exit pin.
fn part_span(item: &Item, angle: f64, entry: &str, exit: &str) -> f64 {
    let e = pin_offset(item, entry, angle);
    let x = pin_offset(item, exit, angle);
    (x.y - e.y).abs()
}

/// Typeset all modules. `anchors` are the Part-node item indices in module-seed
/// order (callers pass most-connected-first).
pub fn form_modules(
    items: &[Item],
    inc: &Incidence,
    classes: &BTreeMap<String, NetClass>,
    g: &Reduced,
    anchors: &[usize],
    labeled: Option<&BTreeSet<String>>,
    port_nets: &BTreeSet<String>,
) -> ModuleForm {
    let mut form = ModuleForm::default();
    let class = |net: &str| *classes.get(net).unwrap_or(&NetClass::Signal);

    // Stub adoption: a 0-part chain from a big anchor's pin straight to a small
    // 2-pin part (single-connected LED, pull resistor) — the part belongs
    // BESIDE that pin, wired, not floating behind a label.
    let mut adopts: Vec<(usize, usize, String, String)> = Vec::new(); // (stub, anchor, anchor_pin, net)
    for c in g.chains.iter() {
        if !c.parts.is_empty() {
            continue;
        }
        let (pa, pb) = match (&g.nodes[c.a.node], &g.nodes[c.b.node]) {
            (NodeKind::Part(x), NodeKind::Part(y)) => (*x, *y),
            _ => continue,
        };
        let small = |i: usize| {
            items[i].geom.pins.len() <= 2 && !is_connector_like_part(&items[i].part)
        };
        // Connectors never adopt: their flanks are label columns by convention.
        let big = |i: usize| {
            items[i].geom.pins.len() >= 3 && !is_connector_like_part(&items[i].part)
        };
        if small(pa) && big(pb) {
            adopts.push((pa, pb, c.b.pin.clone(), c.b.net.clone()));
        } else if small(pb) && big(pa) {
            adopts.push((pb, pa, c.a.pin.clone(), c.a.net.clone()));
        }
    }

    // Adopted stubs leave the anchor set entirely: they are satellites now, and
    // any resolver that still treated them as anchors would double-place them.
    let adopted_set: BTreeSet<usize> = adopts.iter().map(|(st, _, _, _)| *st).collect();
    let anchors_vec: Vec<usize> = anchors
        .iter()
        .copied()
        .filter(|a| !adopted_set.contains(a))
        .collect();
    let anchors = &anchors_vec[..];


    // ── Resolve a chain terminal to an anchor pin, directly or via junction.
    // Junction choices are memoized so every chain on one junction shares a
    // column; the preferred pin side order is E, S, W, N (flow reads rightward).
    let mut junction_pin: BTreeMap<String, Option<(usize, String)>> = BTreeMap::new();
    let side_rank = |s: PinSide| match s {
        PinSide::East => 0,
        PinSide::South => 1,
        PinSide::West => 2,
        PinSide::North => 3,
    };
    // A pin's body side from its ANGLE (KiCAD pins point INTO the body: a
    // left-side pin has angle 0/east). Position-based classification misreads
    // tall symbols (an MCU's top-left pin looks "North" by magnitudes).
    let pin_side_of = |a: usize, num: &str| {
        items[a]
            .geom
            .pins
            .iter()
            .find(|p| p.number == num)
            .map(|p| match p.angle.rem_euclid(360.0) as i64 {
                0 => PinSide::West,
                180 => PinSide::East,
                90 => PinSide::South,
                _ => PinSide::North,
            })
            .unwrap_or(PinSide::East)
    };
    let mut resolve = |t: &crate::chain::Terminal| -> Option<(usize, String)> {
        match &g.nodes[t.node] {
            NodeKind::Part(i) if anchors.contains(i) => Some((*i, t.pin.clone())),
            NodeKind::Junction(net) => junction_pin
                .entry(net.clone())
                .or_insert_with(|| {
                    anchors
                        .iter()
                        .flat_map(|&a| {
                            items[a]
                                .pins
                                .iter()
                                .filter(|(_, _, n)| n.as_deref() == Some(net.as_str()))
                                .map(move |(num, _, _)| (a, num.clone()))
                        })
                        .min_by_key(|(a, num)| {
                            (
                                anchors.iter().position(|x| x == a).unwrap_or(usize::MAX),
                                side_rank(pin_side_of(*a, num)),
                            )
                        })
                })
                .clone(),
            _ => None,
        }
    };

    // ── Classify chains into attachments.
    let mut attach: Vec<Attach> = Vec::new();
    let mut decouple: Vec<usize> = Vec::new();
    for (ci, c) in g.chains.iter().enumerate() {
        if c.parts.is_empty() {
            continue;
        }
        match c.role(classes) {
            ChainRole::RailToRail => {
                let grounds = [&c.a.net, &c.b.net]
                    .iter()
                    .filter(|n| class(n) == NetClass::Ground)
                    .count();
                if c.parts.len() == 1 && grounds == 1 {
                    decouple.push(ci);
                }
                // Multi-part rail ladders without a sensed tap stay spine content.
            }
            ChainRole::ShuntLeg => {
                let (sig_t, a_near, rail_net) = if class(&c.a.net).is_rail() {
                    (&c.b, false, &c.a.net)
                } else {
                    (&c.a, true, &c.b.net)
                };
                if let Some((anchor, pin)) = resolve(sig_t) {
                    let up = class(rail_net) == NetClass::Supply;
                    // A series-inductive part (L*, FB*) to a SUPPLY at an E/W
                    // pin draws IN-LINE with the rail arrow at its far end
                    // (the buck's SW→L→rail row) — a vertical leg jams the
                    // flank the switching pins need.
                    let inductive = c.parts.len() == 1
                        && (items[c.parts[0]].refdes.starts_with('L')
                            || items[c.parts[0]].refdes.starts_with("FB"))
                        && up
                        && matches!(pin_side_of(anchor, &pin), PinSide::East | PinSide::West);
                    if inductive {
                        let via = matches!(&g.nodes[sig_t.node], NodeKind::Junction(_))
                            .then(|| sig_t.net.clone());
                        attach.push(Attach::Tail {
                            chain: ci,
                            anchor,
                            pin,
                            a_near,
                            via_junction: via,
                        });
                    } else {
                        attach.push(Attach::Ladder {
                            chain: ci,
                            anchor,
                            pin,
                            a_near,
                            up,
                            sig_net: sig_t.net.clone(),
                        });
                    }
                }
            }
            ChainRole::Series => {
                let (ra, rb) = (resolve(&c.a), resolve(&c.b));
                match (ra, rb) {
                    (Some((ia, pa)), Some((ib, pb))) if ia == ib && pa != pb => {
                        // Bridges wrap SAME-side pairs (vertical stack) or
                        // opposite E/W pairs (horizontal above). A MIXED pair
                        // (north IN to west EN) would stack across the body's
                        // corner into neighbouring wiring — hang a TAIL at the
                        // first pin instead, label at the second.
                        let (sa, sb) = (pin_side_of(ia, &pa), pin_side_of(ib, &pb));
                        let ew = |s: PinSide| matches!(s, PinSide::East | PinSide::West);
                        if sa == sb || (ew(sa) && ew(sb)) {
                            attach.push(Attach::Bridge {
                                chain: ci,
                                anchor: ia,
                                pin_a: pa,
                                pin_b: pb,
                            });
                        } else if c.parts.len() == 1 {
                            let via = matches!(&g.nodes[c.a.node], NodeKind::Junction(_))
                                .then(|| c.a.net.clone());
                            attach.push(Attach::Tail {
                                chain: ci,
                                anchor: ia,
                                pin: pa,
                                a_near: true,
                                via_junction: via,
                            });
                        }
                    }
                    (Some((ia, pa)), None) if c.parts.len() == 1 => {
                        let via = matches!(&g.nodes[c.a.node], NodeKind::Junction(_))
                            .then(|| c.a.net.clone());
                        attach.push(Attach::Tail {
                            chain: ci,
                            anchor: ia,
                            pin: pa,
                            a_near: true,
                            via_junction: via,
                        });
                    }
                    (None, Some((ib, pb))) if c.parts.len() == 1 => {
                        let via = matches!(&g.nodes[c.b.node], NodeKind::Junction(_))
                            .then(|| c.b.net.clone());
                        attach.push(Attach::Tail {
                            chain: ci,
                            anchor: ib,
                            pin: pb,
                            a_near: false,
                            via_junction: via,
                        });
                    }
                    // Two anchors bridged by a 2-part chain whose midpoint net
                    // is a PORT (forced label): the wire between them will never
                    // draw, so each part belongs at ITS anchor's pin — split
                    // into two tails, the port label lands on the free ends.
                    (Some((ia, pa)), Some((ib, pb)))
                        if ia != ib
                            && c.parts.len() == 2
                            && port_nets.contains(c.nets[1].as_str()) =>
                    {
                        attach.push(Attach::Tail {
                            chain: ci,
                            anchor: ia,
                            pin: pa,
                            a_near: true,
                            via_junction: None,
                        });
                        attach.push(Attach::Tail {
                            chain: ci,
                            anchor: ib,
                            pin: pb,
                            a_near: false,
                            via_junction: None,
                        });
                    }
                    // Different anchors: spine content (inter-module chain).
                    _ => {}
                }
            }
        }
    }

    // Cluster adoption: a small multi-pin part (4-pin crystal, sensor) whose
    // SIGNAL nets all resolve to pins of ONE other anchor belongs beside those
    // pins, like its load caps already do.
    let mut cluster_adopts: Vec<(usize, usize, Vec<String>)> = Vec::new(); // (item, anchor, anchor pins)
    {
        let small_rect = |i: usize| {
            let sz = items[i].geom.approx_size();
            sz[0] <= 18.0 && sz[1] <= 18.0
        };
        let candidates: Vec<usize> = anchors
            .iter()
            .copied()
            .filter(|&i| {
                items[i].geom.pins.len() >= 3
                    && items[i].geom.pins.len() <= 4
                    && small_rect(i)
                    && !is_connector_like_part(&items[i].part)
            })
            .collect();
        for i in candidates {
            let mut host: Option<usize> = None;
            let mut host_pins: Vec<String> = Vec::new();
            let mut ok = true;
            for (_, _, net) in &items[i].pins {
                let Some(net) = net else { continue };
                if class(net).is_rail() {
                    continue;
                }
                // Resolve the net to an anchor pin on some OTHER anchor.
                let found = anchors.iter().copied().filter(|&a| a != i).find_map(|a| {
                    items[a]
                        .pins
                        .iter()
                        .find(|(_, _, n)| n.as_deref() == Some(net.as_str()))
                        .map(|(num, _, _)| (a, num.clone()))
                });
                match found {
                    Some((a, pin)) => {
                        if host.is_some_and(|h| h != a) {
                            ok = false;
                            break;
                        }
                        host = Some(a);
                        host_pins.push(pin);
                    }
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok && let Some(a) = host
                && !host_pins.is_empty()
            {
                cluster_adopts.push((i, a, host_pins));
            }
        }
    }
    // No adoption CHAINS: a host that is itself adopted would leave its
    // adoptee orphaned (both skip module seeding). Drop such entries — the
    // remaining direct adoptions are cycle-free by construction.
    {
        let adoptees: BTreeSet<usize> = cluster_adopts.iter().map(|(i, _, _)| *i).collect();
        cluster_adopts.retain(|(_, a, _)| !adoptees.contains(a));
    }
    let cluster_set: BTreeSet<usize> = cluster_adopts.iter().map(|(i, _, _)| *i).collect();

    // ── Seed one module per anchor (adopted stubs are already filtered out).
    let mut mod_of_anchor: BTreeMap<usize, usize> = BTreeMap::new();
    for &a in anchors {
        if cluster_set.contains(&a) {
            continue;
        }
        mod_of_anchor.insert(a, form.modules.len());
        let h = half_size(&items[a], 0.0);
        form.modules.push(ModulePlan {
            anchor: a,
            sats: Vec::new(),
            env_min: Point2::new(-h.x, -h.y),
            env_max: Point2::new(h.x, h.y),
        });
    }

    // Claimed placement rects per module (anchor rect seeded), so every leg,
    // bridge, and bank cap lands in genuinely free space — the claims use the
    // FULL text-inclusive rect, which is what the overlap wall measures.
    let mut claims: BTreeMap<usize, Vec<geom::Rect>> = BTreeMap::new();
    // Vertical/horizontal wire runs per module: (column key, lo, hi).
    let mut runs: BTreeMap<usize, Vec<(i64, f64, f64, Option<String>)>> = BTreeMap::new();
    for (&a, &mi) in &mod_of_anchor {
        claims
            .entry(mi)
            .or_default()
            .push(sch_floorplan::contract::item_rect(&items[a], [0.0, 0.0]));
    }

    // Pins that a consumed chain will WIRE to (no label there).
    let attached_pins: BTreeSet<(usize, String)> = attach
        .iter()
        .map(|at| match at {
            Attach::Ladder { anchor, pin, .. } => (*anchor, pin.clone()),
            Attach::Bridge { anchor, pin_a, .. } => (*anchor, pin_a.clone()),
            Attach::Tail { anchor, pin, .. } => (*anchor, pin.clone()),
        })
        .chain(attach.iter().filter_map(|at| match at {
            Attach::Bridge { anchor, pin_b, .. } => Some((*anchor, pin_b.clone())),
            _ => None,
        }))
        .collect();

    // Reserve the realizer's PIN TEXT footprint: an unattached signal pin grows a
    // net-name label outward; a rail pin grows a power glyph. Satellites must
    // never sit in those strips (the "symbol overlaps label" lint). Reserve the
    // FULL name only where a label is near-certain (fanout ≥ 3 — bus/GPIO
    // distribution); a 2-pin inter-module net usually WIRES, and reserving its
    // whole auto-generated name inflates every envelope until the labels it
    // predicted become real (the lifted-board label-garden spiral).
    // Bundle detection: many parallel nets between one anchor PAIR (an MCU↔header
    // harness) always realize as labels, however short the span.
    let mut pair_nets: BTreeMap<(usize, usize), usize> = BTreeMap::new();
    let mut net_pair: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for (net, pins) in inc {
        if class(net).is_rail() {
            continue;
        }
        let mut owners: Vec<usize> = pins
            .iter()
            .filter_map(|(i, _)| anchors.contains(i).then_some(*i))
            .collect();
        owners.sort_unstable();
        owners.dedup();
        if let [a, b] = owners.as_slice() {
            *pair_nets.entry((*a, *b)).or_default() += 1;
            net_pair.insert(net.as_str(), (*a, *b));
        }
    }

    // Which nets get label reservations. Pass 1 (`labeled == None`) is
    // OPTIMISTIC: only certain labels reserve (rails, fanout >= 3, harness
    // bundles). Pass 2 receives the nets whose realized spans actually exceed
    // the wire threshold, breaking the reserve→spread→label fixpoint the
    // one-shot policy could never satisfy for both wire-first and label-first
    // boards at once.
    let mut label_boxes: BTreeMap<usize, Vec<geom::Rect>> = BTreeMap::new();
    for (&a, &mi) in &mod_of_anchor {
        for (num, _name, net) in &items[a].pins {
            let Some(net) = net else { continue };
            if attached_pins.contains(&(a, num.clone())) {
                continue;
            }
            let at = pin_offset(&items[a], num, 0.0);
            let is_rail = class(net).is_rail();
            let fanout = inc.get(net).map_or(0, |v| v.len());
            let bundled = net_pair
                .get(net.as_str())
                .is_some_and(|p| pair_nets.get(p).copied().unwrap_or(0) >= 4);
            // Connector pins label by convention regardless of span (a header
            // is a harness boundary), so their names always reserve.
            let connectorish =
                sch_place::netclass::is_connector_like(&items[a].part);
            let certain = fanout >= 3 || bundled || connectorish;
            let predicted = labeled.is_some_and(|set| set.contains(net.as_str()));
            let text = if is_rail {
                // Long rail names (+3V3A) outgrow the one-size glyph strip.
                7.62_f64.max(2.0 + 1.4 * net.chars().count() as f64)
            } else if certain || predicted {
                2.54 + 1.4 * net.chars().count() as f64
            } else {
                7.62_f64.min(2.54 + 1.4 * net.chars().count() as f64)
            };
            let r = match pin_side_of(a, num) {
                PinSide::East => geom::Rect::new(at.x, at.y - 1.27, at.x + text, at.y + 1.27),
                PinSide::West => geom::Rect::new(at.x - text, at.y - 1.27, at.x, at.y + 1.27),
                PinSide::North => geom::Rect::new(at.x - 1.9, at.y - 7.62, at.x + 1.9, at.y),
                PinSide::South => geom::Rect::new(at.x - 1.9, at.y, at.x + 1.9, at.y + 7.62),
            };
            claims.entry(mi).or_default().push(r);
            label_boxes.entry(mi).or_default().push(r);
        }
    }
    // The natural column of a pin: its own x for N/S pins, one pitch outward for
    // E/W pins (the wire elbows out, then the leg runs vertical).
    let pin_col = |a: usize, num: &str| -> f64 {
        let at = pin_offset(&items[a], num, 0.0);
        match pin_side_of(a, num) {
            PinSide::South | PinSide::North => at.x,
            PinSide::East => at.x + LEAD + PITCH,
            PinSide::West => at.x - LEAD - PITCH,
        }
    };
    // Slide a build along `step` until its rects clear the module's claims AND
    // its vertical wire run doesn't overlap another run in the same column —
    // two legs sharing a column with overlapping riser intervals is exactly the
    // collinear geometry KiCAD merges into one net.
    #[allow(clippy::too_many_arguments)]
    fn commit_free(
        items: &[Item],
        claims: &mut Vec<geom::Rect>,
        runs: &mut Vec<(i64, f64, f64, Option<String>)>,
        module: &mut ModulePlan,
        build: impl Fn(Point2) -> Vec<SatPlace>,
        run_of: impl Fn(Point2) -> (i64, f64, f64, Option<String>),
        at0: Point2,
        step: Point2,
    ) -> Option<Point2> {
        commit_free_opt(items, claims, runs, module, build, run_of, at0, step, 64, true)
    }

    /// `commit_free` with a slide budget and optional give-up: `must == false`
    /// returns false instead of force-committing a colliding fallback.
    #[allow(clippy::too_many_arguments)]
    fn commit_free_opt(
        items: &[Item],
        claims: &mut Vec<geom::Rect>,
        runs: &mut Vec<(i64, f64, f64, Option<String>)>,
        module: &mut ModulePlan,
        build: impl Fn(Point2) -> Vec<SatPlace>,
        run_of: impl Fn(Point2) -> (i64, f64, f64, Option<String>),
        at0: Point2,
        step: Point2,
        budget: usize,
        must: bool,
    ) -> Option<Point2> {
        let mut at = at0;
        for tries in 0..budget {
            let sats = build(at);
            let body: Vec<geom::Rect> = sats.iter().map(|s| placed_rect(items, s)).collect();
            let (ck, lo, hi, ref cnet) = run_of(at);
            // A satellite PIN landing on a previously committed wire run is an
            // electrical tap onto a foreign net (the bootstrap-approach short)
            // — cross-axis, so interval keys never catch it. The commit anchor
            // itself is exempt: that coincidence IS the intentional tap.
            let ep_on_wire = sats.iter().any(|sp| {
                let it = &items[sp.item];
                it.geom.pins.iter().any(|pg| {
                    let off = pg.at.transform_offset(sp.angle, false);
                    let (ex, ey) = (sp.offset.x + off[0], sp.offset.y + off[1]);
                    if (ex - at.x).abs() < 0.1 && (ey - at.y).abs() < 0.1 {
                        return false;
                    }
                    // The pin's own net may touch its own wires (that's the
                    // same conductor); only FOREIGN wires are shorts.
                    let pin_net = it
                        .pins
                        .iter()
                        .find(|(num, _, _)| *num == pg.number)
                        .and_then(|(_, _, n)| n.clone());
                    runs.iter().any(|(k, rlo, rhi, rnet)| {
                        let k = *k;
                        if k <= -900_000 {
                            return false;
                        }
                        if let (Some(a), Some(b)) = (&pin_net, rnet)
                            && a == b
                        {
                            return false;
                        }
                        if k >= 900_000 {
                            (snap(ey) / GRID).round() as i64 + 1_000_000 == k
                                && ex > *rlo - 0.1
                                && ex < *rhi + 0.1
                        } else {
                            (snap(ex) / GRID).round() as i64 == k
                                && ey > *rlo - 0.1
                                && ey < *rhi + 0.1
                        }
                    })
                })
            });
            if ep_on_wire && std::env::var_os("SPINE_DEBUG").is_some() {
                let refs: Vec<&str> = sats.iter().map(|s| items[s.item].refdes.as_str()).collect();
                eprintln!("[ep-guard] blocked {refs:?} at ({:.1},{:.1})", at.x, at.y);
            }
            let blocked = ep_on_wire
                || body
                    .iter()
                    .any(|r| claims.iter().any(|c| c.overlaps(r)))
                || runs.iter().any(|(k, rlo, rhi, rnet)| {
                    *k == ck
                        && lo < *rhi
                        && *rlo < hi
                        && !(cnet.is_some() && rnet.is_some() && cnet == rnet)
                });
            if !blocked {
                if tries > 2 && std::env::var_os("SPINE_DEBUG").is_some() {
                    let refs: Vec<&str> =
                        sats.iter().map(|s| items[s.item].refdes.as_str()).collect();
                    eprintln!(
                        "[slide] {refs:?} slid {tries} steps: {:?} -> {:?}",
                        (at0.x, at0.y),
                        (at.x, at.y)
                    );
                }
                claims.extend(body);
                runs.push((ck, lo, hi, cnet.clone()));
                module.sats.extend(sats);
                return Some(at);
            }
            at = Point2::new(at.x + step.x, at.y + step.y);
        }
        if !must {
            return None;
        }
        let sats = build(at0);
        claims.extend(sats.iter().map(|s| placed_rect(items, s)));
        runs.push(run_of(at0));
        module.sats.extend(sats);
        Some(at0)
    }

    // ── Bridges first: they own the pin column between their two pins.
    for at in &attach {
        let Attach::Bridge { chain, anchor, pin_a, pin_b } = at else {
            continue;
        };
        let c = &g.chains[*chain];
        let mi = mod_of_anchor[anchor];
        let pa = pin_offset(&items[*anchor], pin_a, 0.0);
        let pb = pin_offset(&items[*anchor], pin_b, 0.0);
        let (sa, sb) = (pin_side_of(*anchor, pin_a), pin_side_of(*anchor, pin_b));
        let opposite = matches!(
            (sa, sb),
            (PinSide::East, PinSide::West) | (PinSide::West, PinSide::East)
        );
        if opposite {
            // Feedback across the body (op-amp out → in): a HORIZONTAL run
            // ABOVE the anchor, wires looping over the top — a vertical column
            // would drag its approach wire straight through the package.
            let anchor_rect = sch_floorplan::contract::item_rect(&items[*anchor], [0.0, 0.0]);
            let (parts, nets) = (c.parts.clone(), c.nets.clone());
            let build = |at: Point2| -> Vec<SatPlace> {
                let mut x = at.x;
                let mut out = Vec::new();
                for (k, &p) in parts.iter().enumerate() {
                    let item = &items[p];
                    let angle = orient_for(item, &nets[k], Orient::Right);
                    let entry = pin_on(item, &nets[k]).unwrap_or_default();
                    let exit = pin_on(item, &nets[k + 1]).unwrap_or_default();
                    let e_off = pin_offset(item, &entry, angle);
                    let x_off = pin_offset(item, &exit, angle);
                    out.push(SatPlace {
                        item: p,
                        offset: Point2::new(snap(x - e_off.x), snap(at.y - e_off.y)),
                        angle,
                    });
                    x += (x_off.x - e_off.x).abs() + LEAD;
                }
                out
            };
            let at0 = Point2::new((pa.x + pb.x) / 2.0, anchor_rect.min_y - LEAD * 2.0);
            // Horizontal feedback runs live in a disjoint key space (row keys
            // offset far from any column key).
            let bridge_net = c.nets.first().cloned();
            let run_of = |at: Point2| {
                (
                    1_000_000 + (snap(at.y) / GRID).round() as i64,
                    pa.x.min(pb.x),
                    pa.x.max(pb.x),
                    bridge_net.clone(),
                )
            };
            let module = &mut form.modules[mi];
            let committed = commit_free(
                items,
                claims.entry(mi).or_default(),
                runs.entry(mi).or_default(),
                module,
                build,
                run_of,
                at0,
                Point2::new(0.0, -PITCH),
            );
            // The VERTICAL pin approaches (each pin up to the bridge row) are
            // wires: register their column intervals or a same-column tail
            // pin lands on them (the BUCK_EN / bootstrap short).
            if let Some(at) = committed {
                let rr = runs.entry(mi).or_default();
                for (p, n) in [(pa, c.nets.first()), (pb, c.nets.last())] {
                    rr.push((
                        (snap(p.x) / GRID).round() as i64,
                        p.y.min(at.y),
                        p.y.max(at.y),
                        n.cloned(),
                    ));
                }
            }
            form.consumed.insert(*chain, mi);
            continue;
        }
        let x0 = pin_col(*anchor, pin_a).max(pin_col(*anchor, pin_b));
        let outward = if x0 >= 0.0 { PITCH } else { -PITCH };
        // Stack parts vertically from the higher pin toward the lower one,
        // centered in the span.
        let (top, bot) = if pa.y <= pb.y { (pa.y, pb.y) } else { (pb.y, pa.y) };
        let (parts, nets): (Vec<usize>, Vec<String>) = if pa.y <= pb.y {
            (c.parts.clone(), c.nets.clone())
        } else {
            (
                c.parts.iter().rev().copied().collect(),
                c.nets.iter().rev().cloned().collect(),
            )
        };
        let spans: Vec<f64> = parts
            .iter()
            .enumerate()
            .map(|(k, &p)| {
                let item = &items[p];
                let angle = orient_for(item, &nets[k], Orient::Down);
                let entry = pin_on(item, &nets[k]).unwrap_or_default();
                let exit = pin_on(item, &nets[k + 1]).unwrap_or_default();
                part_span(item, angle, &entry, &exit)
            })
            .collect();
        let content: f64 = spans.iter().sum();
        let build = |at: Point2| -> Vec<SatPlace> {
            let mut y = top + ((bot - top) - content) / 2.0;
            let mut out = Vec::new();
            for (k, &p) in parts.iter().enumerate() {
                let item = &items[p];
                let angle = orient_for(item, &nets[k], Orient::Down);
                let entry = pin_on(item, &nets[k]).unwrap_or_default();
                let e_off = pin_offset(item, &entry, angle);
                out.push(SatPlace {
                    item: p,
                    offset: Point2::new(snap(at.x - e_off.x), snap(y - e_off.y)),
                    angle,
                });
                y += spans[k];
            }
            out
        };
        let vb_net = c.nets.first().cloned();
        let run_of =
            |at: Point2| ((snap(at.x) / GRID).round() as i64, top, bot, vb_net.clone());
        let module = &mut form.modules[mi];
        let committed = commit_free(
            items,
            claims.entry(mi).or_default(),
            runs.entry(mi).or_default(),
            module,
            build,
            run_of,
            Point2::new(x0, 0.0),
            Point2::new(outward, 0.0),
        );
        // The two horizontal PIN APPROACHES are wires too: register their row
        // intervals or a same-row tail lands its pin on them (the BUCK_EN /
        // bootstrap short).
        if let Some(at) = committed {
            let rr = runs.entry(mi).or_default();
            for (p, n) in [(pa, c.nets.first()), (pb, c.nets.last())] {
                rr.push((
                    1_000_000 + (snap(p.y) / GRID).round() as i64,
                    p.x.min(at.x),
                    p.x.max(at.x),
                    n.cloned(),
                ));
            }
        }
        form.consumed.insert(*chain, mi);
    }

    // ── Ladders: vertical legs at the pin column, up to supplies, down to ground.
    // Processing order decides who gets the near column. An UP leg's body spans
    // the rows of pins ABOVE its own, so those pins must take NEARER columns:
    // up-legs go top-pin-first, down-legs bottom-pin-first — then every
    // horizontal approach wire stops short of the farther legs' bodies.
    // Junction nets claimed by a tail's NEAR side: their shunt legs defer to
    // the junction-adopted pass (they hang from the tail's wire, not the pin
    // flank the tail needs).
    let tail_claimed: BTreeSet<&str> = attach
        .iter()
        .filter_map(|at| match at {
            Attach::Tail { via_junction: Some(j), .. } => Some(j.as_str()),
            _ => None,
        })
        .collect();

    // How many deferred legs each tail-claimed junction is waiting to hang:
    // the tail leaves that much extra wire between pin and part so the legs'
    // taps land ON the wire (an off-wire tap forces a labeled stub).
    let mut deferred_legs: BTreeMap<&str, usize> = BTreeMap::new();
    for at in &attach {
        if let Attach::Ladder { sig_net, .. } = at
            && tail_claimed.contains(sig_net.as_str())
        {
            *deferred_legs.entry(sig_net.as_str()).or_default() += 1;
        }
    }

    let ladder_order: Vec<&Attach> = {
        let mut v: Vec<&Attach> = attach
            .iter()
            .filter(|at| matches!(at, Attach::Ladder { .. }))
            .collect();
        v.sort_by(|x, y| {
            let key = |at: &&Attach| match at {
                Attach::Ladder { anchor, pin, up, .. } => {
                    let py = pin_offset(&items[*anchor], pin, 0.0).y;
                    (*anchor, *up, if *up { py } else { -py })
                }
                _ => unreachable!(),
            };
            let (ax, ux, kx) = key(x);
            let (ay, uy, ky) = key(y);
            ax.cmp(&ay).then(ux.cmp(&uy)).then(kx.total_cmp(&ky))
        });
        v
    };
    for at in ladder_order {
        let Attach::Ladder { chain, anchor, pin, a_near, up, sig_net } = at else {
            continue;
        };
        if tail_claimed.contains(sig_net.as_str()) {
            continue;
        }
        let c = &g.chains[*chain];
        let mi = mod_of_anchor[anchor];
        let (mut parts, mut nets) = (c.parts.clone(), c.nets.clone());
        if !a_near {
            parts.reverse();
            nets.reverse();
        }
        let pin_at = pin_offset(&items[*anchor], pin, 0.0);
        let x0 = pin_col(*anchor, pin);
        let outward = match pin_side_of(*anchor, pin) {
            PinSide::West => -PITCH,
            _ => PITCH,
        };
        let dirn = if *up { -1.0 } else { 1.0 };
        let dir = if *up { Orient::Up } else { Orient::Down };
        let side = pin_side_of(*anchor, pin);
        // N/S legs lead away from the pin; E/W legs START ON the pin's row so
        // the approach wire is a straight horizontal — leading vertically first
        // would hug the pin column and run through the 2.54-pitch neighbours.
        let y_start = match side {
            PinSide::South | PinSide::North => pin_at.y + dirn * LEAD,
            _ => pin_at.y,
        };
        let build = |at: Point2| -> Vec<SatPlace> {
            let mut y = y_start;
            let mut out = Vec::new();
            for (k, &p) in parts.iter().enumerate() {
                let item = &items[p];
                let angle = orient_for(item, &nets[k], dir);
                let entry = pin_on(item, &nets[k]).unwrap_or_default();
                let exit = pin_on(item, &nets[k + 1]).unwrap_or_default();
                let e_off = pin_offset(item, &entry, angle);
                out.push(SatPlace {
                    item: p,
                    offset: Point2::new(snap(at.x - e_off.x), snap(y - e_off.y)),
                    angle,
                });
                y += dirn * (part_span(item, angle, &entry, &exit) + LEAD);
            }
            out
        };
        // The leg's vertical wire run: pin row to past the last part + glyph.
        let leg_len: f64 = {
            let mut total = 0.0;
            for (k, &p) in parts.iter().enumerate() {
                let item = &items[p];
                let angle = orient_for(item, &nets[k], dir);
                let entry = pin_on(item, &nets[k]).unwrap_or_default();
                let exit = pin_on(item, &nets[k + 1]).unwrap_or_default();
                total += part_span(item, angle, &entry, &exit) + LEAD;
            }
            total + 5.08
        };
        let ladder_net = nets.first().cloned();
        let run_of = |at: Point2| {
            let (lo, hi) = if *up {
                (y_start - leg_len, pin_at.y)
            } else {
                (pin_at.y, y_start + leg_len)
            };
            ((snap(at.x) / GRID).round() as i64, lo, hi, ladder_net.clone())
        };
        let module = &mut form.modules[mi];
        commit_free(
            items,
            claims.entry(mi).or_default(),
            runs.entry(mi).or_default(),
            module,
            build,
            run_of,
            Point2::new(x0, 0.0),
            Point2::new(outward, 0.0),
        );
        if std::env::var_os("SPINE_DEBUG").is_some() {
            let refs: Vec<&str> = parts.iter().map(|&p| items[p].refdes.as_str()).collect();
            eprintln!(
                "[attach] LADDER {refs:?} at {}:{} up={}",
                items[*anchor].refdes, pin, up
            );
        }
        form.consumed.insert(*chain, mi);
    }

    // ── Tails: the chain part sits beside its anchor pin, pointing outward;
    // the free end keeps its label. A tail that finds no clean spot stays a
    // free run (two labels beat a force-fit collision). A committed tail gives
    // its free-end JUNCTION a physical home (module, tap position) — the
    // junction's other legs ladder there in the pass below, turning a column
    // of labeled islets into one wired divider.
    let mut tail_home: BTreeMap<String, (usize, Point2, f64)> = BTreeMap::new(); // (module, tap, outward sign)
    for at in attach.iter().filter(|at| matches!(at, Attach::Tail { .. })) {
        let Attach::Tail { chain, anchor, pin, a_near, via_junction } = at else {
            unreachable!()
        };
        let Some(&mi) = mod_of_anchor.get(anchor) else { continue };
        let c = &g.chains[*chain];
        let part = if *a_near {
            c.parts[0]
        } else {
            *c.parts.last().expect("tail chain has parts")
        };
        let near_net = if *a_near { &c.nets[0] } else { &c.nets[c.nets.len() - 1] };
        let pin_at = pin_offset(&items[*anchor], pin, 0.0);
        let side = pin_side_of(*anchor, pin);
        let item = &items[part];
        let (dir, step) = match side {
            PinSide::East => (Orient::Right, Point2::new(PITCH, 0.0)),
            PinSide::West => (Orient::Left, Point2::new(-PITCH, 0.0)),
            PinSide::North => (Orient::Up, Point2::new(0.0, -PITCH)),
            PinSide::South => (Orient::Down, Point2::new(0.0, PITCH)),
        };
        let entry = pin_on(item, near_net).unwrap_or_default();
        let free_net_for_angle =
            if *a_near { &c.nets[1] } else { &c.nets[c.nets.len() - 2] };
        let free_pin_for_angle = pin_on(item, free_net_for_angle).unwrap_or_default();
        // Pick the rotation whose NEAR pin faces the anchor ON THE RIGHT AXIS —
        // orient_for infers from pins-Vec order, which need not match geometry
        // (the flipped-R9 bug), and a `>=` tie once accepted a VERTICAL body
        // for a horizontal tail (equal x!), laying it across the neighbour
        // rows' wires (the buck R4/bootstrap short). Strict: the pin pair must
        // be colinear along the tail's axis with the near pin toward the pin.
        let angle = {
            let cand = orient_for(item, near_net, dir);
            let pick = |a: f64| {
                let n = pin_offset(item, &entry, a);
                let f = pin_offset(item, &free_pin_for_angle, a);
                match side {
                    PinSide::West => n.x > f.x + 0.01 && (n.y - f.y).abs() < 0.01,
                    PinSide::East => n.x < f.x - 0.01 && (n.y - f.y).abs() < 0.01,
                    PinSide::North => n.y > f.y + 0.01 && (n.x - f.x).abs() < 0.01,
                    PinSide::South => n.y < f.y - 0.01 && (n.x - f.x).abs() < 0.01,
                }
            };
            [0.0_f64, 90.0, 180.0, 270.0]
                .iter()
                .map(|d| (cand + d).rem_euclid(360.0))
                .find(|&a| pick(a))
                .unwrap_or(cand)
        };
        let e_off = pin_offset(item, &entry, angle);
        let near_gap = LEAD * 2.0
            + via_junction
                .as_ref()
                .and_then(|j: &String| deferred_legs.get(j.as_str()))
                .map_or(0.0, |&n| n as f64 * 12.7);
        let at0 = match side {
            PinSide::East => Point2::new(pin_at.x + near_gap, pin_at.y),
            PinSide::West => Point2::new(pin_at.x - near_gap, pin_at.y),
            PinSide::North => Point2::new(pin_at.x, pin_at.y - near_gap),
            PinSide::South => Point2::new(pin_at.x, pin_at.y + near_gap),
        };
        let build = |at: Point2| -> Vec<SatPlace> {
            vec![SatPlace {
                item: part,
                offset: Point2::new(snap(at.x - e_off.x), snap(at.y - e_off.y)),
                angle,
            }]
        };
        let tail_net = Some(near_net.clone());
        let run_of = |at: Point2| match side {
            PinSide::East | PinSide::West => {
                (1_000_000 + (snap(pin_at.y) / GRID).round() as i64,
                 pin_at.x.min(at.x), pin_at.x.max(at.x), tail_net.clone())
            }
            _ => ((snap(pin_at.x) / GRID).round() as i64,
                  pin_at.y.min(at.y), pin_at.y.max(at.y), tail_net.clone()),
        };
        let module = &mut form.modules[mi];
        // Budget 18: connector pin strips reserve ~26mm and a switching flank
        // stacks bridge+banks ~40mm deep; the in-line elements (L2's SW row)
        // must clear them rather than exile to the strap column.
        let ok = commit_free_opt(
            items,
            claims.entry(mi).or_default(),
            runs.entry(mi).or_default(),
            module,
            build,
            run_of,
            at0,
            step,
            18,
            false,
        );
        if ok.is_some() {
            if std::env::var_os("SPINE_DEBUG").is_some() {
                eprintln!("[attach] TAIL {} at {}:{}", items[part].refdes, items[*anchor].refdes, pin);
            }
            form.consumed.insert(*chain, mi);
            // The near-side junction (if any) now lives on the tail's wire at
            // the entry pin: its deferred shunt legs hang there.
            if let Some(jnet) = via_junction
                && let Some(sat) = form.modules[mi].sats.last()
            {
                let n_off = pin_offset(item, &entry, sat.angle);
                let outward = match side {
                    PinSide::West => -1.0,
                    PinSide::East => 1.0,
                    _ => 0.0,
                };
                if outward != 0.0 {
                    tail_home.entry(jnet.clone()).or_insert((
                        mi,
                        Point2::new(sat.offset.x + n_off.x, sat.offset.y + n_off.y),
                        outward,
                    ));
                }
            }
            let free_net = if *a_near { &c.nets[1] } else { &c.nets[c.nets.len() - 2] };
            // A PORT free end carries its label for certain: reserve the text
            // footprint (envelope + claims) or the neighbouring module lands
            // on it.
            if port_nets.contains(free_net.as_str())
                && let Some(free_pin) = pin_on(item, free_net)
                && let Some(sat) = form.modules[mi].sats.last()
            {
                let f = pin_offset(item, &free_pin, sat.angle);
                let (fx, fy) = (sat.offset.x + f.x, sat.offset.y + f.y);
                let text = 2.54 + 1.4 * free_net.chars().count() as f64;
                let dir = match side {
                    PinSide::West => -1.0,
                    PinSide::East => 1.0,
                    _ => 0.0,
                };
                if dir != 0.0 {
                    let r = geom::Rect::new(
                        fx + (dir * text).min(0.0),
                        fy - 2.54,
                        fx + (dir * text).max(0.0),
                        fy + 2.54,
                    );
                    claims.entry(mi).or_default().push(r);
                    label_boxes.entry(mi).or_default().push(r);
                }
            }
            let outward = match side {
                PinSide::West => -1.0,
                PinSide::East => 1.0,
                _ => 0.0,
            };
            if outward != 0.0
                && let Some(free_pin) = pin_on(item, free_net)
                && let Some(sat) = form.modules[mi].sats.last()
            {
                let f_off = pin_offset(item, &free_pin, sat.angle);
                tail_home.entry(free_net.clone()).or_insert((
                    mi,
                    Point2::new(sat.offset.x + f_off.x, sat.offset.y + f_off.y),
                    outward,
                ));
            }
        }
    }

    // ── Tail CHAINING: an unconsumed 1-part series chain whose junction sits
    // on a tail's wire (a recorded home) continues IN-LINE outward from it —
    // the analog cascade grammar: pin ← C13 ← HPF_TAP ← C12 ← PRE_AMP, one
    // row, labels only at the true far end. Fixpoint: each placed link's far
    // net becomes a new home.
    loop {
        let mut progressed = false;
        for (ci, c) in g.chains.iter().enumerate() {
            if c.parts.len() != 1
                || form.consumed.contains_key(&ci)
                || c.role(classes) != ChainRole::Series
            {
                continue;
            }
            let (near_t, far_t, a_near) = if tail_home.contains_key(&c.a.net) {
                (&c.a, &c.b, true)
            } else if tail_home.contains_key(&c.b.net) {
                (&c.b, &c.a, false)
            } else {
                continue;
            };
            if !matches!(&g.nodes[near_t.node], NodeKind::Junction(_)) {
                continue;
            }
            // Chain ONLY dead-end links: if either side resolves to an anchor
            // pin, the chain is wired flow (arrange places it between its
            // modules) — chaining it here rips it out of that flow (the C10
            // pot-coupling regression).
            if matches!(junction_pin.get(&near_t.net), Some(Some(_))) {
                continue;
            }
            let far_flow = match &g.nodes[far_t.node] {
                NodeKind::Part(_) => true,
                NodeKind::Junction(_) => {
                    let fanout = g
                        .chains
                        .iter()
                        .filter(|cc| cc.a.node == far_t.node || cc.b.node == far_t.node)
                        .count();
                    // A label-fanout junction pennants no matter where its
                    // pins sit — chaining onto it loses nothing.
                    fanout < 5 && matches!(junction_pin.get(&far_t.net), Some(Some(_)))
                }
                NodeKind::Rail(_) => true,
            };
            if far_flow {
                continue;
            }
            let &(mi, home, outward) = &tail_home[&near_t.net];
            let part = c.parts[0];
            let item = &items[part];
            let dir = if outward < 0.0 { Orient::Left } else { Orient::Right };
            let entry = pin_on(item, &near_t.net).unwrap_or_default();
            let far_pin = pin_on(item, &far_t.net).unwrap_or_default();
            let angle = {
                let cand = orient_for(item, &near_t.net, dir);
                let flipped = (cand + 180.0).rem_euclid(360.0);
                let pick = |a: f64| {
                    let n = pin_offset(item, &entry, a);
                    let f = pin_offset(item, &far_pin, a);
                    if outward < 0.0 { n.x >= f.x } else { n.x <= f.x }
                };
                if pick(cand) { cand } else { flipped }
            };
            let e_off = pin_offset(item, &entry, angle);
            // A shared net with a THIRD attachment keeps its pennant at this
            // junction: widen the gap to the pennant's text so it lands clear
            // of both bodies.
            let gap = if inc.get(&near_t.net).map_or(0, |v| v.len()) > 2 {
                LEAD * 2.0 + 2.54 + 1.4 * near_t.net.chars().count() as f64
            } else {
                LEAD * 2.0
            };
            let at0 = Point2::new(home.x + outward * gap, home.y);
            let build = |at: Point2| -> Vec<SatPlace> {
                vec![SatPlace {
                    item: part,
                    offset: Point2::new(snap(at.x - e_off.x), snap(at.y - e_off.y)),
                    angle,
                }]
            };
            let chain_net = Some(near_t.net.clone());
            let run_of = |at: Point2| {
                (1_000_000 + (snap(home.y) / GRID).round() as i64,
                 home.x.min(at.x), home.x.max(at.x), chain_net.clone())
            };
            let ok = commit_free_opt(
                items,
                claims.entry(mi).or_default(),
                runs.entry(mi).or_default(),
                &mut form.modules[mi],
                build,
                run_of,
                at0,
                Point2::new(outward * PITCH, 0.0),
                8,
                false,
            );
            if ok.is_some() {
                if std::env::var_os("SPINE_DEBUG").is_some() {
                    eprintln!("[attach] CHAIN {} onto {} home", items[part].refdes, near_t.net);
                }
                form.consumed.insert(ci, mi);
                if let Some(sat) = form.modules[mi].sats.last() {
                    let f_off = pin_offset(item, &far_pin, sat.angle);
                    tail_home.entry(far_t.net.clone()).or_insert((
                        mi,
                        Point2::new(sat.offset.x + f_off.x, sat.offset.y + f_off.y),
                        outward,
                    ));
                    // A PORT far end labels for certain: reserve its text.
                    if port_nets.contains(far_t.net.as_str()) {
                        let (fx, fy) = (sat.offset.x + f_off.x, sat.offset.y + f_off.y);
                        let text = 2.54 + 1.4 * far_t.net.chars().count() as f64;
                        let r = geom::Rect::new(
                            fx + (outward * text).min(0.0),
                            fy - 2.54,
                            fx + (outward * text).max(0.0),
                            fy + 2.54,
                        );
                        claims.entry(mi).or_default().push(r);
                        label_boxes.entry(mi).or_default().push(r);
                    }
                }
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }

    // ── Junction-adopted legs: a ShuntLeg whose signal end is a junction with
    // a tail home ladders at the TAP — R3 up to the supply, R5/C9 down to
    // ground, the human divider — instead of floating as labeled islets.
    for (ci, c) in g.chains.iter().enumerate() {
        if c.parts.is_empty()
            || form.consumed.contains_key(&ci)
            || c.role(classes) != ChainRole::ShuntLeg
        {
            continue;
        }
        let (sig_t, a_near, rail_net) = if class(&c.a.net).is_rail() {
            (&c.b, false, c.a.net.clone())
        } else {
            (&c.a, true, c.b.net.clone())
        };
        let NodeKind::Junction(jnet) = &g.nodes[sig_t.node] else {
            continue;
        };
        let Some(&(mi, tap_at, outward)) = tail_home.get(jnet) else {
            continue;
        };
        let (mut parts, mut nets) = (c.parts.clone(), c.nets.clone());
        if !a_near {
            parts.reverse();
            nets.reverse();
        }
        let up = class(&rail_net) == NetClass::Supply;
        let dirn = if up { -1.0 } else { 1.0 };
        let dir = if up { Orient::Up } else { Orient::Down };
        // Shallow first (body right under the wire), then a deeper lead that
        // clears the row's text band — the human drop when the row is dense.
        let mut ok = false;
        for depth in [LEAD, LEAD * 4.0] {
            let y_start = tap_at.y + dirn * depth;
            let build = |at: Point2| -> Vec<SatPlace> {
                let mut y = y_start;
                let mut out = Vec::new();
                for (k, &p) in parts.iter().enumerate() {
                    let item = &items[p];
                    let angle = orient_for(item, &nets[k], dir);
                    let entry = pin_on(item, &nets[k]).unwrap_or_default();
                    let exit = pin_on(item, &nets[k + 1]).unwrap_or_default();
                    let e_off = pin_offset(item, &entry, angle);
                    out.push(SatPlace {
                        item: p,
                        offset: Point2::new(snap(at.x - e_off.x), snap(y - e_off.y)),
                        angle,
                    });
                    y += dirn * (part_span(item, angle, &entry, &exit) + LEAD);
                }
                out
            };
            let leg_len: f64 = parts
                .iter()
                .enumerate()
                .map(|(k, &p)| {
                    let item = &items[p];
                    let angle = orient_for(item, &nets[k], dir);
                    let entry = pin_on(item, &nets[k]).unwrap_or_default();
                    let exit = pin_on(item, &nets[k + 1]).unwrap_or_default();
                    part_span(item, angle, &entry, &exit) + LEAD
                })
                .sum::<f64>()
                + 5.08;
            let leg_net = nets.first().cloned();
            let run_of = |at: Point2| {
                let (lo, hi) = if up {
                    (y_start - leg_len, tap_at.y)
                } else {
                    (tap_at.y, y_start + leg_len)
                };
                ((snap(at.x) / GRID).round() as i64, lo, hi, leg_net.clone())
            };
            ok = commit_free_opt(
                items,
                claims.entry(mi).or_default(),
                runs.entry(mi).or_default(),
                &mut form.modules[mi],
                build,
                run_of,
                Point2::new(tap_at.x, 0.0),
                Point2::new(outward * PITCH, 0.0),
                8,
                false,
            )
            .is_some()
                || commit_free_opt(
                    items,
                    claims.entry(mi).or_default(),
                    runs.entry(mi).or_default(),
                    &mut form.modules[mi],
                    build,
                    run_of,
                    Point2::new(tap_at.x - outward * PITCH, 0.0),
                    Point2::new(-outward * PITCH, 0.0),
                    2,
                    false,
                )
                .is_some();
            if ok {
                break;
            }
        }
        if ok {
            form.consumed.insert(ci, mi);
        } else if std::env::var_os("SPINE_DEBUG").is_some() {
            eprintln!(
                "[adopt-leg] FAILED {} at net {} home=({:.1},{:.1}) outward={outward}",
                items[c.parts[0]].refdes, jnet, tap_at.x, tap_at.y
            );
        }
    }

    // ── Adopted stubs: one part beside its pin, pointing outward, wired short.
    // A stub that finds no clean nearby spot (label-dense pin columns) reverts
    // to its own module — labeled islets beat force-fit collisions.
    let mut unadopted: Vec<usize> = Vec::new();
    for (stub, anchor, pin, net) in adopts {
        let Some(&mi) = mod_of_anchor.get(&anchor) else { continue };
        let pin_at = pin_offset(&items[anchor], &pin, 0.0);
        let side = pin_side_of(anchor, &pin);
        let item = &items[stub];
        let (dir, step) = match side {
            PinSide::East => (Orient::Right, Point2::new(PITCH, 0.0)),
            PinSide::West => (Orient::Left, Point2::new(-PITCH, 0.0)),
            PinSide::North => (Orient::Up, Point2::new(0.0, -PITCH)),
            PinSide::South => (Orient::Down, Point2::new(0.0, PITCH)),
        };
        let angle = orient_for(item, &net, dir);
        let entry = pin_on(item, &net).unwrap_or_default();
        let e_off = pin_offset(item, &entry, angle);
        let at0 = match side {
            PinSide::East => Point2::new(pin_at.x + LEAD * 2.0, pin_at.y),
            PinSide::West => Point2::new(pin_at.x - LEAD * 2.0, pin_at.y),
            PinSide::North => Point2::new(pin_at.x, pin_at.y - LEAD * 2.0),
            PinSide::South => Point2::new(pin_at.x, pin_at.y + LEAD * 2.0),
        };
        let build = |at: Point2| -> Vec<SatPlace> {
            vec![SatPlace {
                item: stub,
                offset: Point2::new(snap(at.x - e_off.x), snap(at.y - e_off.y)),
                angle,
            }]
        };
        let stub_net = Some(net.clone());
        let run_of = |at: Point2| match side {
            PinSide::East | PinSide::West => {
                (1_000_000 + (snap(pin_at.y) / GRID).round() as i64,
                 pin_at.x.min(at.x), pin_at.x.max(at.x), stub_net.clone())
            }
            _ => ((snap(pin_at.x) / GRID).round() as i64,
                  pin_at.y.min(at.y), pin_at.y.max(at.y), stub_net.clone()),
        };
        let module = &mut form.modules[mi];
        let ok = commit_free_opt(
            items,
            claims.entry(mi).or_default(),
            runs.entry(mi).or_default(),
            module,
            build,
            run_of,
            at0,
            step,
            6,
            false,
        );
        if ok.is_none() {
            unadopted.push(stub);
        }
    }
    for a in unadopted {
        form.strap_items.insert(a);
        mod_of_anchor.insert(a, form.modules.len());
        let h = half_size(&items[a], 0.0);
        form.modules.push(ModulePlan {
            anchor: a,
            sats: Vec::new(),
            env_min: Point2::new(-h.x, -h.y),
            env_max: Point2::new(h.x, h.y),
        });
    }
    // Never-adoptable 2-pin stub modules are straps too.
    for (&a, _) in &mod_of_anchor {
        if items[a].geom.pins.len() <= 2
            && !is_connector_like_part(&items[a].part)
            && form.modules[mod_of_anchor[&a]].sats.is_empty()
        {
            form.strap_items.insert(a);
        }
    }

    // ── Cluster adoptions: beside the mean of their host pins, one side out.
    for (item_i, anchor, host_pins) in cluster_adopts {
        let Some(&mi) = mod_of_anchor.get(&anchor) else { continue };
        let pts: Vec<Point2> = host_pins
            .iter()
            .map(|p| pin_offset(&items[anchor], p, 0.0))
            .collect();
        let mean_y = pts.iter().map(|p| p.y).sum::<f64>() / pts.len() as f64;
        let side = pin_side_of(anchor, &host_pins[0]);
        let h = half_size(&items[item_i], 0.0);
        let (at0, step) = match side {
            PinSide::West => (
                Point2::new(pts.iter().map(|p| p.x).fold(f64::MAX, f64::min)
                    - LEAD * 2.0 - h.x, mean_y),
                Point2::new(-PITCH, 0.0),
            ),
            PinSide::East => (
                Point2::new(pts.iter().map(|p| p.x).fold(f64::MIN, f64::max)
                    + LEAD * 2.0 + h.x, mean_y),
                Point2::new(PITCH, 0.0),
            ),
            PinSide::North => (
                Point2::new(pts.iter().map(|p| p.x).sum::<f64>() / pts.len() as f64,
                    pts.iter().map(|p| p.y).fold(f64::MAX, f64::min) - LEAD * 2.0 - h.y),
                Point2::new(0.0, -PITCH),
            ),
            PinSide::South => (
                Point2::new(pts.iter().map(|p| p.x).sum::<f64>() / pts.len() as f64,
                    pts.iter().map(|p| p.y).fold(f64::MIN, f64::max) + LEAD * 2.0 + h.y),
                Point2::new(0.0, PITCH),
            ),
        };
        let build = |at: Point2| -> Vec<SatPlace> {
            vec![SatPlace { item: item_i, offset: Point2::new(snap(at.x), snap(at.y)), angle: 0.0 }]
        };
        let run_of = |_at: Point2| (i64::MIN / 4 + item_i as i64, 0.0, 0.0, None);
        let module = &mut form.modules[mi];
        let ok = commit_free_opt(
            items,
            claims.entry(mi).or_default(),
            runs.entry(mi).or_default(),
            module,
            build,
            run_of,
            at0,
            step,
            8,
            false,
        );
        if ok.is_none() {
            // Revert to a standalone module (strap column will collect it).
            form.strap_items.insert(item_i);
            mod_of_anchor.insert(item_i, form.modules.len());
            let h2 = half_size(&items[item_i], 0.0);
            form.modules.push(ModulePlan {
                anchor: item_i,
                sats: Vec::new(),
                env_min: Point2::new(-h2.x, -h2.y),
                env_max: Point2::new(h2.x, h2.y),
            });
        }
    }

    // ── Decoupling banks: round-robin caps onto anchors sharing the supply net.
    let mut supply_pins: BTreeMap<(usize, String), usize> = BTreeMap::new();
    for &a in anchors {
        for (_, _, net) in &items[a].pins {
            if let Some(net) = net
                && class(net) == NetClass::Supply
            {
                *supply_pins.entry((a, net.clone())).or_default() += 1;
            }
        }
    }
    let mut served: BTreeMap<(usize, String), usize> = BTreeMap::new();
    // Assign every cap to a module first; each module's bank then places as ONE
    // rigid grid block, so claims collisions slide the whole bank (rhythm kept)
    // instead of scattering individual caps.
    let mut bank_of: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for ci in decouple {
        let c = &g.chains[ci];
        let supply = if class(&c.a.net) == NetClass::Supply {
            c.a.net.clone()
        } else {
            c.b.net.clone()
        };
        let target = anchors
            .iter()
            .filter(|a| supply_pins.contains_key(&(**a, supply.clone())))
            .max_by_key(|a| {
                let demand = supply_pins[&(**a, supply.clone())];
                let done = served.get(&(**a, supply.clone())).copied().unwrap_or(0);
                (demand.saturating_sub(done), usize::MAX - **a)
            })
            .copied();
        let Some(anchor) = target else { continue };
        *served.entry((anchor, supply.clone())).or_default() += 1;
        bank_of.entry(mod_of_anchor[&anchor]).or_default().push(ci);
    }

    /// Human bank pitch: 9 grid columns (value text clears the neighbour's
    /// glyph label), single-lead row gap.
    const CAP_PITCH: f64 = 11.43;
    for (mi, chains) in &bank_of {
        let module_anchor = form.modules[*mi].anchor;
        let anchor_half = half_size(&items[module_anchor], 0.0);
        // Per-cap orientation (supply pin up) computed once, in chain order.
        let mut rail_chars = 0usize;
        let caps: Vec<(usize, f64)> = chains
            .iter()
            .map(|&ci| {
                let c = &g.chains[ci];
                let p = c.parts[0];
                let supply_net = if class(&c.a.net) == NetClass::Supply {
                    c.nets.first().unwrap().clone()
                } else {
                    c.nets.last().unwrap().clone()
                };
                rail_chars = rail_chars.max(supply_net.chars().count());
                (p, orient_for(&items[p], &supply_net, Orient::Down))
            })
            .collect();
        // A long rail name (+3V3A) outgrows the human 9-column pitch: widen so
        // the glyph's value text clears the neighbour cap's field.
        let cap_pitch = CAP_PITCH.max(2.54 + 1.4 * rail_chars as f64 + 2.54);
        let h = half_size(&items[caps[0].0], caps[0].1);
        // A tall anchor (MCU) hosts a narrow bank down its right flank; a wide
        // one (regulator) a row along its top-right. The grid is rigid either
        // way — collisions slide the WHOLE block.
        let per_row = if anchor_half.y > anchor_half.x {
            2.min(caps.len().max(1))
        } else {
            caps.len().clamp(1, 4)
        };
        // Row gap carries the rail glyph plus its VALUE text (a long +3V3A
        // name collides with the next row's refdes at a bare lead).
        let row_h = h.y * 2.0 + LEAD + if rail_chars >= 4 { 2.54 } else { 0.0 };
        let y0 = -anchor_half.y + h.y;
        let x0 = anchor_half.x + LEAD + h.x;
        let build = |at: Point2| -> Vec<SatPlace> {
            caps.iter()
                .enumerate()
                .map(|(k, &(p, angle))| SatPlace {
                    item: p,
                    offset: Point2::new(
                        snap(at.x + (k % per_row) as f64 * cap_pitch),
                        snap(at.y + (k / per_row) as f64 * row_h),
                    ),
                    angle,
                })
                .collect()
        };
        let run_of = |_at: Point2| (i64::MIN / 2 + *mi as i64, 0.0, 0.0, None);
        let module = &mut form.modules[*mi];
        commit_free(
            items,
            claims.entry(*mi).or_default(),
            runs.entry(*mi).or_default(),
            module,
            build,
            run_of,
            Point2::new(x0, y0),
            Point2::new(PITCH, 0.0),
        );
        for &ci in chains {
            form.consumed.insert(ci, *mi);
        }
    }

    // ── Recompute envelopes from FULL placement rects (body + text), padded for
    // wiring: leg stubs, power glyphs below ground legs / above supply taps, and
    // label pennants all need air.
    // 3 grid: label boxes and pin strips are explicit claims now — the old
    // 4-grid margin double-counted them; 2 grid lets router elbows clip
    // neighbour bodies (mcp1703).
    const WIRE_MARGIN: f64 = 3.81;
    for (mi, m) in form.modules.iter_mut().enumerate() {
        let anchor_rect = sch_floorplan::contract::item_rect(&items[m.anchor], [0.0, 0.0]);
        m.env_min.x = anchor_rect.min_x;
        m.env_min.y = anchor_rect.min_y;
        m.env_max.x = anchor_rect.max_x;
        m.env_max.y = anchor_rect.max_y;
        let sat_rects = m.sats.iter().map(|s| placed_rect(items, s));
        let pin_text = label_boxes.get(&mi).into_iter().flatten().copied();
        for r in sat_rects.chain(pin_text) {
            m.env_min.x = m.env_min.x.min(r.min_x);
            m.env_min.y = m.env_min.y.min(r.min_y);
            m.env_max.x = m.env_max.x.max(r.max_x);
            m.env_max.y = m.env_max.y.max(r.max_y);
        }
        m.env_min.x -= WIRE_MARGIN;
        m.env_min.y -= WIRE_MARGIN;
        m.env_max.x += WIRE_MARGIN;
        m.env_max.y += WIRE_MARGIN;
    }
    form
}
