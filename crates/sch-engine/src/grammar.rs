//! Structural layout grammar — netlist-graph classification.
//!
//! Currently covers component role classification (`Anchor` vs `ChainElement`)
//! and per-net pin incidence (`net_uses`). Will grow to cover chains, banks,
//! and clusters (Tasks 2-3). Pure analysis — no KiCAD, no I/O, no geometry.
//! Consumed by `cluster_geom` (local geometry) and `place` (macro placement).

use circuit_lang::model::{Component, NetName, PinTarget, RefDes};
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

#[cfg(test)]
mod tests {
    use super::*;
    use circuit_lang::{MockSymbolProvider, PinType};

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
}
