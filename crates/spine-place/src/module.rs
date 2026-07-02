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
    Ladder { chain: usize, anchor: usize, pin: String, a_near: bool, up: bool },
    /// Vertical run between two pins of the SAME anchor (chain `a` end at `pin_a`).
    Bridge { chain: usize, anchor: usize, pin_a: String, pin_b: String },
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
                    attach.push(Attach::Ladder { chain: ci, anchor, pin, a_near, up });
                }
            }
            ChainRole::Series => {
                let (ra, rb) = (resolve(&c.a), resolve(&c.b));
                if let (Some((ia, pa)), Some((ib, pb))) = (ra, rb)
                    && ia == ib
                    && pa != pb
                {
                    attach.push(Attach::Bridge { chain: ci, anchor: ia, pin_a: pa, pin_b: pb });
                }
                // Different anchors: spine content (inter-module chain).
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
                && items[a].geom.pins.len() > items[i].geom.pins.len()
            {
                cluster_adopts.push((i, a, host_pins));
            }
        }
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
    let mut runs: BTreeMap<usize, Vec<(i64, f64, f64)>> = BTreeMap::new();
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
                7.62
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
        runs: &mut Vec<(i64, f64, f64)>,
        module: &mut ModulePlan,
        build: impl Fn(Point2) -> Vec<SatPlace>,
        run_of: impl Fn(Point2) -> (i64, f64, f64),
        at0: Point2,
        step: Point2,
    ) -> bool {
        commit_free_opt(items, claims, runs, module, build, run_of, at0, step, 64, true)
    }

    /// `commit_free` with a slide budget and optional give-up: `must == false`
    /// returns false instead of force-committing a colliding fallback.
    #[allow(clippy::too_many_arguments)]
    fn commit_free_opt(
        items: &[Item],
        claims: &mut Vec<geom::Rect>,
        runs: &mut Vec<(i64, f64, f64)>,
        module: &mut ModulePlan,
        build: impl Fn(Point2) -> Vec<SatPlace>,
        run_of: impl Fn(Point2) -> (i64, f64, f64),
        at0: Point2,
        step: Point2,
        budget: usize,
        must: bool,
    ) -> bool {
        let mut at = at0;
        for tries in 0..budget {
            let sats = build(at);
            let body: Vec<geom::Rect> = sats.iter().map(|s| placed_rect(items, s)).collect();
            let (ck, lo, hi) = run_of(at);
            let blocked = body
                .iter()
                .any(|r| claims.iter().any(|c| c.overlaps(r)))
                || runs
                    .iter()
                    .any(|&(k, rlo, rhi)| k == ck && lo < rhi && rlo < hi);
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
                runs.push((ck, lo, hi));
                module.sats.extend(sats);
                return true;
            }
            at = Point2::new(at.x + step.x, at.y + step.y);
        }
        if !must {
            return false;
        }
        let sats = build(at0);
        claims.extend(sats.iter().map(|s| placed_rect(items, s)));
        runs.push(run_of(at0));
        module.sats.extend(sats);
        true
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
            let run_of = |at: Point2| {
                (1_000_000 + (snap(at.y) / GRID).round() as i64, pa.x.min(pb.x), pa.x.max(pb.x))
            };
            let module = &mut form.modules[mi];
            commit_free(
                items,
                claims.entry(mi).or_default(),
                runs.entry(mi).or_default(),
                module,
                build,
                run_of,
                at0,
                Point2::new(0.0, -PITCH),
            );
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
        let run_of = |at: Point2| ((snap(at.x) / GRID).round() as i64, top, bot);
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
        form.consumed.insert(*chain, mi);
    }

    // ── Ladders: vertical legs at the pin column, up to supplies, down to ground.
    // Processing order decides who gets the near column. An UP leg's body spans
    // the rows of pins ABOVE its own, so those pins must take NEARER columns:
    // up-legs go top-pin-first, down-legs bottom-pin-first — then every
    // horizontal approach wire stops short of the farther legs' bodies.
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
        let Attach::Ladder { chain, anchor, pin, a_near, up } = at else {
            continue;
        };
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
        let run_of = |at: Point2| {
            let (lo, hi) = if *up {
                (y_start - leg_len, pin_at.y)
            } else {
                (pin_at.y, y_start + leg_len)
            };
            ((snap(at.x) / GRID).round() as i64, lo, hi)
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
        form.consumed.insert(*chain, mi);
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
        let run_of = |at: Point2| match side {
            PinSide::East | PinSide::West => {
                (1_000_000 + (snap(pin_at.y) / GRID).round() as i64,
                 pin_at.x.min(at.x), pin_at.x.max(at.x))
            }
            _ => ((snap(pin_at.x) / GRID).round() as i64,
                  pin_at.y.min(at.y), pin_at.y.max(at.y)),
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
        if !ok {
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
        let run_of = |_at: Point2| (i64::MIN / 4 + item_i as i64, 0.0, 0.0);
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
        if !ok {
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
                (p, orient_for(&items[p], &supply_net, Orient::Down))
            })
            .collect();
        let h = half_size(&items[caps[0].0], caps[0].1);
        // A tall anchor (MCU) hosts a narrow bank down its right flank; a wide
        // one (regulator) a row along its top-right. The grid is rigid either
        // way — collisions slide the WHOLE block.
        let per_row = if anchor_half.y > anchor_half.x {
            2.min(caps.len().max(1))
        } else {
            caps.len().clamp(1, 4)
        };
        let row_h = h.y * 2.0 + LEAD;
        let y0 = -anchor_half.y + h.y;
        let x0 = anchor_half.x + LEAD + h.x;
        let build = |at: Point2| -> Vec<SatPlace> {
            caps.iter()
                .enumerate()
                .map(|(k, &(p, angle))| SatPlace {
                    item: p,
                    offset: Point2::new(
                        snap(at.x + (k % per_row) as f64 * CAP_PITCH),
                        snap(at.y + (k / per_row) as f64 * row_h),
                    ),
                    angle,
                })
                .collect()
        };
        let run_of = |_at: Point2| (i64::MIN / 2 + *mi as i64, 0.0, 0.0);
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
    const WIRE_MARGIN: f64 = 5.08;
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
