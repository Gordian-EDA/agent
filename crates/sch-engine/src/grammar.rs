//! Structural layout grammar — netlist-graph classification.
//!
//! Currently covers component role classification (`Anchor` vs `ChainElement`)
//! and per-net pin incidence (`net_uses`). Will grow to cover chains, banks,
//! and clusters (Tasks 2-3). Pure analysis — no KiCAD, no I/O, no geometry.
//! Consumed by `cluster_geom` (local geometry) and `place` (macro placement).

use std::collections::BTreeSet;

use circuit_lang::model::{Block, Component, NetName, PinTarget, RefDes};
use circuit_lang::{Design, SymbolProvider};
use indexmap::IndexMap;

/// Ground-ish rails hang down; everything else points up. (Shared with
/// reconcile's power-symbol orientation.)
pub fn is_ground(net: &str) -> bool {
    let n = net.to_ascii_uppercase();
    n.contains("GND") || n.starts_with("VSS")
}

/// How a component participates in layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// ≥3 physical pins (or any pin NC'd): placed and preserved individually.
    Anchor,
    /// Exactly 2 pins, both on nets: forms chains.
    ChainElement,
}

/// Classify one component. Physical pin count comes from provider metadata
/// when the part is known; the kernel pin map is the fallback (auto-NC means
/// every physical pin appears there).
pub fn role_of(comp: &Component, provider: &dyn SymbolProvider) -> Role {
    let n = provider
        .symbol(&comp.part)
        .map(|m| m.pins.len())
        .unwrap_or_else(|| {
            comp.pins.len() + comp.units.values().map(IndexMap::len).sum::<usize>()
        });
    // `comp.pins.len() == 2` is belt-and-suspenders: when the provider is
    // unknown and `units.is_empty()`, `n` already equals `comp.pins.len()`.
    let two_nets = n == 2
        && comp.units.is_empty()
        && comp.pins.len() == 2
        && comp.pins.values().all(|t| matches!(t, PinTarget::Net(_)));
    if two_nets {
        Role::ChainElement
    } else {
        Role::Anchor
    }
}

/// Sort a vec by a natural-order string key (stable, deterministic).
fn natural_sort_by_key<T>(v: &mut [T], key: impl Fn(&T) -> String) {
    v.sort_by(|a, b| circuit_lang::canon::natural_cmp(&key(a), &key(b)));
}

/// Per-net pin incidence within one block, split by role.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct NetUse {
    /// `(refdes, pin)` of chain-element pins on this net, natural-ordered.
    pub chain_pins: Vec<(RefDes, String)>,
    /// `(refdes, pin)` of anchor pins on this net, natural-ordered.
    pub anchor_pins: Vec<(RefDes, String)>,
    /// True when the net also appears in another block (label connectivity).
    pub external: bool,
    /// True when the net is a declared power rail.
    pub power: bool,
}

/// Every pin→target of a component, across direct pins and all units.
fn all_pins(comp: &Component) -> impl Iterator<Item = (&String, &PinTarget)> {
    comp.pins.iter().chain(comp.units.values().flatten())
}

/// Build the per-net incidence map for `block_name`.
pub fn net_uses(
    design: &Design,
    block_name: &str,
    provider: &dyn SymbolProvider,
) -> IndexMap<NetName, NetUse> {
    let block = &design.blocks[block_name];
    let mut uses: IndexMap<NetName, NetUse> = IndexMap::new();
    for (refdes, comp) in &block.components {
        let role = role_of(comp, provider);
        for (pin, target) in all_pins(comp) {
            let PinTarget::Net(net) = target else { continue };
            let u = uses.entry(net.clone()).or_default();
            u.power |= design.nets.get(net).is_some_and(|a| a.power);
            match role {
                Role::ChainElement => u.chain_pins.push((refdes.clone(), pin.clone())),
                Role::Anchor => u.anchor_pins.push((refdes.clone(), pin.clone())),
            }
        }
    }
    for (other_name, other) in &design.blocks {
        if other_name == block_name {
            continue;
        }
        for comp in other.components.values() {
            for (_, target) in all_pins(comp) {
                if let PinTarget::Net(net) = target {
                    if let Some(u) = uses.get_mut(net) {
                        u.external = true;
                    }
                }
            }
        }
    }
    for u in uses.values_mut() {
        // NUL separates refdes from pin so "(R1, pin2)" can't collide with "(R, pin12)".
        natural_sort_by_key(&mut u.chain_pins, |p| format!("{}\u{0000}{}", p.0, p.1));
        natural_sort_by_key(&mut u.anchor_pins, |p| format!("{}\u{0000}{}", p.0, p.1));
    }
    uses.sort_by(|k1, _v1, k2, _v2| circuit_lang::canon::natural_cmp(k1, k2));
    uses
}

/// One chain element oriented along its chain: `a` is toward the chain start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub refdes: RefDes,
    pub a_pin: String,
    pub a_net: NetName,
    pub b_pin: String,
    pub b_net: NetName,
}

impl Link {
    /// Build a link for `refdes` entered from `from_net` (which becomes `a`).
    fn of(block: &Block, refdes: &str, from_net: &str) -> Link {
        let comp = &block.components[refdes];
        let mut nets = comp.pins.iter().filter_map(|(p, t)| match t {
            PinTarget::Net(n) => Some((p.clone(), n.clone())),
            PinTarget::NoConnect => None,
        });
        let (p1, n1) = nets.next().expect("chain element has two net pins");
        let (p2, n2) = nets.next().expect("chain element has two net pins");
        debug_assert!(n1 == from_net || n2 == from_net, "Link::of: from_net {from_net:?} is not a net of {refdes}");
        if n1 == from_net {
            Link { refdes: refdes.into(), a_pin: p1, a_net: n1, b_pin: p2, b_net: n2 }
        } else {
            Link { refdes: refdes.into(), a_pin: p2, a_net: n2, b_pin: p1, b_net: n1 }
        }
    }

    #[allow(dead_code)]
    fn reversed(&self) -> Link {
        Link {
            refdes: self.refdes.clone(),
            a_pin: self.b_pin.clone(),
            a_net: self.b_net.clone(),
            b_pin: self.a_pin.clone(),
            b_net: self.a_net.clone(),
        }
    }
}

/// A net continues a chain iff it joins exactly two chain-element pins and is
/// neither a rail nor used outside the block. Anchor pins tap such nets
/// without breaking the walk.
fn through(uses: &IndexMap<NetName, NetUse>, net: &str) -> bool {
    uses.get(net)
        .is_some_and(|u| !u.power && !u.external && u.chain_pins.len() == 2)
}

/// Walk one maximal chain starting from `start_refdes` entered via `start_net`.
fn walk(
    block: &Block,
    uses: &IndexMap<NetName, NetUse>,
    start_refdes: &str,
    start_net: &str,
    visited: &mut BTreeSet<RefDes>,
) -> Vec<Link> {
    let mut links = vec![Link::of(block, start_refdes, start_net)];
    visited.insert(start_refdes.to_string());
    loop {
        let tail = links.last().unwrap().b_net.clone();
        if !through(uses, &tail) {
            break;
        }
        let next = uses[&tail]
            .chain_pins
            .iter()
            .map(|(r, _)| r)
            .find(|r| !visited.contains(*r))
            .cloned();
        let Some(next) = next else { break };
        visited.insert(next.clone());
        links.push(Link::of(block, &next, &tail));
    }
    links
}

/// Maximal element paths plus degradation notes (cycle breaks).
///
/// Deterministic: starts are scanned in net-name natural order (non-through
/// nets only), then chain-pin natural order. Elements left unvisited can only
/// belong to pure cycles; each cycle is broken at its naturally-smallest
/// refdes and reported.
pub fn raw_chains(
    block: &Block,
    uses: &IndexMap<NetName, NetUse>,
    elements: &BTreeSet<RefDes>,
) -> (Vec<Vec<Link>>, Vec<String>) {
    let mut visited: BTreeSet<RefDes> = BTreeSet::new();
    let mut chains = Vec::new();
    let mut notes = Vec::new();

    for (net, u) in uses {
        if through(uses, net) {
            continue;
        }
        for (refdes, _) in &u.chain_pins {
            if !visited.contains(refdes) {
                chains.push(walk(block, uses, refdes, net, &mut visited));
            }
        }
    }

    let mut leftover: Vec<&RefDes> = elements.iter().filter(|r| !visited.contains(*r)).collect();
    natural_sort_by_key(&mut leftover, |r| (*r).clone());
    for refdes in leftover {
        if visited.contains(refdes) {
            continue;
        }
        let comp = &block.components[refdes.as_str()];
        let start_net = all_pins(comp)
            .find_map(|(_, t)| match t {
                PinTarget::Net(n) => Some(n.clone()),
                PinTarget::NoConnect => None,
            })
            .expect("chain element has nets");
        notes.push(format!(
            "cycle through {refdes} broken at net {start_net} (feedback loops degrade to labels)"
        ));
        chains.push(walk(block, uses, refdes, &start_net, &mut visited));
    }

    (chains, notes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use circuit_lang::{MockSymbolProvider, PinType};
    use std::collections::BTreeSet;

    pub(super) fn provider() -> MockSymbolProvider {
        let mut p = MockSymbolProvider::with_basics();
        // A 4-pin "IC": VI is a power input, VO a power output.
        p.add(
            "Mock:REG",
            vec![
                ("1", "VI", PinType::PowerInput, 1),
                ("2", "GND", PinType::PowerInput, 1),
                ("3", "VO", PinType::PowerOutput, 1),
                ("4", "EN", PinType::Other, 1),
            ],
        );
        p
    }

    pub(super) fn compile(src: &str) -> circuit_lang::Design {
        let result = circuit_lang::compile(src, &provider());
        assert!(!result.diagnostics.has_errors(), "{:?}", result.diagnostics);
        result.design.unwrap()
    }

    #[test]
    fn roles_are_inferred_from_pin_count_and_targets() {
        let d = compile(
            "
version: 1
name: t
rails: [VCC, GND]
blocks:
  a:
    components:
      R1: {part: Device:R, value: 1k, between: [VCC, OUT]}
      U1:
        part: Mock:REG
        pins: {VI: VCC, GND: GND, VO: OUT, EN: VCC}
",
        );
        let p = provider();
        let block = &d.blocks["a"];
        assert_eq!(role_of(&block.components["R1"], &p), Role::ChainElement);
        assert_eq!(role_of(&block.components["U1"], &p), Role::Anchor);
    }

    #[test]
    fn two_pin_part_with_nc_pin_is_an_anchor() {
        use circuit_lang::model::{Component, PinTarget};
        let mut c = Component::default();
        c.part = "Device:R".into();
        c.pins.insert("1".into(), PinTarget::Net("A".into()));
        c.pins.insert("2".into(), PinTarget::NoConnect);
        assert_eq!(role_of(&c, &provider()), Role::Anchor);
    }

    #[test]
    fn net_uses_collects_pins_power_and_externality() {
        let d = compile(
            "
version: 1
name: t
rails: [VCC, GND]
blocks:
  a:
    components:
      R1: {part: Device:R, value: 1k, between: [VCC, OUT]}
      R2: {part: Device:R, value: 1k, between: [OUT, GND]}
      U1:
        part: Mock:REG
        pins: {VI: VCC, GND: GND, VO: OUT, EN: VCC}
  b:
    components:
      C1: {part: Device:C, value: 1u, between: [OUT, GND]}
",
        );
        let uses = net_uses(&d, "a", &provider());
        let out = &uses["OUT"];
        assert_eq!(
            out.chain_pins,
            vec![("R1".to_string(), "2".to_string()), ("R2".to_string(), "1".to_string())]
        );
        assert_eq!(out.anchor_pins, vec![("U1".to_string(), "VO".to_string())]);
        assert!(out.external, "OUT is also used in block b");
        assert!(!out.power);
        assert!(uses["GND"].power);
    }

    #[test]
    fn net_uses_keys_in_natural_order() {
        // Lexicographic order: N10 < N2 (because '1' < '2').
        // Natural order: N2 < N10 (numeric suffix comparison: 2 < 10).
        // The map's iteration order must be natural, not lexicographic.
        let d = compile(
            "
version: 1
name: t
rails: [VCC, GND]
blocks:
  a:
    components:
      R1: {part: Device:R, value: 1k, between: [N2, N10]}
      R2: {part: Device:R, value: 1k, between: [N10, GND]}
",
        );
        let uses = net_uses(&d, "a", &provider());
        let keys: Vec<&str> = uses.keys().map(|k| k.as_str()).collect();
        // GND is a power rail; N2 and N10 are plain nets.
        // Regardless of GND's position, N2 must appear before N10.
        let pos_n2 = keys.iter().position(|&k| k == "N2").expect("N2 present");
        let pos_n10 = keys.iter().position(|&k| k == "N10").expect("N10 present");
        assert!(
            pos_n2 < pos_n10,
            "expected N2 before N10 in natural order, but got: {:?}",
            keys
        );
    }

    #[test]
    fn ground_detection() {
        assert!(is_ground("GND"));
        assert!(is_ground("AGND"));
        assert!(is_ground("VSS"));
        assert!(!is_ground("VCC"));
        assert!(!is_ground("3V3"));
    }

    #[test]
    fn unknown_part_with_two_net_pins_is_a_chain_element() {
        use circuit_lang::model::{Component, PinTarget};
        let mut c = Component::default();
        c.part = "Unknown:X".into();
        c.pins.insert("1".into(), PinTarget::Net("A".into()));
        c.pins.insert("2".into(), PinTarget::Net("B".into()));
        assert_eq!(role_of(&c, &provider()), Role::ChainElement);
    }

    /// Helper: walk raw chains for a one-block design.
    fn raw(src: &str) -> (Vec<Vec<Link>>, Vec<String>) {
        let d = compile(src);
        let p = provider();
        let name = d.blocks.keys().next().unwrap().clone();
        let uses = net_uses(&d, &name, &p);
        let elements: BTreeSet<RefDes> = d.blocks[&name]
            .components
            .iter()
            .filter(|(_, c)| role_of(c, &p) == Role::ChainElement)
            .map(|(r, _)| r.clone())
            .collect();
        raw_chains(&d.blocks[&name], &uses, &elements)
    }

    #[test]
    fn divider_node_breaks_chains_into_three() {
        // OUT joins three chain pins -> not a through-net -> three 1-link chains.
        let (chains, notes) = raw(
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
        assert!(notes.is_empty());
        assert_eq!(chains.len(), 3);
        assert!(chains.iter().all(|c| c.len() == 1));
    }

    #[test]
    fn series_elements_chain_through_degree_two_nets_with_anchor_taps() {
        // 555-style: R1 -(N_DIS)- R2 -(N_THR)- C1, with U1 pins tapping both
        // internal nets. Anchor taps must NOT break the walk.
        let (chains, notes) = raw(
            "
version: 1
name: t
rails: [9V, GND]
blocks:
  a:
    components:
      R1: {part: Device:R, value: 4k7, between: [9V, N_DIS]}
      R2: {part: Device:R, value: 10k, between: [N_DIS, N_THR]}
      C1: {part: Device:C, value: 100u, between: [N_THR, GND]}
      U1:
        part: Mock:REG
        pins: {VI: N_DIS, GND: N_THR, VO: Q, EN: 9V}
",
        );
        assert!(notes.is_empty());
        assert_eq!(chains.len(), 1, "{chains:?}");
        let refs: Vec<&str> = chains[0].iter().map(|l| l.refdes.as_str()).collect();
        assert_eq!(refs, ["R1", "R2", "C1"]);
        assert_eq!(chains[0][0].a_net, "9V");
        assert_eq!(chains[0][2].b_net, "GND");
    }

    #[test]
    fn pure_cycle_breaks_deterministically() {
        let (chains, notes) = raw(
            "
version: 1
name: t
rails: []
blocks:
  a:
    components:
      R1: {part: Device:R, value: 1k, between: [N1, N2]}
      R2: {part: Device:R, value: 1k, between: [N2, N3]}
      R3: {part: Device:R, value: 1k, between: [N3, N1]}
",
        );
        assert_eq!(chains.len(), 1);
        assert_eq!(chains[0].len(), 3);
        // Pin the deterministic walk order: smallest leftover refdes (R1) breaks
        // the cycle, entered from its first net (N1); the walk then proceeds
        // R1 → R2 → R3.
        let refs: Vec<&str> = chains[0].iter().map(|l| l.refdes.as_str()).collect();
        assert_eq!(refs, ["R1", "R2", "R3"], "walk sequence must be deterministic");
        assert_eq!(chains[0][0].a_net, "N1", "R1 must be entered from N1 (its first net)");
        assert_eq!(notes.len(), 1, "cycle break must be reported: {notes:?}");
    }
}
