//! Derive an idiom *sketch* from a real circuit — the "learn idioms from existing
//! kicad_sch" path.
//!
//! Given a reference design already loaded as a [`CircuitGraph`] (e.g. lifted from
//! a `.kicad_sch` via the host's netlist exporter) and a seed component, snowball
//! its k-hop neighbourhood into a [`DerivedPattern`]: a role per neighbour
//! (generalised by lib-id family and pin count) and an edge per shared net
//! (carrying its kind). A maintainer then names, prunes, and promotes it into
//! `library.rs`. This turns "here is a board that does X" into a reusable matcher
//! input without hand-authoring the graph.

use crate::graph::{CircuitGraph, NetKind};
use std::collections::{BTreeMap, BTreeSet};

/// A human-readable idiom sketch extracted from a concrete subcircuit. Not a
/// `Pattern` (which is `'static` data); it is the editable intermediate a
/// maintainer turns into one.
#[derive(Debug, Clone, PartialEq)]
pub struct DerivedPattern {
    /// Suggested name (the seed's lib-id family, lower-cased).
    pub name: String,
    /// role name -> (lib-id family substring, pin count). role `"anchor"` is the seed.
    pub roles: BTreeMap<String, RoleSketch>,
    /// `(role_a, role_b, net kind)` for every shared net between two roles.
    pub edges: Vec<(String, String, NetKind)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RoleSketch {
    pub lib_family: String,
    pub pins: usize,
}

/// Reduce a KiCAD lib id to a generalisable family substring: `"Device:Crystal"`
/// stays, but a specific part like `"MCU_ST_STM32F1:STM32F103C8Tx"` generalises to
/// its library (`"MCU_ST_STM32F1"`) so the derived pattern matches the family, not
/// one part number.
fn lib_family(lib_id: &str) -> String {
    match lib_id.split_once(':') {
        // Generic libraries keep the specific symbol (R, C, Crystal, LED …).
        Some(("Device" | "power", sym)) => format!("Device:{sym}").replace("Device:Device:", "Device:"),
        Some((lib, _)) => lib.to_string(),
        None => lib_id.to_string(),
    }
}

/// Snowball the `radius`-hop neighbourhood of `seed_refdes` into a derived pattern.
/// Power/ground rails are followed but not turned into roles (they would pull in
/// the whole board); they become rail edges instead.
pub fn derive(graph: &CircuitGraph, seed_refdes: &str, radius: usize) -> Option<DerivedPattern> {
    let seed = graph.nodes.iter().position(|n| n.refdes == seed_refdes)?;

    // BFS over signal nets only (rails are too connective to traverse).
    let mut depth: BTreeMap<usize, usize> = BTreeMap::new();
    depth.insert(seed, 0);
    let mut frontier = vec![seed];
    for d in 0..radius {
        let mut next = Vec::new();
        for &n in &frontier {
            for net in graph.nodes[n].nets() {
                if graph.net_kind(net) != NetKind::Signal {
                    continue;
                }
                for (j, _) in graph.net_nodes(net) {
                    if !depth.contains_key(j) {
                        depth.insert(*j, d + 1);
                        next.push(*j);
                    }
                }
            }
        }
        frontier = next;
    }
    let members: BTreeSet<usize> = depth.keys().copied().collect();

    // Name a role per member: the seed is "anchor"; others get a family-based slug
    // with a disambiguating index.
    let mut roles: BTreeMap<String, RoleSketch> = BTreeMap::new();
    let mut role_of: BTreeMap<usize, String> = BTreeMap::new();
    let mut family_count: BTreeMap<String, usize> = BTreeMap::new();
    for &m in &members {
        let fam = lib_family(&graph.nodes[m].lib_id);
        let name = if m == seed {
            "anchor".to_string()
        } else {
            let slug = fam.rsplit(':').next().unwrap_or(&fam).to_lowercase();
            let k = family_count.entry(slug.clone()).or_insert(0);
            *k += 1;
            format!("{slug}_{k}")
        };
        roles.insert(name.clone(), RoleSketch { lib_family: fam, pins: graph.nodes[m].pin_count() });
        role_of.insert(m, name);
    }

    // One edge per (member pair, shared-net kind) and per (member, rail kind).
    let mut edges: BTreeSet<(String, String, i32)> = BTreeSet::new();
    let kind_rank = |k: NetKind| match k {
        NetKind::Power => 0,
        NetKind::Ground => 1,
        NetKind::Signal => 2,
    };
    for &a in &members {
        // rail attachments
        for net in graph.nodes[a].nets() {
            let k = graph.net_kind(net);
            if k != NetKind::Signal {
                let rail = if k == NetKind::Power { "PWR" } else { "GND" };
                edges.insert((role_of[&a].clone(), rail.to_string(), kind_rank(k)));
            }
        }
        // member-member shared signal nets
        for &b in &members {
            if a < b
                && let Some((_, k)) = graph.shared_net(a, b)
                && k == NetKind::Signal
            {
                edges.insert((role_of[&a].clone(), role_of[&b].clone(), kind_rank(k)));
            }
        }
    }
    let kind_of = |r: i32| match r {
        0 => NetKind::Power,
        1 => NetKind::Ground,
        _ => NetKind::Signal,
    };
    let edges = edges.into_iter().map(|(a, b, r)| (a, b, kind_of(r))).collect();

    Some(DerivedPattern {
        name: lib_family(&graph.nodes[seed].lib_id).rsplit(':').next().unwrap_or("idiom").to_lowercase(),
        roles,
        edges,
    })
}
