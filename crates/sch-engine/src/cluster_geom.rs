//! Closed-form cluster geometry: turn grammar shapes into local placements,
//! wires, junctions, power ports, and labels. No search, no routing, no I/O.
//!
//! Local coordinate frame: y grows DOWN (sheet convention). Later tasks add a
//! `layout_cluster` that normalizes and composes these primitives.

use std::collections::BTreeSet;

use circuit_lang::model::{Block, Component, NetName, PinTarget, RefDes};
use indexmap::IndexMap;

use crate::emit::Dir;
use crate::grammar::{Bank, Chain, ChainClass, Cluster, is_ground};
use crate::grid::snap_point;

/// Horizontal pitch between bank members, mm.
pub const BANK_PITCH: f64 = 7.62;
/// Bus offset beyond the outermost pin ends, mm.
const BUS_DROP: f64 = 2.54;
/// Horizontal pitch between node hang slots, mm.
pub const SLOT_PITCH: f64 = 10.16;
/// Padding around cluster content after normalize, mm.
const MARGIN: f64 = 5.08;

/// Sheet-space pin-end offset (relative to symbol origin, y down) for
/// `(refdes, pin)` at instance angle 0. `None` for unknown pins.
pub type PinEndFn<'a> = &'a dyn Fn(&str, &str) -> Option<[f64; 2]>;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct ClusterGeom {
    /// (refdes, local symbol-origin position, angle degrees).
    pub placements: Vec<(RefDes, [f64; 2], f64)>,
    /// Local wire segments with their net.
    pub wires: Vec<([f64; 2], [f64; 2], NetName)>,
    /// Junction dots at deliberate ≥3-way joins.
    pub junctions: Vec<[f64; 2]>,
    /// Power ports: (net, attach point). Lib/orientation derive from the net.
    pub ports: Vec<(NetName, [f64; 2])>,
    /// Net labels: (net, position, direction). At most one per net.
    pub labels: Vec<(NetName, [f64; 2], Dir)>,
    /// Tap points usable for pin anchoring: net -> local point.
    pub tap_points: IndexMap<NetName, [f64; 2]>,
    /// Pins whose connectivity this geometry fully expresses.
    pub covered: BTreeSet<(RefDes, String)>,
    /// `[w, h]` after `normalize` (Task 7); zero until then.
    pub envelope: [f64; 2],
}

/// Rotate an angle-0 sheet-space pin end to instance `angle` (0/90/180/270).
fn rotate_end0(end0: [f64; 2], angle: f64) -> [f64; 2] {
    // end0 = transform_offset(local, 0) = [lx, -ly]  =>  local = [end0.x, -end0.y].
    crate::emit::transform_offset([end0[0], -end0[1]], angle, false)
}

fn add2(a: [f64; 2], b: [f64; 2]) -> [f64; 2] {
    [a[0] + b[0], a[1] + b[1]]
}

/// The pin of `comp` on `net`, and the other pin. Panics on non-chain shapes
/// (callers only pass chain elements / bank members).
fn pin_split(comp: &Component, net: &str) -> (String, String) {
    let mut on = None;
    let mut other = None;
    for (pin, target) in &comp.pins {
        match target {
            PinTarget::Net(n) if n == net && on.is_none() => on = Some(pin.clone()),
            PinTarget::Net(_) | PinTarget::NoConnect => other = Some(pin.clone()),
        }
    }
    (on.expect("pin on net"), other.expect("other pin"))
}

/// Stack a chain pin-to-pin from `start`, downward (+y) or upward (−y). Each
/// link's a-end coincides with the running point (pin-coincident joins — no
/// wires needed). Returns the final b-end and the inter-link joint points
/// `(through-net, point)`.
pub fn stack_chain(
    g: &mut ClusterGeom,
    links: &[crate::grammar::Link],
    start: [f64; 2],
    down: bool,
    pins: PinEndFn,
) -> ([f64; 2], Vec<(NetName, [f64; 2])>) {
    let mut p = start;
    let mut joints = Vec::new();
    for (i, l) in links.iter().enumerate() {
        let ea = pins(&l.refdes, &l.a_pin).unwrap_or([0.0, -3.81]);
        let eb = pins(&l.refdes, &l.b_pin).unwrap_or([0.0, 3.81]);
        // Choose 0/180 so the a-end sits on the approach side: stacking down
        // wants the rotated a-end ABOVE the b-end, stacking up the reverse.
        let natural = ea[1] < eb[1]; // a is the upper pin at angle 0
        let angle = if natural == down { 0.0 } else { 180.0 };
        let ra = rotate_end0(ea, angle);
        let rb = rotate_end0(eb, angle);
        let center = snap_point([p[0] - ra[0], p[1] - ra[1]]);
        g.placements.push((l.refdes.clone(), center, angle));
        g.covered.insert((l.refdes.clone(), l.a_pin.clone()));
        g.covered.insert((l.refdes.clone(), l.b_pin.clone()));
        p = snap_point(add2(center, rb));
        if i + 1 < links.len() {
            joints.push((l.b_net.clone(), p));
        }
    }
    (p, joints)
}

/// Bused bank: members side by side, shared top/bottom bus, one port per bus.
pub fn emit_bank(
    g: &mut ClusterGeom,
    bank: &Bank,
    origin: [f64; 2],
    block: &Block,
    pins: PinEndFn,
) {
    let n = bank.members.len();
    let mut top_pts = Vec::with_capacity(n);
    let mut bot_pts = Vec::with_capacity(n);
    for (i, refdes) in bank.members.iter().enumerate() {
        let comp = &block.components[refdes.as_str()];
        let (a_pin, b_pin) = pin_split(comp, &bank.a_net);
        let ea = pins(refdes, &a_pin).unwrap_or([0.0, -3.81]);
        let eb = pins(refdes, &b_pin).unwrap_or([0.0, 3.81]);
        let angle = if ea[1] < eb[1] { 0.0 } else { 180.0 }; // a-net pin up
        let center = snap_point([origin[0] + i as f64 * BANK_PITCH, origin[1]]);
        g.placements.push((refdes.clone(), center, angle));
        top_pts.push(snap_point(add2(center, rotate_end0(ea, angle))));
        bot_pts.push(snap_point(add2(center, rotate_end0(eb, angle))));
        g.covered.insert((refdes.clone(), a_pin));
        g.covered.insert((refdes.clone(), b_pin));
    }
    let bus_top = top_pts.iter().map(|p| p[1]).fold(f64::MAX, f64::min) - BUS_DROP;
    let bus_bot = bot_pts.iter().map(|p| p[1]).fold(f64::MIN, f64::max) + BUS_DROP;
    for (pts, bus_y, net) in [
        (&top_pts, bus_top, &bank.a_net),
        (&bot_pts, bus_bot, &bank.b_net),
    ] {
        for (i, p) in pts.iter().enumerate() {
            g.wires.push((*p, [p[0], bus_y], net.clone()));
            if i + 1 < n {
                g.junctions.push([p[0], bus_y]);
            }
        }
        g.wires.push(([pts[0][0], bus_y], [pts[n - 1][0], bus_y], net.clone()));
        g.ports.push((net.clone(), [pts[0][0], bus_y]));
    }
    g.tap_points.insert(bank.a_net.clone(), [top_pts[0][0], bus_top]);
}

/// Body extents `[w, h]` per refdes (for envelope/normalize).
pub type SizeFn<'a> = &'a dyn Fn(&str) -> [f64; 2];

/// Lay one cluster into LOCAL geometry (y grows down), `normalize`d before
/// return so all coordinates are ≥ MARGIN and `envelope` is filled. `labeled`
/// is the set of nets that must carry exactly one net label somewhere in this
/// cluster (anchor-tapped, cross-block, or multi-way nodes), placed at the
/// net's node/joint point. `sizes(refdes)` returns `[w, h]` body extents for
/// envelope computation. Clusters with a Series spine are not yet supported
/// (Task 8 — `lay_spine` panics).
pub fn layout_cluster(
    cluster: &Cluster,
    block: &Block,
    labeled: &BTreeSet<NetName>,
    pins: PinEndFn,
    sizes: SizeFn,
) -> ClusterGeom {
    let mut g = ClusterGeom::default();
    let mut joints: Vec<(NetName, [f64; 2])> = Vec::new();
    let mut node_x: IndexMap<NetName, f64> = IndexMap::new();
    let mut node_slot: IndexMap<NetName, (f64, f64)> = IndexMap::new(); // (next down x, next up x)
    // Next free x column for standalone items; monotonic high-water mark.
    let mut x_cursor = 0.0_f64;
    // Row cursor for non-spine Series chains, dropping below the main content.
    let mut y_secondary = 40.0_f64;

    // Spine: the longest Series chain (Task 8 lays it; absent here = star case).
    let spine = cluster
        .chains
        .iter()
        .enumerate()
        .filter(|(_, c)| c.class == ChainClass::Series)
        .max_by(|(ia, a), (ib, b)| a.links.len().cmp(&b.links.len()).then(ib.cmp(ia)))
        .map(|(i, _)| i);
    if let Some(si) = spine {
        lay_spine(&mut g, &cluster.chains[si], &mut node_x, &mut joints, pins);
    }

    for (i, chain) in cluster.chains.iter().enumerate() {
        if Some(i) == spine {
            continue;
        }
        match chain.class {
            ChainClass::ToRail => {
                let node = chain.start_net().to_string();
                let down = is_ground(chain.end_net());
                let nx = *node_x.entry(node.clone()).or_insert_with(|| {
                    let x = x_cursor;
                    x_cursor += SLOT_PITCH;
                    x
                });
                let slot = node_slot.entry(node.clone()).or_insert((nx, nx));
                let x = if down { slot.0 } else { slot.1 };
                if down {
                    slot.0 += SLOT_PITCH;
                } else {
                    slot.1 += SLOT_PITCH;
                }
                if x != nx {
                    g.wires.push(([nx.min(x), 0.0], [nx.max(x), 0.0], node.clone()));
                    g.junctions.push([x, 0.0]);
                }
                let (end, js) = stack_chain(&mut g, &chain.links, [x, 0.0], down, pins);
                joints.extend(js);
                g.ports.push((chain.end_net().to_string(), end));
                g.tap_points.entry(node.clone()).or_insert([nx, 0.0]);
                x_cursor = x_cursor.max(x + SLOT_PITCH);
            }
            ChainClass::RailRail => {
                let x = x_cursor;
                x_cursor += SLOT_PITCH;
                let top = [x, 0.0];
                g.ports.push((chain.start_net().to_string(), top));
                let (end, js) = stack_chain(&mut g, &chain.links, top, true, pins);
                joints.extend(js);
                g.ports.push((chain.end_net().to_string(), end));
            }
            ChainClass::Series => {
                // A non-spine Series chain (rare): lay it horizontally below the
                // main content. Each subsequent one drops another row.
                let (_, js) = lay_horizontal(&mut g, &chain.links, [0.0, y_secondary], pins, &mut node_x);
                joints.extend(js);
                y_secondary += 20.0;
            }
        }
    }

    for bank in &cluster.banks {
        let origin = match node_x.get(&bank.a_net) {
            Some(&nx) => [nx, BUS_DROP + 3.81],
            None => {
                let x = x_cursor;
                x_cursor += bank.members.len() as f64 * BANK_PITCH + SLOT_PITCH;
                [x, 0.0]
            }
        };
        emit_bank(&mut g, bank, origin, block, pins);
    }

    // One label per labeled net, at its node/joint point, past the last slot.
    let mut points: IndexMap<NetName, [f64; 2]> = IndexMap::new();
    for (net, &x) in &node_x {
        points.insert(net.clone(), [x, 0.0]);
    }
    for (net, p) in &joints {
        points.entry(net.clone()).or_insert(*p);
    }
    let spine_start: Option<NetName> = spine.map(|si| cluster.chains[si].start_net().to_string());
    for net in labeled {
        if let Some(&p) = points.get(net) {
            // The spine's start net labels to the LEFT (signal arrives from the
            // left); everything else labels to the right of its node.
            let west = spine_start.as_deref() == Some(net.as_str());
            let (lp, dir) = if west {
                ([p[0] - SLOT_PITCH, p[1]], Dir::West)
            } else {
                let ext = node_slot.get(net).map(|s| s.0.max(s.1)).unwrap_or(p[0] + SLOT_PITCH);
                ([ext.max(p[0] + SLOT_PITCH), p[1]], Dir::East)
            };
            g.wires.push(([p[0].min(lp[0]), p[1]], [p[0].max(lp[0]), p[1]], net.clone()));
            g.labels.push((net.clone(), lp, dir));
            g.tap_points.entry(net.clone()).or_insert(p);
        }
    }

    normalize(&mut g, sizes);
    g
}

/// Translate all geometry so the bbox min corner lands at (MARGIN, MARGIN);
/// fill `envelope`.
fn normalize(g: &mut ClusterGeom, sizes: SizeFn) {
    let mut min = [f64::MAX, f64::MAX];
    let mut max = [f64::MIN, f64::MIN];
    let mut grow = |p: [f64; 2], half: [f64; 2]| {
        min[0] = min[0].min(p[0] - half[0]);
        min[1] = min[1].min(p[1] - half[1]);
        max[0] = max[0].max(p[0] + half[0]);
        max[1] = max[1].max(p[1] + half[1]);
    };
    for (refdes, at, _) in &g.placements {
        let s = sizes(refdes);
        grow(*at, [s[0] / 2.0, s[1] / 2.0]);
    }
    for (a, b, _) in &g.wires {
        grow(*a, [0.0; 2]);
        grow(*b, [0.0; 2]);
    }
    for (_, p) in &g.ports {
        grow(*p, [2.54, 5.08]);
    }
    for (_, p, _) in &g.labels {
        grow(*p, [12.7, 1.27]);
    }
    // junctions and tap_points always sit on wires/nodes, so they can't be the
    // sole content; ports and labels can, and are translated below, so they must
    // also gate the early return.
    if g.placements.is_empty() && g.wires.is_empty() && g.ports.is_empty() && g.labels.is_empty() {
        g.envelope = [0.0, 0.0];
        return;
    }
    let d = [MARGIN - min[0], MARGIN - min[1]];
    let t = |p: [f64; 2]| [p[0] + d[0], p[1] + d[1]];
    for (_, at, _) in &mut g.placements {
        *at = t(*at);
    }
    for (a, b, _) in &mut g.wires {
        *a = t(*a);
        *b = t(*b);
    }
    for j in &mut g.junctions {
        *j = t(*j);
    }
    for (_, p) in &mut g.ports {
        *p = t(*p);
    }
    for (_, p, _) in &mut g.labels {
        *p = t(*p);
    }
    for p in g.tap_points.values_mut() {
        *p = t(*p);
    }
    g.envelope = [max[0] - min[0] + 2.0 * MARGIN, max[1] - min[1] + 2.0 * MARGIN];
}

/// Lay the primary Series chain horizontally at y=0, left→right, elements at
/// 90°/270° (vertical bodies turned horizontal), pin-coincident joins.
/// Registers each through-net's x position in `node_x` so hangs/banks attach
/// at the right column.
fn lay_spine(
    g: &mut ClusterGeom,
    chain: &Chain,
    node_x: &mut IndexMap<NetName, f64>,
    joints: &mut Vec<(NetName, [f64; 2])>,
    pins: PinEndFn,
) {
    let (end, js) = lay_horizontal(g, &chain.links, [0.0, 0.0], pins, node_x);
    joints.extend(js);
    node_x.insert(chain.start_net().to_string(), 0.0);
    node_x.insert(chain.end_net().to_string(), end[0]);
}

/// Place a chain's elements in a horizontal row left→right starting at `start`,
/// each at 90°/270° so its body lies horizontal and its a-end is on the left.
/// Registers each interior through-net's x in `node_x`. Returns the final b-end
/// and the inter-link joints.
fn lay_horizontal(
    g: &mut ClusterGeom,
    links: &[crate::grammar::Link],
    start: [f64; 2],
    pins: PinEndFn,
    node_x: &mut IndexMap<NetName, f64>,
) -> ([f64; 2], Vec<(NetName, [f64; 2])>) {
    let mut p = start;
    let mut joints = Vec::new();
    for (i, l) in links.iter().enumerate() {
        let ea = pins(&l.refdes, &l.a_pin).unwrap_or([0.0, -3.81]);
        let eb = pins(&l.refdes, &l.b_pin).unwrap_or([0.0, 3.81]);
        // Pick 90/270 so the rotated a-end sits on the LEFT (approach side).
        let angle = if rotate_end0(ea, 90.0)[0] < rotate_end0(eb, 90.0)[0] {
            90.0
        } else {
            270.0
        };
        let ra = rotate_end0(ea, angle);
        let rb = rotate_end0(eb, angle);
        let center = snap_point([p[0] - ra[0], p[1] - ra[1]]);
        g.placements.push((l.refdes.clone(), center, angle));
        g.covered.insert((l.refdes.clone(), l.a_pin.clone()));
        g.covered.insert((l.refdes.clone(), l.b_pin.clone()));
        p = snap_point(add2(center, rb));
        if i + 1 < links.len() {
            joints.push((l.b_net.clone(), p));
            node_x.insert(l.b_net.clone(), p[0]);
        }
    }
    (p, joints)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grammar::{Bank, Link};

    /// Mock pin ends: every part is a vertical 2-pin passive, pin 1 up.
    pub(super) fn mock_pins(_refdes: &str, pin: &str) -> Option<[f64; 2]> {
        match pin {
            "1" => Some([0.0, -3.81]),
            "2" => Some([0.0, 3.81]),
            _ => None,
        }
    }

    pub(super) fn link(refdes: &str, a_pin: &str, a_net: &str, b_pin: &str, b_net: &str) -> Link {
        Link {
            refdes: refdes.into(),
            a_pin: a_pin.into(),
            a_net: a_net.into(),
            b_pin: b_pin.into(),
            b_net: b_net.into(),
        }
    }

    #[test]
    fn rotate_end0_inverts_y_and_rotates() {
        // angle 0 is identity; 180 flips the sheet-space y end.
        let id = rotate_end0([0.0, -3.81], 0.0);
        assert!((id[0]).abs() < 1e-9 && (id[1] + 3.81).abs() < 1e-9, "{id:?}");
        let flipped = rotate_end0([0.0, -3.81], 180.0);
        assert!((flipped[0]).abs() < 1e-9 && (flipped[1] - 3.81).abs() < 1e-9, "{flipped:?}");
    }

    #[test]
    fn stack_chain_places_pin_to_pin_downward() {
        let mut g = ClusterGeom::default();
        // D1 oriented a=pin2 (needs 180°), R2 a=pin1 (0°).
        let links = vec![
            link("D1", "2", "3V3", "1", "MID"),
            link("R2", "1", "MID", "2", "GND"),
        ];
        let (end, joints) = stack_chain(&mut g, &links, [0.0, 0.0], true, &mock_pins);
        assert_eq!(g.placements[0], ("D1".to_string(), [0.0, 3.81], 180.0));
        assert_eq!(g.placements[1], ("R2".to_string(), [0.0, 11.43], 0.0));
        assert_eq!(end, [0.0, 15.24]);
        assert_eq!(joints, vec![("MID".to_string(), [0.0, 7.62])]);
        assert_eq!(g.covered.len(), 4);
    }

    #[test]
    fn divider_star_hangs_up_and_down_with_node_wire_and_label() {
        let d = crate::grammar::tests::compile(
            "
version: 1
name: t
rails: [VCC, GND]
blocks:
  a:
    components:
      R7: {part: Device:R, value: 649k, between: [VCC, OUT]}
      R8: {part: Device:R, value: 200k, between: [OUT, GND]}
      C3: {part: Device:C, value: 47n, between: [OUT, GND]}
",
        );
        let g = crate::grammar::analyze(&d, "a", &crate::grammar::tests::provider());
        assert_eq!(g.clusters.len(), 1);
        let mut labeled = std::collections::BTreeSet::new();
        labeled.insert("OUT".to_string());
        let geom = layout_cluster(&g.clusters[0], &d.blocks["a"], &labeled, &mock_pins, &|_| {
            [5.08, 10.16]
        });

        let pos = |r: &str| {
            geom.placements.iter().find(|(refdes, _, _)| refdes == r).map(|(_, at, _)| *at).unwrap()
        };
        // STRUCTURAL INTENT (adjust the exact refdes below to the TRUE
        // deterministic order — see the note after this test):
        // R7 is the only UP hang; two DOWN hangs sit side by side one SLOT_PITCH
        // apart sharing a row; the up hang shares the node x with the first
        // down hang.
        let up = pos("R7");
        let downs = [pos("R8"), pos("C3")];
        // exactly the two down-hangs share a y, and R7 is above them:
        assert_eq!(downs[0][1], downs[1][1], "down hangs share a row");
        assert!(up[1] < downs[0][1], "R7 hangs up, above the down hangs");
        // the two down-hangs are one SLOT_PITCH apart on x:
        let dx = (downs[0][0] - downs[1][0]).abs();
        assert_eq!(dx, SLOT_PITCH, "down hangs are one slot apart");
        // the up hang shares x with the LEFT (node-x) down hang:
        let left_down = downs[0][0].min(downs[1][0]);
        assert_eq!(up[0], left_down, "up hang shares node x with the left down hang");

        // Ports: one VCC (up), two GND (one per down-hang).
        assert_eq!(geom.ports.iter().filter(|(n, _)| n == "VCC").count(), 1);
        assert_eq!(geom.ports.iter().filter(|(n, _)| n == "GND").count(), 2);
        // Node wire on OUT, one junction at the offset tap, OUT label reads East.
        assert!(geom.wires.iter().any(|(_, _, n)| n == "OUT"));
        assert_eq!(geom.junctions.len(), 1);
        let label = geom.labels.iter().find(|(n, _, _)| n == "OUT").unwrap();
        assert!(matches!(label.2, Dir::East));
        // Normalized: nothing at negative coordinates, envelope positive.
        assert!(geom.envelope[0] > 0.0 && geom.envelope[1] > 0.0);
        for (_, at, _) in &geom.placements {
            assert!(at[0] >= 0.0 && at[1] >= 0.0, "normalized: {at:?}");
        }
    }

    #[test]
    fn bank_has_buses_single_ports_and_junctions() {
        let d = crate::grammar::tests::compile(
            "
version: 1
name: t
rails: [3V3, GND]
blocks:
  a:
    components:
      C1: {part: Device:C, value: 100n, between: [3V3, GND]}
      C2: {part: Device:C, value: 100n, between: [3V3, GND]}
      C3: {part: Device:C, value: 100n, between: [GND, 3V3]}
",
        );
        let block = &d.blocks["a"];
        let bank = Bank {
            a_net: "3V3".into(),
            b_net: "GND".into(),
            members: vec!["C1".into(), "C2".into(), "C3".into()],
        };
        let mut g = ClusterGeom::default();
        emit_bank(&mut g, &bank, [0.0, 0.0], block, &mock_pins);

        assert_eq!(g.placements.len(), 3);
        assert_eq!(g.placements[0].1[0] + BANK_PITCH, g.placements[1].1[0]);
        assert_eq!(g.placements[2].2, 180.0, "C3 flips so its 3V3 pin is up");
        assert_eq!(g.ports.len(), 2);
        let nets: Vec<&str> = g.ports.iter().map(|(n, _)| n.as_str()).collect();
        assert!(nets.contains(&"3V3") && nets.contains(&"GND"));
        assert_eq!(g.wires.len(), 8); // 6 stubs + 2 bus wires
        assert_eq!(g.junctions.len(), 4); // interior+port member on both buses
        assert_eq!(g.covered.len(), 6);
    }

    #[test]
    fn power_entry_spine_runs_left_to_right_with_riser_and_bank_below() {
        // F1 (BUS_5V -> N_VI) is a Series spine; bank {C1,C2} hangs at node N_VI.
        let d = crate::grammar::tests::compile(
            "
version: 1
name: t
rails: [5V_IN, GND]
blocks:
  a:
    components:
      F1: {part: Device:R, value: Polyfuse, between: [BUS_5V, N_VI]}
      C1: {part: Device:C, value: 10u, between: [N_VI, GND]}
      C2: {part: Device:C, value: 1u, between: [N_VI, GND]}
      U1:
        part: Mock:REG
        pins: {VI: N_VI, GND: GND, VO: N_VO, EN: N_VI}
  ext:
    components:
      R9: {part: Device:R, value: 1k, between: [BUS_5V, GND]}
",
        );
        let g = crate::grammar::analyze(&d, "a", &crate::grammar::tests::provider());
        let cluster = g
            .clusters
            .iter()
            .find(|c| c.chains.iter().any(|ch| ch.links[0].refdes == "F1"))
            .unwrap();
        let labeled: std::collections::BTreeSet<String> =
            ["BUS_5V".to_string(), "N_VI".to_string()].into();
        let geom = layout_cluster(cluster, &d.blocks["a"], &labeled, &mock_pins, &|_| {
            [5.08, 10.16]
        });

        let f1 = geom.placements.iter().find(|(r, _, _)| r == "F1").unwrap();
        assert_eq!(f1.2, 90.0, "spine elements lie horizontal");
        let c1 = geom.placements.iter().find(|(r, _, _)| r == "C1").unwrap();
        assert!(c1.1[1] > f1.1[1], "bank hangs below the spine");
        let bus = geom.labels.iter().find(|(n, _, _)| n == "BUS_5V").unwrap();
        assert!(matches!(bus.2, Dir::West), "spine start label reads West");
        assert!(geom.labels.iter().any(|(n, _, _)| n == "N_VI"));
    }

    #[test]
    fn pin_tapped_chain_hangs_to_gnd_with_single_port() {
        // Q (anchor-tapped) -> R1 -> D1 -> GND: a ToRail hang, label at top, one
        // GND port at the bottom.
        let d = crate::grammar::tests::compile(
            "
version: 1
name: t
rails: [VCC, GND]
blocks:
  a:
    components:
      R1: {part: Device:R, value: 1k, between: [Q, MID]}
      D1: {part: Device:LED, value: red, between: [MID, GND]}
      U1:
        part: Mock:REG
        pins: {VI: VCC, GND: GND, VO: Q, EN: VCC}
",
        );
        let g = crate::grammar::analyze(&d, "a", &crate::grammar::tests::provider());
        assert_eq!(g.clusters.len(), 1);
        let labeled: std::collections::BTreeSet<String> = ["Q".to_string()].into();
        let geom = layout_cluster(&g.clusters[0], &d.blocks["a"], &labeled, &mock_pins, &|_| {
            [5.08, 10.16]
        });
        let gnd_ports: Vec<_> = geom.ports.iter().filter(|(n, _)| n == "GND").collect();
        assert_eq!(gnd_ports.len(), 1);
        let q = geom.labels.iter().find(|(n, _, _)| n == "Q").unwrap();
        let r1 = geom.placements.iter().find(|(r, _, _)| r == "R1").unwrap();
        assert!(gnd_ports[0].1[1] > r1.1[1], "GND below the chain");
        assert!(q.1[1] < gnd_ports[0].1[1], "Q label at the top of the hang");
    }
}
