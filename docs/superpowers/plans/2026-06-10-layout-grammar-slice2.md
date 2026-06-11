# Layout Grammar — Slice 2 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the structural layout grammar of `docs/superpowers/specs/2026-06-10-layout-grammar-design.md`: classify each block's netlist graph into anchors/chains/banks/clusters, generate wired cluster geometry (buses, junctions, hangs, spines), and integrate it into placement + reconciliation so professional layouts emerge without an idiom library.

**Architecture:** Two new sch-engine modules — `grammar.rs` (pure netlist-graph analysis: roles → chains → classification → banks → clusters) and `cluster_geom.rs` (closed-form local geometry: placements, wires, junctions, ports, labels per cluster). `emit.rs` gains junction elements and net-aware wire occupancy (same-net touches become deliberate joins). `place.rs` packs anchors + cluster envelopes and returns angles + cluster origins. `reconcile.rs` consumes all of it: cluster-as-unit preservation, covered-pin skipping, translated decoration. Four reference fixtures become structural-oracle tests.

**Tech Stack:** Rust 2024 workspace, KiCAD 10 `kicad-cli` (tests SKIP without it; this machine has 10.0.3), existing crates only (no new deps).

**Out of scope (follow-up plan):** `sheet:` 3×3 grid / `title:` / `place:` DSL surface (none of these exist in the model yet; band stacking by `edge:` remains the macro fallback), cross-net bus merging, side-aware anchor cell clearance (anchors keep the uniform `CLEARANCE_MM` cell; clusters are already tight), and the vision-loop rubric update.

**Conventions for every task:** run tests with `cargo test -p <crate>` from the repo root. NEVER run two cargo commands concurrently (shared target dir). Tests touching real KiCAD use `KicadEnv::detect()` and SKIP when absent. Ordering helpers use `circuit_lang::canon::natural_lt` (natural refdes order) — never plain string sort for refdes.

---

## Part A — Grammar analysis (`sch-engine/src/grammar.rs`, pure, no KiCAD)

### Task 1: Roles + net incidence

**Files:**
- Create: `crates/sch-engine/src/grammar.rs`
- Modify: `crates/sch-engine/src/lib.rs` (add `pub mod grammar;`)
- Modify: `crates/sch-engine/src/reconcile.rs` (replace its private `is_ground` with `use crate::grammar::is_ground;` — delete the local fn)

- [ ] **Step 1: Write the failing tests** (inline `mod tests` in the new `grammar.rs`)

```rust
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
        assert_eq!(out.anchor_pins, vec![("U1".to_string(), "3".to_string())]);
        assert!(out.external, "OUT is also used in block b");
        assert!(!out.power);
        assert!(uses["GND"].power);
        assert!(!uses["GND"].external == d.blocks["b"].components.is_empty() || uses["GND"].external);
    }

    #[test]
    fn ground_detection() {
        assert!(is_ground("GND"));
        assert!(is_ground("AGND"));
        assert!(is_ground("VSS"));
        assert!(!is_ground("VCC"));
        assert!(!is_ground("3V3"));
    }
}
```

Note on `between:` pin order: desugar maps `between: [A, B]` to pin `1` → A, pin `2` → B for two-pin parts. The `net_uses` test relies on that (R1 pin 2 on OUT). If the assertion fails on pin numbers, check the desugar convention in `crates/circuit-lang/src/desugar.rs` and fix the TEST to match it — do not change desugar.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p sch-engine grammar`
Expected: COMPILE ERROR — module missing.

- [ ] **Step 3: Implement**

```rust
//! Structural layout grammar — netlist-graph classification.
//!
//! Implements the analysis half of `docs/superpowers/specs/`
//! `2026-06-10-layout-grammar-design.md`: every component classifies as an
//! anchor or a chain element; chains form by walking degree-2 nets; chains
//! classify by their endpoints; parallel rail-rail chains group into banks;
//! chains sharing signal nodes group into clusters. Pure analysis — no KiCAD,
//! no I/O, no geometry. Consumed by `cluster_geom` (local geometry) and
//! `place` (macro placement).

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
    v.sort_by(|a, b| {
        let (ka, kb) = (key(a), key(b));
        if circuit_lang::canon::natural_lt(&ka, &kb) {
            std::cmp::Ordering::Less
        } else if circuit_lang::canon::natural_lt(&kb, &ka) {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    });
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
            u.power = design.nets.get(net).is_some_and(|a| a.power);
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
        natural_sort_by_key(&mut u.chain_pins, |p| format!("{}\u{0}{}", p.0, p.1));
        natural_sort_by_key(&mut u.anchor_pins, |p| format!("{}\u{0}{}", p.0, p.1));
    }
    uses.sort_keys();
    uses
}
```

In `reconcile.rs`, delete the private `fn is_ground` and add `use crate::grammar::is_ground;`. In `lib.rs`, add `pub mod grammar;`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p sch-engine grammar && cargo test -p sch-engine reconcile`
Expected: PASS (run the two commands sequentially, never concurrently).

- [ ] **Step 5: Commit**

```bash
git add crates/sch-engine/src/grammar.rs crates/sch-engine/src/lib.rs crates/sch-engine/src/reconcile.rs
git commit -m "feat(sch-engine): grammar roles + net incidence analysis"
```

---

### Task 2: Chain walking

**Files:**
- Modify: `crates/sch-engine/src/grammar.rs`

- [ ] **Step 1: Write the failing tests** (append to `grammar.rs` `mod tests`)

```rust
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
      RA: {part: Device:R, value: 1k, between: [N1, N2]}
      RB: {part: Device:R, value: 1k, between: [N2, N3]}
      RC: {part: Device:R, value: 1k, between: [N3, N1]}
",
        );
        assert_eq!(chains.len(), 1);
        assert_eq!(chains[0].len(), 3);
        assert_eq!(chains[0][0].refdes, "RA");
        assert_eq!(notes.len(), 1, "cycle break must be reported: {notes:?}");
    }
```

Add `use std::collections::BTreeSet;` to the test imports if missing.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p sch-engine grammar`
Expected: COMPILE ERROR — `Link` / `raw_chains` missing.

- [ ] **Step 3: Implement** (append to `grammar.rs`)

```rust
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
        if n1 == from_net {
            Link { refdes: refdes.into(), a_pin: p1, a_net: n1, b_pin: p2, b_net: n2 }
        } else {
            Link { refdes: refdes.into(), a_pin: p2, a_net: n2, b_pin: p1, b_net: n1 }
        }
    }

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
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p sch-engine grammar`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/sch-engine/src/grammar.rs
git commit -m "feat(sch-engine): grammar chain walking with anchor taps + cycle break"
```

---

### Task 3: Chain classification, banks, clusters

**Files:**
- Modify: `crates/sch-engine/src/grammar.rs`

- [ ] **Step 1: Write the failing tests** (append to `mod tests`)

```rust
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
        let g = analyze_one(
            "
version: 1
name: t
rails: [VCC, GND]
blocks:
  a:
    components:
      C9: {part: Device:C, value: 47n, between: [GND, OUT]}
      F1: {part: Device:R, value: 0R, between: [VOUT_REG, OUT]}
      U1:
        part: Mock:REG
        pins: {VI: VCC, GND: GND, VO: VOUT_REG, EN: VCC}
",
        );
        // C9: GND<->OUT must orient OUT (node) first, GND last.
        let c9 = g.clusters.iter().flat_map(|c| &c.chains)
            .find(|c| c.links[0].refdes == "C9").unwrap();
        assert_eq!(c9.class, ChainClass::ToRail);
        assert_eq!(c9.links[0].a_net, "OUT");
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
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p sch-engine grammar`
Expected: COMPILE ERROR — `ChainClass` / `Bank` / `BlockGraph` / `analyze` missing.

- [ ] **Step 3: Implement** (append to `grammar.rs`)

```rust
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

#[derive(Debug, Clone, PartialEq)]
pub struct Chain {
    pub links: Vec<Link>,
    pub class: ChainClass,
}

impl Chain {
    pub fn start_net(&self) -> &str {
        &self.links[0].a_net
    }
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
    /// Clusters in deterministic first-member order.
    pub clusters: Vec<Cluster>,
    /// Human-readable degradation notes (cycle breaks etc.).
    pub degradations: Vec<String>,
}

/// Lower score wants to be the LEFT (driving) end of a Series chain.
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
        let etype = meta
            .pins
            .iter()
            .find(|p| &p.number == pin)
            .or_else(|| meta.pins.iter().find(|p| &p.name == pin))
            .map(|p| p.etype);
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
    let mut place_item = |nodes: Vec<String>,
                          clusters: &mut Vec<Cluster>,
                          net_cluster: &mut IndexMap<String, usize>|
     -> usize {
        let mut idx = nodes.iter().find_map(|n| net_cluster.get(n).copied());
        if idx.is_none() {
            clusters.push(Cluster::default());
            idx = Some(clusters.len() - 1);
        }
        let idx = idx.unwrap();
        for n in nodes {
            net_cluster.insert(n, idx);
        }
        idx
    };
    for chain in chains {
        let nodes: Vec<String> = [chain.start_net(), chain.end_net()]
            .iter()
            .filter_map(|n| node_of(n))
            .collect();
        let idx = place_item(nodes, &mut clusters, &mut net_cluster);
        clusters[idx].chains.push(chain);
    }
    for bank in banks {
        let nodes: Vec<String> = [bank.a_net.as_str(), bank.b_net.as_str()]
            .iter()
            .filter_map(|n| node_of(n))
            .collect();
        let idx = place_item(nodes, &mut clusters, &mut net_cluster);
        clusters[idx].banks.push(bank);
    }
    clusters.retain(|c| !c.chains.is_empty() || !c.banks.is_empty());

    degradations
        .iter_mut()
        .for_each(|n| *n = format!("block {block_name}: {n}"));
    BlockGraph { anchors, clusters, degradations }
}
```

Note: `place_item` as written can leave two clusters unmerged if a later item bridges two existing clusters (chain A in cluster 0, chain B in cluster 1, chain C touching both). For v1 graphs this is rare; handle it correctly anyway — after the loop, merge clusters that share any `net_cluster` index collision by replacing the naive `place_item` with: when `nodes` map to two *different* existing indices, drain the higher-index cluster into the lower and remap. Implement that directly (it is ~10 lines) rather than leaving the bug.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p sch-engine grammar`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/sch-engine/src/grammar.rs
git commit -m "feat(sch-engine): chain classification, banks, cluster assembly"
```

---

## Part B — Emitter primitives

### Task 4: Junction elements

**Files:**
- Modify: `crates/sch-engine/src/emit.rs`

- [ ] **Step 1: Write the failing test** (in `emit.rs` `mod tests`)

```rust
    #[test]
    fn junctions_render_sorted_and_deduped() {
        let mut w = SchematicWriter::new();
        w.add_junction([50.8, 25.4]);
        w.add_junction([25.4, 25.4]);
        w.add_junction([50.8, 25.4]); // duplicate -> dropped
        let sch = w.finish();
        let count = sch.matches("(junction").count();
        assert_eq!(count, 2);
        let first = sch.find("(at 25.4 25.4)").unwrap();
        let second = sch.find("(at 50.8 25.4)").unwrap();
        assert!(first < second, "junctions sorted by uuid_key");
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p sch-engine junctions_render`
Expected: COMPILE ERROR — no method `add_junction`.

- [ ] **Step 3: Implement**

Struct (near `Wire`):

```rust
/// One `(junction …)` dot marking a deliberate ≥3-way wire join.
struct Junction {
    at: [f64; 2],
    /// Stable key for the junction uuid (content-derived from the position).
    uuid_key: String,
}
```

Writer field: `junctions: Vec<Junction>,` (add to the `SchematicWriter` struct and let `Default` cover it). Method (near `add_wire`):

```rust
    /// Add a junction dot at a wire join. Deduplicated by position.
    pub fn add_junction(&mut self, at: [f64; 2]) {
        let at = snap_point(at);
        let uuid_key = format!("{}:{}", at[0], at[1]);
        if self.junctions.iter().any(|j| j.uuid_key == uuid_key) {
            return;
        }
        self.junctions.push(Junction { at, uuid_key });
    }
```

Rendering in `finish`, immediately after the wire loop:

```rust
        let mut junctions = self.junctions;
        junctions.sort_by(|a, b| a.uuid_key.cmp(&b.uuid_key));
        for j in &junctions {
            let uuid = stable_uuid("junction", &j.uuid_key);
            let _ = writeln!(
                out,
                "\t(junction\n\t\t(at {} {})\n\t\t(diameter 0)\n\t\t(color 0 0 0 0)\n\t\t(uuid \"{uuid}\")\n\t)",
                fmt_coord(j.at[0]),
                fmt_coord(j.at[1]),
            );
        }
```

Note: `finish` consumes `self` by value but `retract_colliding_stubs` runs first; junctions don't participate in retraction. Take `self.junctions` via `std::mem::take` if the borrow checker complains about the move order.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p sch-engine --lib`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/sch-engine/src/emit.rs
git commit -m "feat(sch-engine): junction elements with stable uuids"
```

---

### Task 5: Net-aware wires + same-net occupancy whitelist

**Files:**
- Modify: `crates/sch-engine/src/emit.rs`

The load-bearing change: today `retract_colliding_stubs` registers every existing wire under the `\0power_wire` sentinel, so ANY touch retracts a signal stub. Cluster wires carry a real net; touches by the SAME net are deliberate joins.

- [ ] **Step 1: Write the failing test** (in `emit.rs` `mod tests`)

```rust
    #[test]
    fn same_net_wire_touch_survives_foreign_retracts() {
        let Some(env) = kicad_bridge::env::KicadEnv::detect() else {
            eprintln!("SKIP: no KiCAD environment detected");
            return;
        };
        // Two resistors, each with a signal label stubbed East from pin 1 at
        // y=63.5. A cluster wire on net SIG runs through R1's stub end; a
        // cluster wire on net OTHER runs through R2's stub end.
        let mut w = SchematicWriter::new();
        w.add_symbol(&env, "Device:R", "R1", "1k", [127.0, 63.5], 0.0).unwrap();
        w.add_symbol(&env, "Device:R", "R2", "1k", [177.8, 63.5], 0.0).unwrap();
        w.add_signal_label(&env, "R1", "1", "SIG").unwrap();
        w.add_signal_label(&env, "R2", "1", "SIG").unwrap();
        // R pin 1 endpoint is (x, 59.69); stub goes North to (x, 55.88).
        w.add_wire_on_net([121.92, 55.88], [132.08, 55.88], "SIG");
        w.add_wire_on_net([172.72, 55.88], [182.88, 55.88], "OTHER");
        let sch = w.finish();

        // R1's stub end lies on a SIG wire -> same net -> stub survives:
        // its label stays displaced from the pin endpoint.
        assert!(sch.contains("(label \"SIG\""));
        // R2's stub end lies on a foreign OTHER wire -> retracted back to the
        // pin endpoint at (177.8, 59.69).
        assert!(
            sch.contains("(label \"SIG\"\n\t\t(at 177.8 59.69"),
            "R2 label must retract to its pin endpoint:\n{sch}"
        );
    }
```

(If the label rendering layout differs — check an existing label in the output and adjust the retraction assertion to match the actual `(at x y` formatting; the invariant being tested is R2's label AT the pin endpoint and R1's label NOT at it.)

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p sch-engine same_net_wire`
Expected: COMPILE ERROR — no method `add_wire_on_net`.

- [ ] **Step 3: Implement**

`Wire` gains a net:

```rust
struct Wire {
    a: [f64; 2],
    b: [f64; 2],
    uuid_key: String,
    /// The net this wire belongs to, when known (cluster-generated wires).
    /// `None` for legacy power stubs/risers (treated as a reserved foreign net).
    net: Option<String>,
}
```

`add_wire` keeps its signature and passes `net: None`; both delegate to one private helper:

```rust
    /// Add a wire segment between two sheet points (snapped).
    pub fn add_wire(&mut self, a: [f64; 2], b: [f64; 2]) {
        self.push_wire(a, b, None);
    }

    /// Add a wire that belongs to a known net (cluster geometry). Same-net
    /// touches against it are deliberate joins, not collisions.
    pub fn add_wire_on_net(&mut self, a: [f64; 2], b: [f64; 2], net: &str) {
        self.push_wire(a, b, Some(net.to_string()));
    }

    fn push_wire(&mut self, a: [f64; 2], b: [f64; 2], net: Option<String>) {
        let a = snap_point(a);
        let b = snap_point(b);
        if a == b {
            return;
        }
        let uuid_key = format!("{}:{}:{}:{}", a[0], a[1], b[0], b[1]);
        if self.wires.iter().any(|w| w.uuid_key == uuid_key) {
            return;
        }
        self.wires.push(Wire { a, b, uuid_key, net });
    }
```

(Adapt to the existing `add_wire` body — keep its current snap/self-loop/dedup semantics exactly; only the `net` field is new. Update the one existing `Wire { … }` construction site if `add_wire` builds the struct inline.)

In `retract_colliding_stubs`, replace the existing-wires registration:

```rust
        // Existing wires: power stubs/risers carry the reserved PWR net;
        // cluster wires carry their real net so same-net stubs may touch them.
        for w in &self.wires {
            let net = w.net.clone().unwrap_or_else(|| PWR.to_string());
            segments.push((w.a, w.b, net.clone()));
            add_point(w.a, &net, &mut points);
            add_point(w.b, &net, &mut points);
        }
```

- [ ] **Step 4: Run the full engine suite**

Run: `cargo test -p sch-engine`
Expected: PASS — including all pre-existing retraction/bluepill tests (power wires keep the sentinel, so their behavior is unchanged).

- [ ] **Step 5: Commit**

```bash
git add crates/sch-engine/src/emit.rs
git commit -m "feat(sch-engine): net-aware wire occupancy (same-net joins allowed)"
```

---

## Part C — Cluster geometry (`sch-engine/src/cluster_geom.rs`)

All geometry is closed-form, cluster-local (origin top-left after normalize, y grows down), and uses a pin-end callback so unit tests run without KiCAD: `Device:R`/`Device:C` mock as pin `1` end `[0.0, -3.81]`, pin `2` end `[0.0, 3.81]` (vertical body, pin 1 up, sheet coords).

### Task 6: Transform extraction + vertical stacking + bank geometry

**Files:**
- Modify: `crates/sch-engine/src/emit.rs` (extract `transform_offset` from `pin_endpoint`)
- Create: `crates/sch-engine/src/cluster_geom.rs`
- Modify: `crates/sch-engine/src/lib.rs` (add `pub mod cluster_geom;`)

- [ ] **Step 1: Extract the offset transform in `emit.rs`**

`pin_endpoint` currently computes: mirror → rotate by instance angle → sheet Y-flip → add instance position. Factor the offset part into a crate-visible function and re-express `pin_endpoint` with it (NO behavior change — the existing emit tests are the regression net):

```rust
/// Transform a symbol-space offset (y up) into a sheet-space offset (y down)
/// for an instance at `angle` degrees, optionally mirrored. This is the
/// offset half of `pin_endpoint`; cluster geometry reuses it to reason about
/// pin ends before any instance exists.
pub(crate) fn transform_offset(local: [f64; 2], angle: f64, mirror: bool) -> [f64; 2] {
    let (mut x, y) = (local[0], local[1]);
    if mirror {
        x = -x;
    }
    let phi = angle.to_radians();
    let (s, c) = phi.sin_cos();
    let rx = x * c - y * s;
    let ry = x * s + y * c;
    [rx, -ry]
}
```

(Match the exact math currently inside `pin_endpoint` — copy it, don't re-derive. If `pin_endpoint`'s body differs from the above, the extracted function must replicate IT, and the doc comment adjusted.) Run `cargo test -p sch-engine` — all existing tests must stay green before proceeding.

- [ ] **Step 2: Write the failing tests** (inline `mod tests` in new `cluster_geom.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::grammar::{Bank, Chain, ChainClass, Link};

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
        // D1 oriented a=pin2 (so needs 180°), R2 a=pin1 (0°).
        let links = vec![
            link("D1", "2", "3V3", "1", "MID"),
            link("R2", "1", "MID", "2", "GND"),
        ];
        let (end, joints) = stack_chain(&mut g, &links, [0.0, 0.0], true, &mock_pins);
        // D1 a-end at the start point; rotated 180 so pin2 is on top.
        assert_eq!(g.placements[0], ("D1".to_string(), [0.0, 3.81], 180.0));
        // R2 a-end (pin 1) coincides with D1's b-end at y=7.62.
        assert_eq!(g.placements[1], ("R2".to_string(), [0.0, 11.43], 0.0));
        assert_eq!(end, [0.0, 15.24]);
        assert_eq!(joints, vec![("MID".to_string(), [0.0, 7.62])]);
        // Every chain pin is covered.
        assert_eq!(g.covered.len(), 4);
    }

    #[test]
    fn bank_has_buses_single_ports_and_junctions() {
        // Build via a one-block compile so component pin maps exist.
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

        // Three members at BANK_PITCH; C3 is flipped (its pin 1 is on GND).
        assert_eq!(g.placements.len(), 3);
        assert_eq!(g.placements[0].1[0] + BANK_PITCH, g.placements[1].1[0]);
        assert_eq!(g.placements[2].2, 180.0, "C3 flips so its 3V3 pin is up");
        // Exactly one port per net.
        assert_eq!(g.ports.len(), 2);
        let nets: Vec<&str> = g.ports.iter().map(|(n, _)| n.as_str()).collect();
        assert!(nets.contains(&"3V3") && nets.contains(&"GND"));
        // Two bus wires + 6 stubs.
        assert_eq!(g.wires.len(), 8);
        // Junctions: interior member (1) + port member (0), on both buses.
        assert_eq!(g.junctions.len(), 4);
        // All six pins covered.
        assert_eq!(g.covered.len(), 6);
    }
}
```

For the cross-module test helpers, change `grammar.rs`'s `mod tests` to `pub(crate) mod tests` IS not possible for `#[cfg(test)]` private mods directly — instead mark the two helpers in `grammar.rs` tests as `pub(super)` and re-export for tests via a `#[cfg(test)] pub(crate) use` in `grammar.rs`:

```rust
#[cfg(test)]
pub(crate) use tests::{compile, provider};
```

and in `grammar.rs` make the `tests` module `#[cfg(test)] pub(crate) mod tests { … }` with `compile`/`provider` as `pub(crate) fn`. Adjust visibility errors mechanically until `cargo test -p sch-engine` compiles.

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -p sch-engine cluster_geom`
Expected: COMPILE ERROR — module missing.

- [ ] **Step 4: Implement**

```rust
//! Closed-form cluster geometry: turn grammar shapes into local placements,
//! wires, junctions, power ports, and labels. No search, no routing, no I/O.
//!
//! Local coordinate frame: y grows DOWN (sheet convention). `layout_cluster`
//! normalizes so all geometry sits in `[0,w]×[0,h]` with `MARGIN` padding;
//! the caller translates everything by the cluster's sheet origin.

use std::collections::BTreeSet;

use circuit_lang::model::{Block, Component, NetName, PinTarget, RefDes};
use indexmap::IndexMap;

use crate::emit::Dir;
use crate::grammar::{is_ground, Bank, Chain, ChainClass, Cluster, Link};

/// Horizontal pitch between bank members, mm.
pub const BANK_PITCH: f64 = 7.62;
/// Bus offset beyond the outermost pin ends, mm.
const BUS_DROP: f64 = 2.54;
/// Horizontal pitch between node slots (hangs side by side), mm.
const SLOT_PITCH: f64 = 10.16;
/// Riser length from a spine end to its power port, mm.
const RISER_MM: f64 = 5.08;
/// Padding around the cluster content, mm.
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
    /// `[w, h]` after `normalize`.
    pub envelope: [f64; 2],
}

/// Rotate an angle-0 sheet-space pin end to instance `angle` (0/90/180/270).
fn rotate_end0(end0: [f64; 2], angle: f64) -> [f64; 2] {
    // end0 = transform_offset(local, 0) = [lx, -ly]  =>  local = [x, -y].
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

/// Stack a chain pin-to-pin from `start`, downward (+y) or upward (−y).
/// Each link's a-end coincides with the running point (pin-coincident joins —
/// no wires needed). Returns the final b-end and the inter-link joint points
/// `(through-net, point)`.
pub fn stack_chain(
    g: &mut ClusterGeom,
    links: &[Link],
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
        let center = [p[0] - ra[0], p[1] - ra[1]];
        g.placements.push((l.refdes.clone(), center, angle));
        g.covered.insert((l.refdes.clone(), l.a_pin.clone()));
        g.covered.insert((l.refdes.clone(), l.b_pin.clone()));
        p = add2(center, rb);
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
        let center = [origin[0] + i as f64 * BANK_PITCH, origin[1]];
        g.placements.push((refdes.clone(), center, angle));
        top_pts.push(add2(center, rotate_end0(ea, angle)));
        bot_pts.push(add2(center, rotate_end0(eb, angle)));
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
            // Junctions: interior taps, plus the port end (3-way join there).
            if i + 1 < n {
                g.junctions.push([p[0], bus_y]);
            }
        }
        g.wires.push((
            [pts[0][0], bus_y],
            [pts[n - 1][0], bus_y],
            net.clone(),
        ));
        g.ports.push((net.clone(), [pts[0][0], bus_y]));
    }
    g.tap_points.insert(bank.a_net.clone(), [top_pts[0][0], bus_top]);
}
```

Add `pub mod cluster_geom;` to `lib.rs`.

- [ ] **Step 5: Run, fix the exact expected coordinates if the transform disagrees** (the test values assume `rotate_end0([0,-3.81], 180) == [0, 3.81]` — if `transform_offset`'s extracted math yields a sign surprise, the BUG is in the extraction, not the test; re-check against `pin_endpoint`)

Run: `cargo test -p sch-engine cluster_geom`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/sch-engine/src/cluster_geom.rs crates/sch-engine/src/emit.rs crates/sch-engine/src/lib.rs crates/sch-engine/src/grammar.rs
git commit -m "feat(sch-engine): cluster geometry — vertical stacking + bused banks"
```

---

### Task 7: Node-star composition (`layout_cluster` for hang clusters)

**Files:**
- Modify: `crates/sch-engine/src/cluster_geom.rs`

- [ ] **Step 1: Write the failing test** (append to `cluster_geom.rs` tests)

```rust
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
            geom.placements
                .iter()
                .find(|(refdes, _, _)| refdes == r)
                .map(|(_, at, _)| *at)
                .unwrap()
        };
        // R7 hangs UP from the node, R8 and C3 hang DOWN side by side.
        assert!(pos("R7")[1] < pos("R8")[1]);
        assert_eq!(pos("R7")[0], pos("R8")[0], "first up and first down share the node x");
        assert_eq!(pos("C3")[0], pos("R8")[0] + SLOT_PITCH);
        assert_eq!(pos("C3")[1], pos("R8")[1]);
        // Ports: one VCC (up), two GND (one per down-hang).
        assert_eq!(
            geom.ports.iter().filter(|(n, _)| n == "VCC").count(),
            1
        );
        assert_eq!(geom.ports.iter().filter(|(n, _)| n == "GND").count(), 2);
        // Node wire exists on OUT with a junction at the C3 tap, and the OUT
        // label sits at the right end of the node wire.
        assert!(geom.wires.iter().any(|(_, _, n)| n == "OUT"));
        assert_eq!(geom.junctions.len(), 1);
        let label = geom.labels.iter().find(|(n, _, _)| n == "OUT").unwrap();
        assert!(matches!(label.2, Dir::East));
        // Normalized: nothing at negative coordinates, envelope is positive.
        assert!(geom.envelope[0] > 0.0 && geom.envelope[1] > 0.0);
        for (_, at, _) in &geom.placements {
            assert!(at[0] >= 0.0 && at[1] >= 0.0, "normalized: {at:?}");
        }
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p sch-engine divider_star`
Expected: COMPILE ERROR — `layout_cluster` missing.

- [ ] **Step 3: Implement** (append to `cluster_geom.rs`)

```rust
/// Body extents `[w, h]` per refdes (for envelope/normalize).
pub type SizeFn<'a> = &'a dyn Fn(&str) -> [f64; 2];

/// Lay out one cluster. `labeled` is the set of nets that must carry a net
/// label somewhere in this cluster (anchor-tapped, cross-block, or multi-way
/// nodes) — each gets exactly one label at its node/joint point.
pub fn layout_cluster(
    cluster: &Cluster,
    block: &Block,
    labeled: &BTreeSet<NetName>,
    pins: PinEndFn,
    sizes: SizeFn,
) -> ClusterGeom {
    let mut g = ClusterGeom::default();
    let mut joints: Vec<(NetName, [f64; 2])> = Vec::new();
    // Node nets and their x position / next free slot, discovered as we lay.
    let mut node_x: IndexMap<NetName, f64> = IndexMap::new();
    let mut node_slot: IndexMap<NetName, (f64, f64)> = IndexMap::new(); // (next down x, next up x)
    let mut x_cursor = 0.0_f64; // for standalone items appended at the right

    // Spine: the longest Series chain (Task 8 lays it; absent here, the star
    // case). All non-spine chains hang off node nets at y=0.
    let spine = cluster
        .chains
        .iter()
        .enumerate()
        .filter(|(_, c)| c.class == ChainClass::Series)
        .max_by(|(ia, a), (ib, b)| {
            a.links.len().cmp(&b.links.len()).then(ib.cmp(ia))
        })
        .map(|(i, _)| i);
    if let Some(si) = spine {
        lay_spine(&mut g, &cluster.chains[si], &mut node_x, &mut joints, pins);
    }

    // Hang chains: ToRail at their node; RailRail multi-link standalone.
    for (i, chain) in cluster.chains.iter().enumerate() {
        if Some(i) == spine {
            continue;
        }
        match chain.class {
            ChainClass::ToRail => {
                let node = chain.start_net().to_string();
                // Ground-ish rails hang down; positive rails hang up.
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
                    // Tap stub along the node line + junction at the tap.
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
                // Standalone vertical run: positive port on top, GND below.
                let x = x_cursor;
                x_cursor += SLOT_PITCH;
                let top = [x, 0.0];
                g.ports.push((chain.start_net().to_string(), top));
                let (end, js) = stack_chain(&mut g, &chain.links, top, true, pins);
                joints.extend(js);
                g.ports.push((chain.end_net().to_string(), end));
            }
            ChainClass::Series => {
                // Secondary series chains degrade to their own horizontal run
                // below the existing content (Task 8 handles the primary).
                let y = 30.0; // refined in Task 8's lay_spine_secondary
                let (_, js) = stack_chain(&mut g, &chain.links, [0.0, y], true, pins);
                joints.extend(js);
            }
        }
    }

    // Banks: at their node when ToRail-ish (a_net is a node), else standalone.
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

    // One label per labeled net, at its node/joint/tap point, reading East.
    let mut points: IndexMap<NetName, [f64; 2]> = IndexMap::new();
    for (net, &x) in &node_x {
        points.insert(net.clone(), [x, 0.0]);
    }
    for (net, p) in &joints {
        points.entry(net.clone()).or_insert(*p);
    }
    for net in labeled {
        if let Some(&p) = points.get(net) {
            // Label past the LAST occupied slot on this node so it never
            // overprints a hang's tap point.
            let ext = node_slot
                .get(net)
                .map(|s| s.0.max(s.1))
                .unwrap_or(p[0] + SLOT_PITCH);
            let lp = [ext.max(p[0] + SLOT_PITCH), p[1]];
            g.wires.push((p, lp, net.clone()));
            g.labels.push((net.clone(), lp, Dir::East));
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
        grow(*p, [2.54, 5.08]); // power symbol body allowance
    }
    for (_, p, _) in &g.labels {
        grow(*p, [12.7, 1.27]); // label text allowance, reading East
    }
    if g.placements.is_empty() && g.wires.is_empty() {
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

/// Placeholder until Task 8: a cluster reaching here has no Series chain.
fn lay_spine(
    _g: &mut ClusterGeom,
    _chain: &Chain,
    _node_x: &mut IndexMap<NetName, f64>,
    _joints: &mut Vec<(NetName, [f64; 2])>,
    _pins: PinEndFn,
) {
    unimplemented!("Task 8");
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p sch-engine cluster_geom`
Expected: PASS (the star test has no Series chain, so the `lay_spine` stub is never hit).

- [ ] **Step 5: Commit**

```bash
git add crates/sch-engine/src/cluster_geom.rs
git commit -m "feat(sch-engine): node-star cluster composition (hangs, slots, labels)"
```

---

### Task 8: Horizontal spines, risers, L-exits

**Files:**
- Modify: `crates/sch-engine/src/cluster_geom.rs`

- [ ] **Step 1: Write the failing test** (append)

```rust
    #[test]
    fn power_entry_spine_runs_left_to_right_with_riser_and_bank_below() {
        // MCP1703 input side: 5V_BUS -> F1 -> VI node, bank {C1,C2} on the node.
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
      RX: {part: Device:R, value: 1k, between: [BUS_5V, GND]}
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

        let f1 = geom
            .placements
            .iter()
            .find(|(r, _, _)| r == "F1")
            .unwrap();
        assert_eq!(f1.2, 90.0, "spine elements lie horizontal");
        // C1/C2 hang below the spine y.
        let c1 = geom.placements.iter().find(|(r, _, _)| r == "C1").unwrap();
        assert!(c1.1[1] > f1.1[1]);
        // Labels exist for both labeled nets; BUS_5V reads West (left end).
        let bus = geom.labels.iter().find(|(n, _, _)| n == "BUS_5V").unwrap();
        assert!(matches!(bus.2, Dir::West));
        assert!(geom.labels.iter().any(|(n, _, _)| n == "N_VI"));
    }

    #[test]
    fn pin_tapped_chain_hangs_to_gnd_with_single_port() {
        // Q (anchor-tapped) -> R1 -> D1 -> GND: a ToRail hang with the label
        // at the top and one GND port at the bottom (the spec's L-exit arrives
        // via Task 10's horizontal join wire + this vertical hang).
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
        // The chain is ToRail (Q -> GND): hangs down, GND port at the bottom.
        let gnd_ports: Vec<_> = geom.ports.iter().filter(|(n, _)| n == "GND").collect();
        assert_eq!(gnd_ports.len(), 1);
        let q = geom.labels.iter().find(|(n, _, _)| n == "Q").unwrap();
        let r1 = geom.placements.iter().find(|(r, _, _)| r == "R1").unwrap();
        assert!(gnd_ports[0].1[1] > r1.1[1], "GND below the chain");
        assert!(q.1[1] < gnd_ports[0].1[1], "Q label at the top of the hang");
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p sch-engine cluster_geom`
Expected: `power_entry_spine` test PANICS at `unimplemented!("Task 8")`.

- [ ] **Step 3: Implement `lay_spine`** (replace the stub)

```rust
/// Lay the primary Series chain horizontally at y=0, left→right, elements at
/// 90°, pin-coincident joins. Rail endpoints get a riser + port (up for
/// positive rails, down for ground). Registers node x positions for hangs.
fn lay_spine(
    g: &mut ClusterGeom,
    chain: &Chain,
    node_x: &mut IndexMap<NetName, f64>,
    joints: &mut Vec<(NetName, [f64; 2])>,
    pins: PinEndFn,
) {
    let mut p = [0.0_f64, 0.0_f64];
    node_x.insert(chain.start_net().to_string(), p[0]);
    for (i, l) in chain.links.iter().enumerate() {
        let ea = pins(&l.refdes, &l.a_pin).unwrap_or([0.0, -3.81]);
        let eb = pins(&l.refdes, &l.b_pin).unwrap_or([0.0, 3.81]);
        // 90/270 turns a vertical body horizontal; pick the one putting the
        // rotated a-end on the LEFT (the approach side).
        let (r90a, r90b) = (rotate_end0(ea, 90.0), rotate_end0(eb, 90.0));
        let angle = if r90a[0] < r90b[0] { 90.0 } else { 270.0 };
        let ra = rotate_end0(ea, angle);
        let rb = rotate_end0(eb, angle);
        let center = [p[0] - ra[0], p[1] - ra[1]];
        g.placements.push((l.refdes.clone(), center, angle));
        g.covered.insert((l.refdes.clone(), l.a_pin.clone()));
        g.covered.insert((l.refdes.clone(), l.b_pin.clone()));
        p = add2(center, rb);
        if i + 1 < chain.links.len() {
            joints.push((l.b_net.clone(), p));
            node_x.insert(l.b_net.clone(), p[0]);
        }
    }
    node_x.insert(chain.end_net().to_string(), p[0]);
    // Rail endpoints: riser + port. (Series chains have non-power endpoints by
    // definition, but a reversed ToRail rendered as spine — future — lands
    // here too; keep it general and cheap.)
}
```

Then in `layout_cluster`, after `lay_spine`, handle the spine's endpoint labels with direction: when emitting the per-net labels (the loop that already exists), use `Dir::West` when the labeled net's point x equals the spine start x (`node_x[chain.start_net()]` for the spine cluster) and the label point would extend LEFT: concretely, replace the label loop with:

```rust
    let spine_start: Option<NetName> = spine.map(|si| cluster.chains[si].start_net().to_string());
    for net in labeled {
        if let Some(&p) = points.get(net) {
            let west = spine_start.as_deref() == Some(net.as_str());
            let (lp, dir) = if west {
                ([p[0] - SLOT_PITCH, p[1]], Dir::West)
            } else {
                ([p[0] + SLOT_PITCH, p[1]], Dir::East)
            };
            g.wires.push((
                [p[0].min(lp[0]), p[1]],
                [p[0].max(lp[0]), p[1]],
                net.clone(),
            ));
            g.labels.push((net.clone(), lp, dir));
            g.tap_points.entry(net.clone()).or_insert(p);
        }
    }
```

Also fix the secondary-Series placeholder from Task 7: replace the hard-coded `y = 30.0` arm with a horizontal run using the same element math as `lay_spine` but at `y = y_secondary` where `y_secondary` starts at `40.0` and advances by `20.0` per extra series chain (rare; lint will note it in Task 12 — record `g` has no degradation channel, so just lay it; the note comes from grammar/reconcile).

- [ ] **Step 4: Run the tests**

Run: `cargo test -p sch-engine cluster_geom`
Expected: PASS. Iterate on coordinate expectations ONLY by checking the math by hand — the invariants (horizontal angle, hang below spine, label directions, single GND port) must hold as written.

- [ ] **Step 5: Commit**

```bash
git add crates/sch-engine/src/cluster_geom.rs
git commit -m "feat(sch-engine): horizontal spines with node registration + label directions"
```

---

## Part D — Placement integration

### Task 9: `place()` packs anchors + cluster envelopes; angles in `Layout`

**Files:**
- Modify: `crates/sch-engine/src/place.rs` (replace `block_clusters`/`layout_cluster`/`layout_block` with unit packing)
- Modify: `crates/sch-engine/src/grammar.rs` (add `Cluster::anchor_taps` — used in Task 10, declared now so the struct is stable)

This REPLACES the decouple-cluster packing: synthesized decouple caps are rail→rail chain elements, so the grammar turns them into banks automatically. Parent adjacency survives via unit ordering.

- [ ] **Step 1: Write the failing tests** (replace the existing `place.rs` cluster tests — `decouple_caps_form_a_row_beside_parent` and the `cluster_bound` test — with these; keep the band/determinism tests, updating their `place(...)` calls to the new signature)

```rust
    use crate::cluster_geom::ClusterGeom;
    use crate::grammar::BlockGraph;

    /// Empty grammar (no clusters) for blocks that should pack as plain anchors.
    fn no_grammar(design: &Design) -> IndexMap<String, BlockGraph> {
        design
            .blocks
            .keys()
            .map(|n| (n.clone(), BlockGraph::default()))
            .collect()
    }

    #[test]
    fn cluster_members_place_at_origin_plus_local_offset() {
        let design = compile(
            "
version: 1
name: t
rails: [3V3, GND]
blocks:
  a:
    components:
      C1: {part: Device:C, value: 100n, between: [3V3, GND]}
      C2: {part: Device:C, value: 100n, between: [3V3, GND]}
",
        );
        let provider = circuit_lang::MockSymbolProvider::with_basics();
        let graphs: IndexMap<String, BlockGraph> = design
            .blocks
            .keys()
            .map(|n| (n.clone(), crate::grammar::analyze(&design, n, &provider)))
            .collect();
        let mock_pins = |_: &str, pin: &str| match pin {
            "1" => Some([0.0, -3.81]),
            "2" => Some([0.0, 3.81]),
            _ => None,
        };
        let geoms: IndexMap<String, Vec<ClusterGeom>> = graphs
            .iter()
            .map(|(n, g)| {
                let gs = g
                    .clusters
                    .iter()
                    .map(|c| {
                        crate::cluster_geom::layout_cluster(
                            c,
                            &design.blocks[n],
                            &std::collections::BTreeSet::new(),
                            &mock_pins,
                            &|_| [5.08, 10.16],
                        )
                    })
                    .collect();
                (n.clone(), gs)
            })
            .collect();

        let layout = place(&design, &SizeMap::new(), &graphs, &geoms);
        let key = cluster_key("a", 0);
        let origin = layout.cluster_origins[&key];
        let local_c1 = geoms["a"][0]
            .placements
            .iter()
            .find(|(r, _, _)| r == "C1")
            .unwrap()
            .1;
        assert_eq!(
            layout.positions["C1"],
            crate::grid::snap_point([origin[0] + local_c1[0], origin[1] + local_c1[1]])
        );
        // Bank members carry their geometry angle.
        assert!(layout.angles.contains_key("C1"));
    }

    #[test]
    fn rows_center_units_vertically() {
        // One tall anchor + one short anchor in a row: the short one's center
        // must sit at the row's vertical middle, not the top.
        let design = compile(
            "
version: 1
name: t
rails: []
blocks:
  a:
    components:
      U1: {part: Mock:BIG, pins: {A: N1, B: N2, C: N3, D: N4}}
      U2: {part: Mock:SMALL, pins: {A: N5, B: N6, C: N7}}
",
        );
        let mut sizes = SizeMap::new();
        sizes.insert("U1".into(), [20.0, 60.0]);
        sizes.insert("U2".into(), [20.0, 10.0]);
        let layout = place(&design, &sizes, &no_grammar(&design), &IndexMap::new());
        let (u1, u2) = (layout.positions["U1"], layout.positions["U2"]);
        // Same row (U2 beside U1), centered: |y difference| small relative to
        // the height difference (top-aligned would put U2's center ~25mm above).
        assert!((u1[1] - u2[1]).abs() < 5.1, "u1={u1:?} u2={u2:?}");
    }
```

The `Mock:BIG`/`Mock:SMALL` parts must exist in the test provider — extend the `compile` helper in `place.rs` tests to add them (4 and 3 `Other` pins respectively) following the `Mock:REG` pattern from `grammar.rs` tests.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p sch-engine place`
Expected: COMPILE ERROR — `place` arity, `cluster_key`, `Layout::angles` missing.

- [ ] **Step 3: Implement**

In `grammar.rs`, extend `Cluster` (with `Default` still derivable):

```rust
pub struct Cluster {
    pub chains: Vec<Chain>,
    pub banks: Vec<Bank>,
    /// Candidate pin anchors: cluster endpoint nets joining exactly one anchor
    /// pin in-block and nothing external: (net, anchor refdes, pin).
    pub anchor_taps: Vec<(NetName, RefDes, String)>,
}
```

Populate at the end of `analyze` (after clusters are final):

```rust
    for cluster in &mut clusters {
        let mut nets: BTreeSet<NetName> = BTreeSet::new();
        for c in &cluster.chains {
            nets.insert(c.start_net().to_string());
            nets.insert(c.end_net().to_string());
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
```

In `place.rs`, the new shape (full replacement of the cluster/block layout section; `band_rank`, `snap_up`, `cell_of`, constants, and the band loop in `place` survive):

```rust
use crate::cluster_geom::ClusterGeom;
use crate::grammar::BlockGraph;
use circuit_lang::model::Origin;

#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    pub positions: IndexMap<RefDes, [f64; 2]>,
    /// Engine-chosen orientation per component (degrees). Cluster members get
    /// their geometry angle; anchors default to 0 (RailSpan flip handled by
    /// reconcile's initial_angle for non-cluster components).
    pub angles: IndexMap<RefDes, f64>,
    /// Absolute sheet origin of each cluster, keyed by [`cluster_key`].
    pub cluster_origins: IndexMap<String, [f64; 2]>,
}

/// Stable cluster identity for layout + reconciliation: block name + index in
/// the block's deterministic cluster order.
pub fn cluster_key(block: &str, idx: usize) -> String {
    format!("{block}\u{0}{idx}")
}

/// One packable unit inside a block.
enum Unit {
    Anchor(RefDes),
    Cluster(usize),
}

/// Deterministic unit order: anchors in natural order, each immediately
/// followed by the clusters whose members are its synthesized children
/// (decouple banks stay beside their parent), then remaining clusters in
/// cluster order.
fn block_units(block: &Block, graph: &BlockGraph) -> Vec<Unit> {
    let mut units = Vec::new();
    let mut used: Vec<bool> = vec![false; graph.clusters.len()];
    for anchor in &graph.anchors {
        units.push(Unit::Anchor(anchor.clone()));
        for (ci, cluster) in graph.clusters.iter().enumerate() {
            if used[ci] {
                continue;
            }
            let child_of_anchor = cluster.members().iter().any(|m| {
                matches!(
                    &block.components[m.as_str()].origin,
                    Origin::Synthesized { parent, .. } if parent == anchor
                )
            });
            if child_of_anchor {
                used[ci] = true;
                units.push(Unit::Cluster(ci));
            }
        }
    }
    for (ci, _) in graph.clusters.iter().enumerate() {
        if !used[ci] {
            units.push(Unit::Cluster(ci));
        }
    }
    units
}

/// Pack a block's units into rows wrapping at a ~square width; rows center
/// their units vertically. Returns unit top-left origins + block envelope.
fn pack_units(extents: &[[f64; 2]]) -> (Vec<[f64; 2]>, [f64; 2]) {
    let total: f64 = extents.iter().map(|e| e[0] * e[1]).sum();
    let target_w = total
        .sqrt()
        .max(extents.iter().map(|e| e[0]).fold(0.0, f64::max));
    // First pass: assign rows.
    let mut rows: Vec<Vec<usize>> = vec![Vec::new()];
    let mut x = 0.0;
    for (i, e) in extents.iter().enumerate() {
        if x > 0.0 && x + e[0] > target_w {
            rows.push(Vec::new());
            x = 0.0;
        }
        rows.last_mut().unwrap().push(i);
        x += e[0];
    }
    // Second pass: place with vertical centering per row.
    let mut origins = vec![[0.0, 0.0]; extents.len()];
    let mut y = 0.0;
    let mut max_w = 0.0_f64;
    for row in &rows {
        let row_h = row.iter().map(|&i| extents[i][1]).fold(0.0, f64::max);
        let mut x = 0.0;
        for &i in row {
            origins[i] = [x, y + (row_h - extents[i][1]) / 2.0];
            x += extents[i][0];
        }
        max_w = max_w.max(x);
        y += row_h;
    }
    (origins, [max_w, y])
}

pub fn place(
    design: &Design,
    sizes: &SizeMap,
    graphs: &IndexMap<String, BlockGraph>,
    geoms: &IndexMap<String, Vec<ClusterGeom>>,
) -> Layout {
    // … keep the existing band ordering exactly (sort blocks by band_rank,
    // band x accumulation, per-band y cursor). Per block, replace the old
    // `layout_block` call with:
    //
    //   let graph = graphs.get(block_name) — default empty BlockGraph;
    //   let units = block_units(block, graph);
    //   let extents: Vec<[f64;2]> = units of:
    //       Unit::Anchor(r)   -> cell_of(r, sizes)
    //       Unit::Cluster(ci) -> geoms[block_name][ci].envelope
    //   let (origins, env) = pack_units(&extents);
    //
    // and emit positions:
    //   Anchor(r): center = block_origin + unit_origin + extents/2,
    //              positions[r] = snap_point(center)
    //   Cluster(ci): let o = block_origin + unit_origin (snap_point it);
    //              cluster_origins.insert(cluster_key(block_name, ci), o);
    //              for (refdes, local, angle) in &geoms[block_name][ci].placements {
    //                  positions[refdes] = snap_point(o + local);
    //                  angles[refdes] = angle;
    //              }
    //
    // Components in neither set (anchors are graph.anchors; cluster members
    // come from placements) CANNOT exist — analyze() classifies every
    // component — but guard anyway: any block component missing from
    // `positions` after the loop gets a fallback cell appended below the
    // block envelope (and counts as a degradation in Task 12).
}
```

Delete `block_clusters`, `layout_cluster`, `layout_block`, and the `Cluster` type alias from `place.rs`. Update remaining `place(&design, &sizes)` call sites in place.rs tests to the 4-argument form (using `no_grammar`/empty geoms for the band tests).

`crates/sch-engine/src/lib.rs` (`emit_design`) and `reconcile.rs` will not compile until Task 11 — to keep this task self-contained, give both call sites a TEMPORARY shim: build `graphs` via `grammar::analyze` per block with the real provider and `geoms` via `cluster_geom::layout_cluster` with a pin callback `|_, _| None` and sizes from the SizeMap, but DO NOT yet consume angles/cluster origins in emission (emission still places every component at `layout.positions[refdes]` with `initial_angle`). The schematic output changes (banks move), which is expected; the bluepill integration test asserts ERC/netlist invariants, not positions — it must stay green.

- [ ] **Step 4: Run the engine suite**

Run: `cargo test -p sch-engine`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/sch-engine/src
git commit -m "feat(sch-engine): unit packing — anchors + cluster envelopes, row centering"
```

---

### Task 10: Pin-anchored cluster slotting (East/West v1)

**Files:**
- Modify: `crates/sch-engine/src/place.rs`

A cluster with exactly ONE `anchor_taps` entry whose anchor lives in the same block snaps beside that pin instead of packing: cluster tap point aligns with the pin, joined later by a wire (Task 11 emits it). v1 handles East/West pins; anything else falls back to packing.

- [ ] **Step 1: Write the failing test** (append to `place.rs` tests)

```rust
    #[test]
    fn single_tap_cluster_slots_beside_its_anchor_pin() {
        let design = compile(
            "
version: 1
name: t
rails: [VCC, GND]
blocks:
  a:
    components:
      R1: {part: Device:R, value: 5k1, between: [CC1, GND]}
      U1: {part: Mock:BIG, pins: {A: CC1, B: N2, C: N3, D: N4}}
",
        );
        let provider = place_test_provider(); // the helper extended in Task 9
        let graphs: IndexMap<String, BlockGraph> = design
            .blocks
            .keys()
            .map(|n| (n.clone(), crate::grammar::analyze(&design, n, &provider)))
            .collect();
        assert_eq!(graphs["a"].clusters[0].anchor_taps.len(), 1);

        let mock_pins = |_: &str, pin: &str| match pin {
            "1" => Some([0.0, -3.81]),
            "2" => Some([0.0, 3.81]),
            _ => None,
        };
        let geoms = build_test_geoms(&design, &graphs, &mock_pins); // helper from Task 9 test, extracted
        // U1's pin A points East from its right side, 10mm from origin.
        let mut pin_ends = AnchorPinEnds::new();
        pin_ends.insert(("U1".to_string(), "A".to_string()), ([10.16, 0.0], crate::emit::Dir::East));

        let layout = place_with_anchor_pins(&design, &SizeMap::new(), &graphs, &geoms, &pin_ends);
        // The cluster tap aligns with the pin row: same y, to the East.
        let u1 = layout.positions["U1"];
        let pin = [u1[0] + 10.16, u1[1]];
        let join = layout
            .joins
            .iter()
            .find(|(_, _, n)| n == "CC1")
            .expect("join wire recorded");
        assert_eq!(join.0, pin, "join starts at the pin endpoint");
        assert_eq!(join.0[1], join.1[1], "straight horizontal join");
        assert!(join.1[0] > pin[0], "cluster sits East of the pin");
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p sch-engine single_tap_cluster`
Expected: COMPILE ERROR — `AnchorPinEnds` / `place_with_anchor_pins` / `Layout::joins` missing.

- [ ] **Step 3: Implement**

```rust
/// Anchor pin geometry: (refdes, pin) -> (offset from symbol origin at angle 0,
/// outward direction). Provided by the caller from `SymbolGeometry`.
pub type AnchorPinEnds = IndexMap<(RefDes, String), ([f64; 2], crate::emit::Dir)>;

/// Gap between an anchor pin and an anchored cluster's tap, mm.
const JOIN_MM: f64 = 5.08;
```

`Layout` gains `pub joins: Vec<([f64; 2], [f64; 2], String)>` (pin endpoint → cluster tap point, net). Keep `place(design, sizes, graphs, geoms)` as a thin wrapper over the full-fat entry point:

```rust
pub fn place(
    design: &Design, sizes: &SizeMap,
    graphs: &IndexMap<String, BlockGraph>,
    geoms: &IndexMap<String, Vec<ClusterGeom>>,
) -> Layout {
    place_with_anchor_pins(design, sizes, graphs, geoms, &AnchorPinEnds::new())
}

pub fn place_with_anchor_pins(/* …, */ pin_ends: &AnchorPinEnds) -> Layout {
    // Identical to Task 9's body, with one addition per block AFTER packing:
    // for each cluster with exactly one anchor_taps entry (net, aref, apin):
    //   - the tap net must have a tap point in the cluster geom
    //     (geom.tap_points.get(net)); otherwise skip (fallback: packed spot).
    //   - look up pin_ends[(aref, apin)]; only Dir::East | Dir::West qualify.
    //   - pin endpoint (sheet) = positions[aref] + offset  (anchors are angle 0).
    //   - desired cluster origin:
    //       East: o = [pin.x + JOIN_MM + (tap is at geom-local tap.x: o.x = pin.x + JOIN_MM - 0 …)]
    //       concretely: o = [pin.x + JOIN_MM - tap.x + tap.x, …] — write it as:
    //         o.x = pin.x + JOIN_MM            // cluster's left edge clears the pin
    //         o.y = pin.y - tap.y              // tap row aligns with the pin row
    //       West mirrors: o.x = pin.x - JOIN_MM - geom.envelope[0]
    //   - overlap check: the moved cluster rect [o, o+envelope] must not
    //     intersect any other unit rect already placed in this block
    //     (track unit rects in a Vec during packing). On intersection: skip
    //     (keep the packed position, no join).
    //   - on success: update cluster_origins + member positions to the new
    //     origin, and record layout.joins.push((pin, [o.x + tap.x, o.y + tap.y]
    //     — with the x clamped so the join is horizontal: tap.y must equal
    //     pin.y by construction —, net)).
    //
    // Note the East origin math: the join wire runs from the pin endpoint to
    // the cluster's tap point at (o.x + tap.x, pin.y). With o.x = pin.x +
    // JOIN_MM the wire length is JOIN_MM + tap.x: fine — the tap is near the
    // cluster's left margin, so the run stays short.
}
```

Everything here is deterministic: clusters are processed in cluster order; first-fit wins.

- [ ] **Step 4: Run the engine suite**

Run: `cargo test -p sch-engine`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/sch-engine/src/place.rs
git commit -m "feat(sch-engine): pin-anchored cluster slotting (East/West) + join wires"
```

---

## Part E — Reconciliation integration

### Task 11: Clusters in `emit_design_reconciled`

**Files:**
- Modify: `crates/sch-engine/src/reconcile.rs`, `crates/sch-engine/src/lib.rs` (shared input builder; remove Task 9's shim)
- Modify: `crates/sch-engine/src/emit.rs` (expose `pub(crate) fn pin_end0(env, lib_id, pin) -> io::Result<Vec<[f64;2]>>` — angle-0 sheet offsets of a pin, derived from `SymbolGeometry::load` + `transform_offset`)
- Test: `crates/sch-engine/tests/grammar_emit.rs` (create)

The contract implemented here:
- **Cluster = rigid unit.** Members emit at `origin + local`, angle from geometry. The origin is preserved from the prior sheet via the representative member (`cluster.members()[0]`); non-representative member priors are ignored (re-normalization).
- **`layout_rev` for cluster members** additionally hashes the cluster structure, so YAML edits that change chains/banks re-place the block: extend `layout_rev` with a `cluster: Option<&str>` argument carrying `grammar_rev(cluster)`:

```rust
/// Content hash of a cluster's placement-relevant structure.
pub fn grammar_rev(cluster: &crate::grammar::Cluster) -> String {
    use std::fmt::Write as _;
    let mut desc = String::new();
    for c in &cluster.chains {
        let _ = write!(desc, "chain[{:?}]:", c.class);
        for l in &c.links {
            let _ = write!(desc, "{}({}->{})|", l.refdes, l.a_net, l.b_net);
        }
    }
    for b in &cluster.banks {
        let _ = write!(desc, "bank[{}/{}]:{:?}|", b.a_net, b.b_net, b.members);
    }
    crate::ids::stable_uuid("grammar_rev", &desc)
}
```

- **Covered pins skip `emit_pin`** (their connectivity is cluster wiring); everything else (anchor pins, degraded links) keeps today's label/power-symbol path.
- **Cluster decoration** is translated by the cluster origin and emitted: wires via `add_wire_on_net`, junctions via `add_junction`, ports as power symbols (lib from `power_lib_id`, refdes `#PWR_CL_<n>` numbered globally in emission order, value = net, angle 0; record `used_nets` + `power_attach`), labels via the legacy `add_pin_label`-style writer call (use `w.add_cluster_label(net, at, dir)` — add this thin public method to `SchematicWriter` that pushes a `PinLabel { net, at, uuid_key: format!("cluster:{net}:{x}:{y}"), dir, stub: None }`).
- **Join wires** from `layout.joins` emit via `add_wire_on_net`.

- [ ] **Step 1: Write the failing integration test** (`crates/sch-engine/tests/grammar_emit.rs`)

```rust
//! Grammar-driven emission: banks bus + single ports; divider hangs; ERC and
//! netlist stay truthful. SKIPs without KiCAD.

use kicad_bridge::cli::KicadCli;
use kicad_bridge::env::KicadEnv;
use kicad_bridge::provider::RealSymbolProvider;

fn compile(env: &KicadEnv, src: &str) -> circuit_lang::Design {
    let provider = RealSymbolProvider::new(env.clone());
    let result = circuit_lang::compile(src, &provider);
    assert!(!result.diagnostics.has_errors(), "{:?}", result.diagnostics);
    result.design.unwrap()
}

const DIVIDER: &str = "
version: 1
name: divider
rails: [VCC, GND]
blocks:
  div:
    components:
      R7: {part: Device:R, value: 649k, between: [VCC, OUT]}
      R8: {part: Device:R, value: 200k, between: [OUT, GND]}
      C3: {part: Device:C, value: 47n, between: [OUT, GND]}
";

#[test]
fn divider_emits_wired_star_with_clean_erc_and_netlist() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let design = compile(&env, DIVIDER);
    let out = sch_engine::emit_design_reconciled(&env, &design, None, &Default::default()).unwrap();

    assert!(out.sch.contains("(junction"), "node tap needs a junction");
    assert!(out.sch.contains("(label \"OUT\""), "OUT keeps exactly one label");
    assert_eq!(out.sch.matches("(label \"OUT\"").count(), 1);
    assert!(!out.sch.contains("(label \"GND\""));

    let tmp = tempfile::tempdir().unwrap();
    let sch = tmp.path().join("divider.kicad_sch");
    std::fs::write(&sch, &out.sch).unwrap();

    let nl = KicadCli::new(&env).netlist(&sch).unwrap();
    let net_of = |r: &str, p: &str| {
        nl.nets
            .iter()
            .find(|n| n.nodes.contains(&(r.to_string(), p.to_string())))
            .map(|n| n.name.clone())
            .unwrap_or_default()
    };
    assert_eq!(net_of("R7", "1"), "VCC");
    assert_eq!(net_of("R7", "2"), "OUT");
    assert_eq!(net_of("R8", "1"), "OUT");
    assert_eq!(net_of("R8", "2"), "GND");
    assert_eq!(net_of("C3", "1"), "OUT");

    let erc = KicadCli::new(&env).erc(&sch).unwrap();
    assert_eq!(erc.error_count(), 0, "{:?}", erc.violations);
}

#[test]
fn bank_emits_one_power_symbol_per_bus_and_cluster_drag_survives() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let src = "
version: 1
name: bank
rails: [3V3, GND]
blocks:
  pwr:
    components:
      C1: {part: Device:C, value: 100n, between: [3V3, GND]}
      C2: {part: Device:C, value: 100n, between: [3V3, GND]}
      C3: {part: Device:C, value: 10u, between: [3V3, GND]}
";
    let design = compile(&env, src);
    let out = sch_engine::emit_design_reconciled(&env, &design, None, &Default::default()).unwrap();
    // One +3V3 and one GND power symbol INSTANCE for the whole bank (no
    // per-pin spam). Count instances via `(lib_id …)` — the bare lib name also
    // appears once in the lib_symbols section, which must not be counted.
    assert_eq!(
        out.sch.matches("(lib_id \"power:+3V3\")").count(),
        1,
        "single top port"
    );
    assert_eq!(
        out.sch.matches("(lib_id \"power:GND\")").count(),
        1,
        "single bottom port"
    );

    // Whole-cluster drag: translate every member by (+25.4, +12.7) in the
    // prior, re-emit -> members keep the dragged positions (rigid group).
    let dragged = {
        // Parse C1's emitted position, then rewrite all three (at x y 0) lines.
        // Cheap text surgery is fine here: positions are unique-enough strings.
        let mut s = out.sch.clone();
        for r in ["C1", "C2", "C3"] {
            let at = sch_engine::test_util::symbol_at(&s, r);
            let new = [at[0] + 25.4, at[1] + 12.7];
            s = sch_engine::test_util::replace_symbol_at(&s, r, new);
        }
        s
    };
    let out2 =
        sch_engine::emit_design_reconciled(&env, &design, Some(&dragged), &Default::default())
            .unwrap();
    let c1 = sch_engine::test_util::symbol_at(&out2.sch, "C1");
    let c1_orig = sch_engine::test_util::symbol_at(&out.sch, "C1");
    assert_eq!(c1, [c1_orig[0] + 25.4, c1_orig[1] + 12.7], "drag survived");
}
```

Add the two helpers as a small `pub mod test_util` in `sch-engine/src/lib.rs` (cfg-gated is NOT possible for integration tests — make it a normal tiny module documented "test support"):

```rust
/// Test support: read/rewrite a symbol's `(at x y angle)` in emitted text by
/// locating the `(property "Reference" "<refdes>"` block's parent symbol.
pub mod test_util {
    /// Position of `refdes`'s symbol instance in `sch` text.
    pub fn symbol_at(sch: &str, refdes: &str) -> [f64; 2] {
        let needle = format!("(property \"Reference\" \"{refdes}\"");
        let ref_idx = sch.find(&needle).expect("refdes present");
        let sym_idx = sch[..ref_idx].rfind("(symbol").expect("enclosing symbol");
        let at_idx = sch[sym_idx..].find("(at ").unwrap() + sym_idx + 4;
        let rest = &sch[at_idx..];
        let mut it = rest.split_whitespace();
        let x: f64 = it.next().unwrap().parse().unwrap();
        let y: f64 = it.next().unwrap().trim_end_matches(')').parse().unwrap();
        [x, y]
    }

    /// Rewrite `refdes`'s instance `(at …)` to `new`, preserving the angle.
    pub fn replace_symbol_at(sch: &str, refdes: &str, new: [f64; 2]) -> String {
        let needle = format!("(property \"Reference\" \"{refdes}\"");
        let ref_idx = sch.find(&needle).expect("refdes present");
        let sym_idx = sch[..ref_idx].rfind("(symbol").expect("enclosing symbol");
        let at_idx = sch[sym_idx..].find("(at ").unwrap() + sym_idx;
        let end = sch[at_idx..].find(')').unwrap() + at_idx + 1;
        let angle = sch[at_idx + 4..end - 1]
            .split_whitespace()
            .nth(2)
            .unwrap_or("0")
            .to_string();
        format!(
            "{}(at {} {} {}){}",
            &sch[..at_idx],
            crate::emit::fmt_coord(new[0]),
            crate::emit::fmt_coord(new[1]),
            angle,
            &sch[end..]
        )
    }
}
```

(`fmt_coord` must become `pub(crate)`→`pub` or re-export a formatting helper; pick the smallest visibility change that compiles.)

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p sch-engine --test grammar_emit`
Expected: FAIL — bank still emits per-pin power symbols (Task 9's shim doesn't consume geometry), drag test fails.

- [ ] **Step 3: Implement in `reconcile.rs`**

Build all grammar inputs once (also used by `lib.rs::emit_design` — extract as `pub(crate) fn build_grammar_inputs(env, design) -> GrammarInputs` in `reconcile.rs`):

```rust
pub(crate) struct GrammarInputs {
    pub sizes: place::SizeMap,
    pub graphs: IndexMap<String, crate::grammar::BlockGraph>,
    pub geoms: IndexMap<String, Vec<crate::cluster_geom::ClusterGeom>>,
    pub anchor_pin_ends: place::AnchorPinEnds,
    /// Cluster membership per refdes.
    pub members: IndexMap<String, MemberInfo>,
    /// All cluster-covered pins.
    pub covered: std::collections::BTreeSet<(String, String)>,
}

pub(crate) struct MemberInfo {
    /// [`place::cluster_key`] of the owning cluster.
    pub cluster_key: String,
    /// Index of the cluster within its block's `BlockGraph::clusters`.
    pub cluster_idx: usize,
    /// Local symbol-origin position inside the cluster geometry.
    pub local: [f64; 2],
    /// Engine-chosen orientation, degrees.
    pub angle: f64,
}
```

Construction:
1. `sizes` — exactly the existing loop.
2. `graphs` — `grammar::analyze` per block with `RealSymbolProvider`.
3. Pin callback for geometry: cache `(part, pin) -> Vec<[f64;2]>` via the new `emit::pin_end0`; the closure returns the FIRST end (multi-end pin names don't occur on 2-pin passives).
4. `labeled` per block: nets in `net_uses` with `external || !anchor_pins.is_empty() || chain_pins.len() > 2`, minus power nets.
5. `geoms` — `layout_cluster` per cluster with sizes fallback `[5.08, 10.16]`.
6. `anchor_pin_ends` — for every `(net, aref, apin)` in every cluster's `anchor_taps`: `emit::pin_end0` offset + direction from `SymbolGeometry` pin angle quantized exactly like `SchematicWriter::pin_dirs` does (extract that quantization into `pub(crate) fn quantize_dir(...)` if needed).
7. `members`/`covered` — walk `geoms`.

Then in `emit_design_reconciled`:

- `let auto = place::place_with_anchor_pins(design, &gi.sizes, &gi.graphs, &gi.geoms, &gi.anchor_pin_ends);`
- **Cluster origins:** before the main component loop, resolve every cluster's origin:

```rust
    let mut origins: IndexMap<String, [f64; 2]> = IndexMap::new();
    for (block_name, graph) in &gi.graphs {
        let block = &design.blocks[block_name.as_str()];
        for (ci, cluster) in graph.clusters.iter().enumerate() {
            let key = place::cluster_key(block_name, ci);
            let auto_origin = auto.cluster_origins[&key];
            let rep = cluster.members()[0].clone();
            let rep_comp = &block.components[rep.as_str()];
            let rep_local = gi.members[&rep].local; // [f64;2]
            let rev = member_rev(block, rep_comp, cluster); // layout_rev + grammar_rev
            let block_relayout = /* same Relayout match as resolve_placement */;
            let prior = prior_map.get(&Identity::of(&rep, &rep_comp.origin));
            let origin = match prior {
                Some(p) if !block_relayout
                    && p.layout_rev.as_deref().is_none_or(|r| r == rev) =>
                {
                    [p.at[0] - rep_local[0], p.at[1] - rep_local[1]]
                }
                Some(_) => {
                    *relayout_blocks.entry(block_name.clone()).or_insert(0) +=
                        cluster.members().len();
                    auto_origin
                }
                None => auto_origin,
            };
            origins.insert(key, snap_point(origin));
        }
    }
```

- **Component loop:** members short-circuit `resolve_placement`:

```rust
            let (at, angle, uuid, rev) = match gi.members.get(refdes.as_str()) {
                Some(m) => {
                    let o = origins[&m.cluster_key];
                    let at = snap_point([o[0] + m.local[0], o[1] + m.local[1]]);
                    let rev = member_rev(block, comp, &gi.graphs[block_name].clusters[m.cluster_idx]);
                    // uuid still preserved by identity when the prior had one.
                    let uuid = prior_map
                        .get(&Identity::of(refdes, &comp.origin))
                        .and_then(|p| p.uuid.clone());
                    (at, m.angle, uuid, rev)
                }
                None => { /* existing resolve_placement path, unchanged */ }
            };
```

  `member_rev(block, comp, cluster)` = the existing `layout_rev` string extended with `|cluster={grammar_rev(cluster)}` before hashing — change `layout_rev`'s signature to take `Option<&Cluster>` and pass `None` on the anchor path.
- **Pin loop:** wrap both `emit_pin` call sites: `if gi.covered.contains(&(refdes.clone(), pin.clone())) { continue; }` — still calling `record_power_role` unconditionally (flag bookkeeping must see every pin).
- **Cluster decoration** after the component loop:

```rust
    let mut pwr_n = 0usize;
    for (block_name, graph) in &gi.graphs {
        for (ci, _cluster) in graph.clusters.iter().enumerate() {
            let key = place::cluster_key(block_name, ci);
            let o = origins[&key];
            let t = |p: [f64; 2]| [p[0] + o[0], p[1] + o[1]];
            let geom = &gi.geoms[block_name][ci];
            for (a, b, net) in &geom.wires {
                w.add_wire_on_net(t(*a), t(*b), net);
            }
            for j in &geom.junctions {
                w.add_junction(t(*j));
            }
            for (net, p) in &geom.ports {
                pwr_n += 1;
                let lib = power_lib_id(net, &provider);
                w.add_power_symbol(&env, &lib, &format!("#PWR_CL{pwr_n:02}"), net, t(*p), 0.0)?;
                used_nets.insert(net.clone());
                power_attach.entry(net.clone()).or_insert(t(*p));
            }
            for (net, p, dir) in &geom.labels {
                w.add_cluster_label(net, t(*p), *dir);
                used_nets.insert(net.clone());
            }
        }
    }
    for (a, b, net) in &auto.joins {
        w.add_wire_on_net(*a, *b, net);
    }
```

- **Frames/rightmost:** replace the `resolved_at` closure with a `IndexMap<RefDes, [f64;2]>` filled during the component loop (single source stays single: the map records what was emitted).
- Power-symbol ANGLE for ground ports: `add_power_symbol(…, 0.0)` works for GND (body hangs down) and positive (body up) at attach point — identical to the spike test's convention. If ERC or the render disagrees for positive rails, the angle policy lives in ONE place here; fix it here, not in geometry.
- Remove Task 9's shim from `lib.rs::emit_design`: route it through `build_grammar_inputs` + the same emission helpers (the one-shot path is `emit_design_reconciled` with `prior: None` — if `lib.rs` already delegates, nothing to do).

- [ ] **Step 4: Run everything, sequentially**

Run: `cargo test -p sch-engine`
Expected: PASS — including `grammar_emit`, bluepill, reconcile, lift round-trip. Lift is decoration-blind (wires/junctions/power symbols are netlist-invisible) but VERIFY `lift_roundtrip` rather than assuming.

- [ ] **Step 5: Commit**

```bash
git add crates/sch-engine/src crates/sch-engine/tests/grammar_emit.rs
git commit -m "feat(sch-engine): cluster-as-unit emission — wires, buses, ports, rigid drags"
```

---

## Part F — Lint, fixtures, regression

### Task 12: Degradation counters + sparseness warning

**Files:**
- Modify: `crates/sch-engine/src/reconcile.rs`

- [ ] **Step 1: Write the failing test** (append to `grammar_emit.rs`)

```rust
#[test]
fn degradations_and_sparseness_surface_in_warnings() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    // RA/RB/RC form a pure cycle -> one cycle-break degradation note.
    let src = "
version: 1
name: cyc
rails: []
blocks:
  fb:
    components:
      RA: {part: Device:R, value: 1k, between: [N1, N2]}
      RB: {part: Device:R, value: 1k, between: [N2, N3]}
      RC: {part: Device:R, value: 1k, between: [N3, N1]}
";
    let design = compile(&env, src);
    let out = sch_engine::emit_design_reconciled(&env, &design, None, &Default::default()).unwrap();
    assert!(
        out.layout_warnings.iter().any(|w| w.starts_with("grammar: block fb: cycle")),
        "{:?}",
        out.layout_warnings
    );
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p sch-engine --test grammar_emit degradations`
Expected: FAIL — no grammar warnings emitted.

- [ ] **Step 3: Implement**

In `emit_design_reconciled`, after computing `layout_warnings`:

```rust
    let mut layout_warnings = w.layout_warnings();
    for graph in gi.graphs.values() {
        layout_warnings.extend(graph.degradations.iter().map(|d| format!("grammar: {d}")));
    }
    // Sparseness: a block whose frame area dwarfs its content area reads as
    // floating parts; flag it for the agent before any vision round is spent.
    for (block_name, block) in &design.blocks {
        let content: f64 = block
            .components
            .keys()
            .map(|r| gi.sizes.get(r).map(|s| s[0] * s[1]).unwrap_or(129.0))
            .sum();
        let Some(b) = frame_bounds.get(block_name) else { continue };
        let frame = (b[2] - b[0]) * (b[3] - b[1]);
        if content > 0.0 && frame > 3.0 * content {
            layout_warnings.push(format!(
                "sparse: block {block_name} frame is {:.0}x its content area",
                frame / content
            ));
        }
    }
```

(`frame_bounds` = store each block's computed `b` from the existing frame loop into a `BTreeMap<String, [f64;4]>` instead of recomputing.)

- [ ] **Step 4: Run, commit**

Run: `cargo test -p sch-engine --test grammar_emit`
Expected: PASS.

```bash
git add crates/sch-engine/src/reconcile.rs crates/sch-engine/tests/grammar_emit.rs
git commit -m "feat(sch-engine): grammar degradation + sparseness lint counters"
```

---

### Task 13: Reference fixtures as structural oracles

**Files:**
- Create: `docs/validation/divider-filter.circuit.yaml`, `docs/validation/mcp1703-power-entry.circuit.yaml`, `docs/validation/555-blinker.circuit.yaml`, `docs/validation/uart-level-translator.circuit.yaml`
- Create: `crates/sch-engine/tests/grammar_fixtures.rs`

- [ ] **Step 0: Verify symbol lib_ids against the local libraries** (adjust YAML below to what exists — these are best-guess ids):

```bash
grep -o '(symbol "[^"]*MCP170[03][^"]*"' /usr/share/kicad/symbols/Regulator_Linear.kicad_sym | head
grep -o '(symbol "[^"]*555[^"]*"' /usr/share/kicad/symbols/Timer.kicad_sym | head
grep -o '(symbol "[^"]*LVC2T45[^"]*"' /usr/share/kicad/symbols/Logic_LevelTranslator.kicad_sym | head
grep -o '(symbol "Polyfuse[^"]*"' /usr/share/kicad/symbols/Device.kicad_sym | head
```

Use the pin NAMES from the chosen symbols in the `pins:` maps (check with `grep -A2 '(pin ' …` or the project's symbol search tool).

- [ ] **Step 1: Write the fixtures**

`docs/validation/divider-filter.circuit.yaml`:

```yaml
version: 1
name: divider-filter
rails: [VCC, GND]
blocks:
  divider:
    components:
      R7: {part: Device:R, value: 649k, between: [VCC, OUT]}
      R8: {part: Device:R, value: 200k, between: [OUT, GND]}
      C3: {part: Device:C, value: 47n, between: [OUT, GND]}
```

`docs/validation/mcp1703-power-entry.circuit.yaml` (adjust part ids/pin names per Step 0):

```yaml
version: 1
name: mcp1703-power-entry
rails: [5V, 3V3, GND]
blocks:
  power_entry:
    components:
      F1: {part: Device:Polyfuse, value: Polyfuse, between: [5V_BUS, 5V]}
      C1: {part: Device:CP, value: 10u, between: [5V, GND]}
      C2: {part: Device:C, value: 1u, between: [5V, GND]}
      U1:
        part: Regulator_Linear:MCP1703A-3302_SOT23
        pins: {VI: 5V, VO: 3V3, GND: GND}
      C3: {part: Device:C, value: 1u, between: [3V3, GND]}
      C4: {part: Device:CP, value: 10u, between: [3V3, GND]}
      D1: {part: Device:LED, value: red, between: [LED_K, 3V3]}
      R2: {part: Device:R, value: 330, between: [GND, LED_K]}
```

`docs/validation/555-blinker.circuit.yaml` (pin names per the chosen 555 symbol):

```yaml
version: 1
name: 555-blinker
rails: [9V, GND]
blocks:
  blinker:
    components:
      U1:
        part: Timer:NE555P
        pins: {VCC: 9V, GND: GND, TR: N_TR, THR: N_TR, DIS: N_DIS, CV: N_CV, R: 9V, Q: N_Q}
      R1: {part: Device:R, value: 4k7, between: [9V, N_DIS]}
      R2: {part: Device:R, value: 10k, between: [N_DIS, N_TR]}
      C1: {part: Device:CP, value: 100u, between: [N_TR, GND]}
      C2: {part: Device:C, value: 10n, between: [N_CV, GND]}
      R3: {part: Device:R, value: 1k, between: [N_Q, N_LED]}
      D1: {part: Device:LED, value: red, between: [GND, N_LED]}
```

`docs/validation/uart-level-translator.circuit.yaml` (connector + translator block; the far MCU is a second block to exercise cross-block labels):

```yaml
version: 1
name: uart-level-translator
rails: [VCC3V3, VCCD, GNDD]
blocks:
  uart:
    components:
      J1:
        part: Connector_Generic:Conn_01x05
        pins: {Pin_1: TX_RAW, Pin_2: RX_RAW, Pin_3: NC_RTS, Pin_4: NC_CTS, Pin_5: GNDD}
      R7: {part: Device:R, value: 62R, between: [TX_RAW, B1]}
      R8: {part: Device:R, value: 62R, between: [RX_RAW, B2]}
      R9: {part: Device:R, value: 10k, between: [B1, VCC3V3]}
      R16: {part: Device:R, value: 10k, between: [B2, VCC3V3]}
      U2:
        part: Logic_LevelTranslator:SN74LVC2T45DCU
        pins: {VCCA: VCCD, VCCB: VCC3V3, GND: GNDD, A1: A1_OUT, A2: A2_OUT, B1: B1, B2: B2, DIR: VCCD}
      R15: {part: Device:R, value: 62R, between: [A1_OUT, TXD1]}
      R13: {part: Device:R, value: 62R, between: [A2_OUT, RXD1]}
      C14: {part: Device:C, value: 100n, between: [VCC3V3, GNDD]}
      C16: {part: Device:C, value: 100n, between: [VCCD, GNDD]}
  mcu:
    components:
      R20: {part: Device:R, value: 0R, between: [TXD1, RXD1]}
```

- [ ] **Step 2: Write the failing oracle test** (`crates/sch-engine/tests/grammar_fixtures.rs`)

```rust
//! The four reference fixtures (docs/validation/references/*.png) emitted from
//! circuit-YAML: ERC-clean, netlist == YAML, structurally wired. SKIPs without
//! KiCAD.

use std::path::Path;

use kicad_bridge::cli::KicadCli;
use kicad_bridge::env::KicadEnv;
use kicad_bridge::provider::RealSymbolProvider;

const FIXTURES: &[&str] = &[
    "divider-filter",
    "mcp1703-power-entry",
    "555-blinker",
    "uart-level-translator",
];

fn fixture_path(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../docs/validation/{name}.circuit.yaml"))
}

#[test]
fn fixtures_emit_erc_clean_with_truthful_netlists() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD environment detected");
        return;
    };
    let provider = RealSymbolProvider::new(env.clone());
    for name in FIXTURES {
        let src = std::fs::read_to_string(fixture_path(name)).unwrap();
        let result = circuit_lang::compile(&src, &provider);
        assert!(!result.diagnostics.has_errors(), "{name}: {:?}", result.diagnostics);
        let design = result.design.unwrap();
        let out = sch_engine::emit_design_reconciled(&env, &design, None, &Default::default())
            .unwrap_or_else(|e| panic!("{name}: {e}"));

        let tmp = tempfile::tempdir().unwrap();
        let sch = tmp.path().join(format!("{name}.kicad_sch"));
        std::fs::write(&sch, &out.sch).unwrap();

        // ERC clean.
        let erc = KicadCli::new(&env).erc(&sch).unwrap();
        assert_eq!(erc.error_count(), 0, "{name}: {:?}", erc.violations);

        // Netlist == YAML: every authored pin->net lands on the same net.
        let nl = KicadCli::new(&env).netlist(&sch).unwrap();
        for (block_name, block) in &design.blocks {
            for (refdes, comp) in &block.components {
                for (pin, target) in &comp.pins {
                    let circuit_lang::model::PinTarget::Net(want) = target else { continue };
                    let got = nl
                        .nets
                        .iter()
                        .find(|n| n.nodes.iter().any(|(r, p)| {
                            r == refdes && (p == pin || nl_pin_matches(&nl, r, p, pin))
                        }))
                        .map(|n| n.name.as_str());
                    assert_eq!(
                        got, Some(want.as_str()),
                        "{name}/{block_name}/{refdes}.{pin}"
                    );
                }
            }
        }

        // No power-net text labels anywhere.
        for rail in design.nets.iter().filter(|(_, a)| a.power).map(|(n, _)| n) {
            assert!(
                !out.sch.contains(&format!("(label \"{rail}\"")),
                "{name}: rail {rail} leaked as a text label"
            );
        }
    }
}
```

The netlist pin-name vs pin-number matching: the kernel pin map may key by NAME (e.g. `VI`) while netlists report NUMBERS. Reuse whatever the existing bluepill integration test does for this mapping (`crates/sch-engine/tests/bluepill_emit.rs` solved it already) — copy that helper as `nl_pin_matches`, or simplify the assertion to the same form the bluepill test uses.

Per-fixture structural assertions (append to the same file; one test per fixture so failures localize):

```rust
#[test]
fn mcp1703_structure() {
    let Some(env) = KicadEnv::detect() else { return };
    let provider = RealSymbolProvider::new(env.clone());
    let src = std::fs::read_to_string(fixture_path("mcp1703-power-entry")).unwrap();
    let design = circuit_lang::compile(&src, &provider).design.unwrap();
    let g = sch_engine::grammar::analyze(&design, "power_entry", &provider);
    // U1 anchors; F1 is a series chain; {C1,C2} and {C3,C4} are banks;
    // D1+R2 is one rail-rail chain (3V3 -> ... -> GND).
    assert_eq!(g.anchors, vec!["U1"]);
    let banks: Vec<_> = g.clusters.iter().flat_map(|c| &c.banks).collect();
    assert_eq!(banks.len(), 2, "{banks:?}");
    let led = g
        .clusters
        .iter()
        .flat_map(|c| &c.chains)
        .find(|c| c.links.iter().any(|l| l.refdes == "D1"))
        .unwrap();
    assert_eq!(led.class, sch_engine::grammar::ChainClass::RailRail);
    assert_eq!(led.links[0].a_net, "3V3");
    assert_eq!(led.links.last().unwrap().b_net, "GND");
}

#[test]
fn blinker_structure() {
    let Some(env) = KicadEnv::detect() else { return };
    let provider = RealSymbolProvider::new(env.clone());
    let src = std::fs::read_to_string(fixture_path("555-blinker")).unwrap();
    let design = circuit_lang::compile(&src, &provider).design.unwrap();
    let g = sch_engine::grammar::analyze(&design, "blinker", &provider);
    // R1 -> R2 -> C1 chains through the anchor-tapped nets into ONE chain.
    let chain = g
        .clusters
        .iter()
        .flat_map(|c| &c.chains)
        .find(|c| c.links.iter().any(|l| l.refdes == "R1"))
        .unwrap();
    let refs: Vec<&str> = chain.links.iter().map(|l| l.refdes.as_str()).collect();
    assert_eq!(refs, ["R1", "R2", "C1"]);
}
```

(`grammar`/`ChainClass` need `pub` re-exports from `sch_engine` — add `pub use grammar; pub use cluster_geom;` style re-exports in `lib.rs` as needed.)

- [ ] **Step 3: Run, iterate on YAML pin names until green**

Run: `cargo test -p sch-engine --test grammar_fixtures`
Expected: PASS. Failures here are usually fixture problems (wrong pin name for the real symbol) — fix the YAML first, the engine second.

- [ ] **Step 4: Commit**

```bash
git add docs/validation/*.circuit.yaml crates/sch-engine/tests/grammar_fixtures.rs crates/sch-engine/src/lib.rs
git commit -m "test(sch-engine): reference fixtures as structural + ERC + netlist oracles"
```

---

### Task 14: Render harness + full regression sweep

**Files:**
- Modify: the validation render harness from slice 1 (locate it: `grep -rn "render" crates/*/tests/ scripts/ 2>/dev/null | grep -i valid` — it renders every `docs/validation/*.circuit.yaml` or `.kicad_sch` to PNG)

- [ ] **Step 1: Add the four fixtures to the harness's input set** (if it globs `docs/validation/*.circuit.yaml` they're already included — verify by running it) and run it. Eyeball each output PNG against its reference in `docs/validation/references/`:
  - divider: R7 above the node, R8+C3 below, one OUT label, junction dot visible.
  - mcp1703: F1 hangs to a single +5V port (5V is a declared rail, so there is
    no drawn spine — rail connectivity is by power symbol, the conventional
    KiCAD style); both cap banks bused with one port per side; LED chain vertical.
  - 555: R1→R2→C1 one vertical/horizontal chain, anchor labels for DIS/TR taps.
  - uart: inline 62R chains, pullups hanging to VCC3V3, two bused decouple banks.

This is the human acceptance gate of the spec ("real tests in the loop") — attach/flag the PNGs for user review rather than self-certifying.

- [ ] **Step 2: Full sweep, sequentially**

```bash
cargo test -p circuit-lang
cargo test -p kicad-bridge
cargo test -p sch-engine
cargo test -p agent
```

Expected: ALL PASS. The bluepill integration test (`bluepill_emit.rs`) is the regression anchor: ERC-clean, zero foreign collisions, netlist unchanged. Its decoupling caps now render as bused banks — if any of its assertions hard-coded the old per-cap power symbols, update THOSE assertions to the bank invariants (single port per bus), keeping ERC/netlist assertions untouched.

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "test: fixture render harness + full regression sweep for layout grammar"
```

---

## Done criteria (maps to spec)

- [ ] Roles/chains/banks/clusters inferred from the netlist graph alone (Tasks 1–3)
- [ ] Junctions + same-net wire joins; foreign-net retraction unchanged (Tasks 4–5)
- [ ] Banks render bused with one power symbol per side (Tasks 6, 11)
- [ ] Hangs, spines, node wires, labels-once-per-net (Tasks 7–8)
- [ ] Anchors + cluster envelopes packed with row centering; decouple banks beside parents (Task 9)
- [ ] Pin-anchored clusters with join wires (Task 10)
- [ ] Cluster-as-unit reconciliation: whole-group drags survive, partial drags re-normalize, `grammar_rev` re-places on structure change (Task 11)
- [ ] Degradation ladder linted, never blocking (Task 12)
- [ ] Four reference fixtures: ERC-clean, netlist-truthful, structurally wired, rendered for human review (Tasks 13–14)
