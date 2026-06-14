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

/// What a chain's endpoints say about its rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainClass {
    /// rail → rail: vertical, positive rail top, GND bottom.
    RailRail,
    /// node/open → rail: hangs from its `a` node toward the rail at `b`.
    ToRail,
    /// signal → signal: horizontal series run, flow left (`a`) → right (`b`).
    Series,
}

/// A classified, oriented run of [`Link`]s.
///
/// `links[0].a_net` is the canonical left/driving end; `links.last().b_net` is
/// the right/sink end.  The orientation and class are fixed by [`classify`].
#[derive(Debug, Clone, PartialEq)]
pub struct Chain {
    pub links: Vec<Link>,
    pub class: ChainClass,
}

impl Chain {
    /// The `a_net` of the first link — the canonical driving/left endpoint.
    pub fn start_net(&self) -> &str {
        &self.links[0].a_net
    }
    /// The `b_net` of the last link — the canonical sink/right endpoint.
    pub fn end_net(&self) -> &str {
        &self.links[self.links.len() - 1].b_net
    }
}

/// Parallel single-element rail→rail chains on the same net pair: rendered as
/// one bused bank with a single power symbol per side. `a_net` is the
/// positive/top net, `b_net` the bottom (ground side when present).
#[derive(Debug, Clone, PartialEq)]
pub struct Bank {
    pub a_net: NetName,
    pub b_net: NetName,
    /// Natural-ordered member refdes.
    pub members: Vec<RefDes>,
}

/// The rigid placement unit: chains joined through shared signal nodes, plus
/// any banks. A standalone bank or single chain is a cluster by itself.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Cluster {
    pub chains: Vec<Chain>,
    pub banks: Vec<Bank>,
    /// Candidate pin anchors: cluster endpoint nets joining exactly one anchor
    /// pin in-block and nothing external: (net, anchor refdes, pin). Populated
    /// by `analyze`; consumed by placement (Task 10).
    pub anchor_taps: Vec<(NetName, RefDes, String)>,
}

impl Cluster {
    /// Natural-ordered member refdes (chains then banks, deduped).
    pub fn members(&self) -> Vec<RefDes> {
        let mut m: Vec<RefDes> = self
            .chains
            .iter()
            .flat_map(|c| c.links.iter().map(|l| l.refdes.clone()))
            .chain(self.banks.iter().flat_map(|b| b.members.iter().cloned()))
            .collect();
        natural_sort_by_key(&mut m, |r| r.clone());
        m.dedup();
        m
    }
}

/// One block's full grammar analysis.
#[derive(Debug, Default)]
pub struct BlockGraph {
    /// Natural-ordered anchors.
    pub anchors: Vec<RefDes>,
    /// Placement clusters in deterministic order.
    ///
    /// Order is determined by the first member of each cluster encountered
    /// during chain/bank assignment (i.e. the natural-order net that triggered
    /// the cluster's creation).  Empty slots from merge operations are removed
    /// by `analyze` before this field is populated.
    pub clusters: Vec<Cluster>,
    /// Human-readable degradation notes (cycle breaks etc.).
    pub degradations: Vec<String>,
}

/// Score an endpoint net's preference for being the LEFT (driving/source) end.
///
/// Sign convention: **lower score = wants to be the left (`a`) end.**
/// A PowerOutput anchor pin on the net contributes −2 (source, drives left);
/// a PowerInput pin contributes +2 (sink, pushed right); an external (cross-block)
/// net contributes −1 (incoming signal treated as arriving from the left).
fn end_score(
    net: &str,
    uses: &IndexMap<NetName, NetUse>,
    block: &Block,
    provider: &dyn SymbolProvider,
) -> i32 {
    use circuit_lang::PinType;
    let Some(u) = uses.get(net) else { return 0 };
    let mut score = 0;
    for (refdes, pin) in &u.anchor_pins {
        let comp = &block.components[refdes.as_str()];
        let Some(meta) = provider.symbol(&comp.part) else { continue };
        let etype = circuit_lang::find_pin(&meta.pins, pin).map(|p| p.etype);
        match etype {
            Some(PinType::PowerOutput) => score -= 2,
            Some(PinType::PowerInput) => score += 2,
            _ => {}
        }
    }
    if u.external {
        score -= 1; // cross-block signals read as arriving from the left
    }
    score
}

/// Orient a raw link sequence and derive its [`ChainClass`].
///
/// Mutates `links` in-place so that the canonically-left (driving/source) end
/// becomes `links[0].a_net`.  For RailRail, positive rail first; for ToRail,
/// the non-rail (node) end first.  For Series, the end with the lower
/// [`end_score`] goes left; when scores are equal the naturally-smaller net
/// name is placed at `a` (tie-break ensures a deterministic canonical form).
fn classify(
    mut links: Vec<Link>,
    uses: &IndexMap<NetName, NetUse>,
    block: &Block,
    provider: &dyn SymbolProvider,
) -> Chain {
    fn reverse(links: &mut Vec<Link>) {
        links.reverse();
        for l in links.iter_mut() {
            *l = l.reversed();
        }
    }
    let power = |n: &str| uses.get(n).is_some_and(|u| u.power);
    let s = links[0].a_net.clone();
    let e = links.last().unwrap().b_net.clone();
    let class = match (power(&s), power(&e)) {
        (true, true) => {
            if is_ground(&s) && !is_ground(&e) {
                reverse(&mut links);
            }
            ChainClass::RailRail
        }
        (false, true) => ChainClass::ToRail,
        (true, false) => {
            reverse(&mut links);
            ChainClass::ToRail
        }
        (false, false) => {
            let (ss, es) = (
                end_score(&s, uses, block, provider),
                end_score(&e, uses, block, provider),
            );
            if ss > es || (ss == es && circuit_lang::canon::natural_lt(&e, &s)) {
                reverse(&mut links);
            }
            ChainClass::Series
        }
    };
    Chain { links, class }
}

/// Full grammar analysis for one block.
pub fn analyze(design: &Design, block_name: &str, provider: &dyn SymbolProvider) -> BlockGraph {
    let block = &design.blocks[block_name];
    let uses = net_uses(design, block_name, provider);

    let mut anchors: Vec<RefDes> = Vec::new();
    let mut elements: BTreeSet<RefDes> = BTreeSet::new();
    for (refdes, comp) in &block.components {
        match role_of(comp, provider) {
            Role::Anchor => anchors.push(refdes.clone()),
            Role::ChainElement => {
                elements.insert(refdes.clone());
            }
        }
    }
    natural_sort_by_key(&mut anchors, |r| r.clone());

    let (raw, mut degradations) = raw_chains(block, &uses, &elements);
    let mut chains: Vec<Chain> = raw
        .into_iter()
        .map(|links| classify(links, &uses, block, provider))
        .collect();

    // Banks: single-link RailRail chains grouped by their (a, b) net pair.
    let mut banks: Vec<Bank> = Vec::new();
    let mut keep: Vec<Chain> = Vec::new();
    let mut groups: IndexMap<(NetName, NetName), Vec<RefDes>> = IndexMap::new();
    for chain in chains.drain(..) {
        if chain.class == ChainClass::RailRail && chain.links.len() == 1 {
            let key = (chain.start_net().to_string(), chain.end_net().to_string());
            groups.entry(key).or_default().push(chain.links[0].refdes.clone());
            keep.push(chain); // provisional; pulled out below if its group banks
        } else {
            keep.push(chain);
        }
    }
    let banked: BTreeSet<RefDes> = groups
        .iter()
        .filter(|(_, members)| members.len() >= 2)
        .flat_map(|(_, members)| members.iter().cloned())
        .collect();
    for ((a, b), mut members) in groups {
        if members.len() >= 2 {
            natural_sort_by_key(&mut members, |r| r.clone());
            banks.push(Bank { a_net: a, b_net: b, members });
        }
    }
    chains = keep
        .into_iter()
        .filter(|c| !(c.links.len() == 1 && banked.contains(&c.links[0].refdes)))
        .collect();

    // Clusters: union chains/banks through shared non-power endpoint nets.
    // Rails never merge (GND would glue everything together).
    let node_of = |net: &str| -> Option<String> {
        uses.get(net).filter(|u| !u.power).map(|_| net.to_string())
    };
    let mut clusters: Vec<Cluster> = Vec::new();
    let mut net_cluster: IndexMap<String, usize> = IndexMap::new();

    /// Assign `nodes` to a cluster, merging pre-existing clusters as needed.
    ///
    /// When all nodes are new a fresh [`Cluster::default()`] is pushed and its
    /// index returned.  When a bridging item connects two already-distinct
    /// clusters, every cluster with a higher index is drained into the
    /// lowest-index one via [`std::mem::take`], leaving the drained slot as an
    /// empty `Cluster::default()`.  All `net_cluster` entries that pointed to a
    /// drained slot are remapped to `lowest`.
    ///
    /// **Invariant for callers:** after all assignments are done the cluster vec
    /// will contain empty slots from merges.  Callers **must** call
    /// `clusters.retain(|c| !c.chains.is_empty() || !c.banks.is_empty())` before
    /// using the vec.  `analyze` does this after both assignment loops.
    fn assign(
        nodes: &[String],
        clusters: &mut Vec<Cluster>,
        net_cluster: &mut IndexMap<String, usize>,
    ) -> usize {
        // Existing distinct indices these nodes already belong to.
        let mut existing: Vec<usize> = nodes
            .iter()
            .filter_map(|n| net_cluster.get(n).copied())
            .collect();
        existing.sort_unstable();
        existing.dedup();
        let idx = match existing.first() {
            None => {
                clusters.push(Cluster::default());
                clusters.len() - 1
            }
            Some(&lowest) => {
                // Merge every other existing cluster into `lowest`.
                for &other in existing.iter().skip(1) {
                    let drained = std::mem::take(&mut clusters[other]);
                    clusters[lowest].chains.extend(drained.chains);
                    clusters[lowest].banks.extend(drained.banks);
                    // Remap any net pointing at `other` to `lowest`.
                    for v in net_cluster.values_mut() {
                        if *v == other {
                            *v = lowest;
                        }
                    }
                }
                lowest
            }
        };
        for n in nodes {
            net_cluster.insert(n.clone(), idx);
        }
        idx
    }

    for chain in chains {
        let nodes: Vec<String> = [chain.start_net(), chain.end_net()]
            .iter()
            .filter_map(|n| node_of(n))
            .collect();
        let idx = assign(&nodes, &mut clusters, &mut net_cluster);
        clusters[idx].chains.push(chain);
    }
    for bank in banks {
        let nodes: Vec<String> = [bank.a_net.as_str(), bank.b_net.as_str()]
            .iter()
            .filter_map(|n| node_of(n))
            .collect();
        let idx = assign(&nodes, &mut clusters, &mut net_cluster);
        clusters[idx].banks.push(bank);
    }
    clusters.retain(|c| !c.chains.is_empty() || !c.banks.is_empty());

    // Candidate pin anchors per cluster: cluster nets — endpoints AND interior
    // joints (a rail-rail chain like a 555 timing ladder touches its anchor
    // only at interior joints) — that join exactly one in-block anchor pin and
    // are neither power nor external.
    for cluster in &mut clusters {
        let mut nets: std::collections::BTreeSet<NetName> = std::collections::BTreeSet::new();
        for c in &cluster.chains {
            for l in &c.links {
                nets.insert(l.a_net.clone());
                nets.insert(l.b_net.clone());
            }
        }
        for b in &cluster.banks {
            nets.insert(b.a_net.clone());
        }
        for net in nets {
            let Some(u) = uses.get(&net) else { continue };
            if !u.power && !u.external && u.anchor_pins.len() == 1 {
                let (r, p) = &u.anchor_pins[0];
                cluster.anchor_taps.push((net, r.clone(), p.clone()));
            }
        }
    }

    degradations
        .iter_mut()
        .for_each(|n| *n = format!("block {block_name}: {n}"));
    BlockGraph { anchors, clusters, degradations }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use circuit_lang::{MockSymbolProvider, PinType};
    use std::collections::BTreeSet;

    pub(crate) fn provider() -> MockSymbolProvider {
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

    pub(crate) fn compile(src: &str) -> circuit_lang::Design {
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

    fn analyze_one(src: &str) -> BlockGraph {
        let d = compile(src);
        let name = d.blocks.keys().next().unwrap().clone();
        analyze(&d, &name, &provider())
    }

    #[test]
    fn rail_rail_chain_orients_positive_first() {
        let g = analyze_one(
            "
version: 1
name: t
rails: [3V3, GND]
blocks:
  a:
    components:
      D1: {part: Device:LED, value: red, between: [GND, MID]}
      R2: {part: Device:R, value: 330, between: [MID, 3V3]}
",
        );
        assert_eq!(g.clusters.len(), 1);
        let chain = &g.clusters[0].chains[0];
        assert_eq!(chain.class, ChainClass::RailRail);
        assert_eq!(chain.links[0].a_net, "3V3", "positive rail first");
        assert_eq!(chain.links.last().unwrap().b_net, "GND");
    }

    #[test]
    fn to_rail_chain_orients_node_first_and_series_flow_uses_pin_types() {
        // C9 uses COUT (its own isolated node) so it is a standalone single-link
        // chain and does not chain with F1 through a shared through-net.
        let g = analyze_one(
            "
version: 1
name: t
rails: [VCC, GND]
blocks:
  a:
    components:
      C9: {part: Device:C, value: 47n, between: [GND, COUT]}
      F1: {part: Device:R, value: 0R, between: [VOUT_REG, OUT]}
      U1:
        part: Mock:REG
        pins: {VI: VCC, GND: GND, VO: VOUT_REG, EN: VCC}
",
        );
        // C9: GND<->COUT must orient COUT (node) first, GND last.
        let c9 = g.clusters.iter().flat_map(|c| &c.chains)
            .find(|c| c.links[0].refdes == "C9").unwrap();
        assert_eq!(c9.class, ChainClass::ToRail);
        assert_eq!(c9.links[0].a_net, "COUT");
        // F1: VOUT_REG carries U1's PowerOutput pin -> that end goes LEFT (a).
        let f1 = g.clusters.iter().flat_map(|c| &c.chains)
            .find(|c| c.links[0].refdes == "F1").unwrap();
        assert_eq!(f1.class, ChainClass::Series);
        assert_eq!(f1.links[0].a_net, "VOUT_REG");
    }

    #[test]
    fn parallel_rail_rail_singles_group_into_a_bank() {
        let g = analyze_one(
            "
version: 1
name: t
rails: [3V3, GND]
blocks:
  a:
    components:
      C1: {part: Device:C, value: 100n, between: [3V3, GND]}
      C2: {part: Device:C, value: 100n, between: [3V3, GND]}
      C3: {part: Device:C, value: 10u, between: [GND, 3V3]}
      R9: {part: Device:R, value: 10k, between: [SIG, 3V3]}
",
        );
        let banks: Vec<&Bank> = g.clusters.iter().flat_map(|c| &c.banks).collect();
        assert_eq!(banks.len(), 1);
        assert_eq!(banks[0].members, vec!["C1", "C2", "C3"]);
        assert_eq!(banks[0].a_net, "3V3");
        assert_eq!(banks[0].b_net, "GND");
        // R9 (ToRail) stays a chain, not a bank member.
        assert!(g.clusters.iter().flat_map(|c| &c.chains)
            .any(|c| c.links[0].refdes == "R9"));
    }

    #[test]
    fn chains_sharing_a_node_form_one_cluster() {
        let g = analyze_one(
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
      C5: {part: Device:C, value: 100n, between: [VCC, GND]}
",
        );
        // R7+R8+C3 share node OUT -> one cluster of three chains.
        // C5 is rail-rail with no node -> its own cluster.
        assert_eq!(g.clusters.len(), 2, "{:?}", g.clusters);
        let star = g.clusters.iter().find(|c| c.chains.len() == 3).unwrap();
        assert!(star.banks.is_empty());
        let solo = g.clusters.iter().find(|c| c.chains.len() == 1).unwrap();
        assert_eq!(solo.chains[0].class, ChainClass::RailRail);
    }

    #[test]
    fn bridging_chain_merges_two_clusters_into_one() {
        // Two clusters form early via the open start-nets A0 and A1 (processed
        // before the node nets M0/M1 in natural order), then R5 (M0<->M1) is
        // walked during M0's turn when M0 and M1 ALREADY belong to two
        // different clusters -> exercises assign()'s merge branch.
        //   R1: A0 <-> M1   (start A0 -> cluster on M1)
        //   R2: A1 <-> M0   (start A1 -> cluster on M0)
        //   R3: B0 <-> M0   (filler: pads M0 to degree 3 so it stays non-through)
        //   R4: B1 <-> M1   (filler: pads M1 to degree 3)
        //   R5: M0 <-> M1   (bridge -> merges the two clusters)
        let g = analyze_one(
            "
version: 1
name: t
rails: []
blocks:
  a:
    components:
      R1: {part: Device:R, value: 1k, between: [A0, M1]}
      R2: {part: Device:R, value: 1k, between: [A1, M0]}
      R3: {part: Device:R, value: 1k, between: [B0, M0]}
      R4: {part: Device:R, value: 1k, between: [B1, M1]}
      R5: {part: Device:R, value: 1k, between: [M0, M1]}
",
        );
        assert_eq!(g.clusters.len(), 1, "{:?}", g.clusters);
        assert_eq!(g.clusters[0].members(), vec!["R1", "R2", "R3", "R4", "R5"]);
    }

    #[test]
    fn anchor_taps_record_single_pin_endpoint_nets() {
        // R1 (Q<->MID), D1 (MID<->GND) form one ToRail cluster. U1.VO taps Q
        // (exactly one anchor pin, no external use) -> one anchor_tap on Q.
        // MID joins two chain pins and no anchor pin -> none. GND is a rail ->
        // none.
        let g = analyze_one(
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
        assert_eq!(g.clusters.len(), 1, "{:?}", g.clusters);
        assert_eq!(
            g.clusters[0].anchor_taps,
            vec![("Q".to_string(), "U1".to_string(), "VO".to_string())]
        );
    }
}
