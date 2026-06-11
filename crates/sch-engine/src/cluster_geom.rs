//! Closed-form cluster geometry: turn grammar shapes into local placements,
//! wires, junctions, power ports, and labels. No search, no routing, no I/O.
//!
//! Local coordinate frame: y grows DOWN (sheet convention). Later tasks add a
//! `layout_cluster` that normalizes and composes these primitives.

use std::collections::BTreeSet;

use circuit_lang::model::{Block, Component, NetName, PinTarget, RefDes};
use indexmap::IndexMap;

use crate::emit::Dir;
use crate::grammar::Bank;
use crate::grid::snap_point;

/// Horizontal pitch between bank members, mm.
pub const BANK_PITCH: f64 = 7.62;
/// Bus offset beyond the outermost pin ends, mm.
const BUS_DROP: f64 = 2.54;

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
}
