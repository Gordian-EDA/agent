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
use sch_place::netclass::PinSide;

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
    _inc: &Incidence,
    classes: &BTreeMap<String, NetClass>,
    g: &Reduced,
    anchors: &[usize],
) -> ModuleForm {
    let mut form = ModuleForm::default();
    let class = |net: &str| *classes.get(net).unwrap_or(&NetClass::Signal);

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

    // ── Seed one module per anchor.
    let mut mod_of_anchor: BTreeMap<usize, usize> = BTreeMap::new();
    for &a in anchors {
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
    // never sit in those strips (the "symbol overlaps label" lint).
    let mut label_boxes: BTreeMap<usize, Vec<geom::Rect>> = BTreeMap::new();
    for (&a, &mi) in &mod_of_anchor {
        for (num, _name, net) in &items[a].pins {
            let Some(net) = net else { continue };
            if attached_pins.contains(&(a, num.clone())) {
                continue;
            }
            let at = pin_offset(&items[a], num, 0.0);
            let is_rail = class(net).is_rail();
            let text = if is_rail { 7.62 } else { 2.54 + 1.4 * net.chars().count() as f64 };
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
    ) {
        let mut at = at0;
        for _ in 0..64 {
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
                claims.extend(body);
                runs.push((ck, lo, hi));
                module.sats.extend(sats);
                return;
            }
            at = Point2::new(at.x + step.x, at.y + step.y);
        }
        let sats = build(at0);
        claims.extend(sats.iter().map(|s| placed_rect(items, s)));
        runs.push(run_of(at0));
        module.sats.extend(sats);
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
    let mut bank_count: BTreeMap<usize, usize> = BTreeMap::new();
    /// Caps per bank row before wrapping to a second row.
    const BANK_ROW: usize = 6;
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

        let mi = mod_of_anchor[&anchor];
        let p = c.parts[0];
        let item = &items[p];
        // Vertical cap, supply pin up, banked in rows off the anchor's top-right
        // corner (fixed anchor extents, so the bank origin never compounds).
        let supply_net = if class(&c.a.net) == NetClass::Supply {
            c.nets.first().unwrap().clone()
        } else {
            c.nets.last().unwrap().clone()
        };
        let angle = orient_for(item, &supply_net, Orient::Down);
        let h = half_size(item, angle);
        let anchor_half = half_size(&items[anchor], 0.0);
        // Bank grid: slots advance rightward at CAP_PITCH and WRAP into a new
        // row past the width budget, so a many-cap MCU gets a 2-3 row bank
        // instead of a sheet-wide strip.
        const CAP_PITCH: f64 = 12.7;
        let bank_x0 = anchor_half.x + LEAD + h.x;
        let bank_w = (anchor_half.x * 2.0).max(CAP_PITCH * 4.0);
        let row_h = h.y * 2.0 + LEAD * 2.0;
        let mut slot = *bank_count.entry(mi).or_default();
        let cl = claims.entry(mi).or_default();
        let module = &mut form.modules[mi];
        for _ in 0..64 {
            let per_row = (bank_w / CAP_PITCH).max(1.0) as usize;
            let x = bank_x0 + (slot % per_row) as f64 * CAP_PITCH;
            let y = -anchor_half.y + h.y + (slot / per_row) as f64 * row_h;
            let sat = SatPlace { item: p, offset: Point2::new(snap(x), snap(y)), angle };
            let r = placed_rect(items, &sat);
            if !cl.iter().any(|c| c.overlaps(&r)) {
                cl.push(r);
                module.sats.push(sat);
                break;
            }
            slot += 1;
        }
        bank_count.insert(mi, slot + 1);
        form.consumed.insert(ci, mi);
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
