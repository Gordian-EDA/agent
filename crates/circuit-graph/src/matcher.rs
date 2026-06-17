//! The generic idiom matcher: find every place a [`Pattern`] occurs in a
//! [`CircuitGraph`].
//!
//! This is attributed **subgraph matching** with an approximate / similarity
//! relaxation. Singleton roles are bound by ordered backtracking (a VF2-style
//! search: extend the partial map along an edge to an already-bound role, pruning
//! on attribute and net-kind feasibility); bank roles (`Mult::Many`) collect every
//! feasible member at once. *Optional* roles and edges may go unsatisfied at a
//! cost, so a near-miss (a crystal with one visible load cap) still matches with a
//! lower **similarity score** instead of being silently dropped. The algorithm is
//! generic over the pattern: the library is pure data, and a new idiom never
//! touches this file.

use crate::graph::{CircuitGraph, Node};
use crate::pattern::{Mult, NetMatch, NodePred, Pattern, Target};
use std::collections::{BTreeMap, BTreeSet};

/// One occurrence of an idiom in the circuit.
#[derive(Debug, Clone)]
pub struct Match {
    /// The pattern's `name` (`"crystal"`, `"decoupling"`, …).
    pub pattern: &'static str,
    /// Refdes of the node bound to the anchor role (e.g. `"U1"`).
    pub anchor: String,
    /// role name -> bound refdes(es), in binding order.
    pub bindings: BTreeMap<&'static str, Vec<String>>,
    /// Graph-similarity confidence in `[0, 1]`: fraction of roles + edges that
    /// were satisfied (required ones always are; optionals may not be).
    pub score: f64,
}

impl Match {
    /// Every bound refdes except the anchor — the cluster the host co-places.
    pub fn members(&self, anchor_role: &str) -> Vec<String> {
        self.bindings
            .iter()
            .filter(|(role, _)| **role != anchor_role)
            .flat_map(|(_, v)| v.iter().cloned())
            .collect()
    }
}

type Bindings = BTreeMap<&'static str, Vec<usize>>;

fn node_matches(pred: &NodePred, node: &Node) -> bool {
    match pred {
        NodePred::LibAny(subs) => subs.iter().any(|s| node.lib_id.contains(s)),
        NodePred::Pins(n) => node.pin_count() == *n,
        NodePred::PinsAtLeast(n) => node.pin_count() >= *n,
        NodePred::Value { lo, hi } => {
            crate::value::parse_eng(&node.value).is_some_and(|v| v >= *lo && v <= *hi)
        }
        NodePred::And(ps) => ps.iter().all(|p| node_matches(p, node)),
        NodePred::Any => true,
    }
}

/// The single bound node for a role, if it is a `One` role already bound.
fn one_node(bindings: &Bindings, role: &str) -> Option<usize> {
    bindings.get(role).and_then(|v| (v.len() == 1).then(|| v[0]))
}

/// Evaluate an edge given concrete nodes for its endpoints (`b_node` is `None` for
/// a rail edge). Folds in `negate`.
fn edge_holds(g: &CircuitGraph, e: &crate::pattern::Edge, a_node: usize, b_node: Option<usize>) -> bool {
    let raw = match &e.b {
        Target::Rail(kind) => g.touches_kind(a_node, *kind),
        Target::Role(_) => match b_node {
            Some(bn) => {
                let shared = g.shared_nets(a_node, bn);
                match e.net {
                    NetMatch::Any => !shared.is_empty(),
                    NetMatch::Kind(k) => shared.iter().any(|n| g.net_kind(n) == k),
                }
            }
            None => false,
        },
    };
    raw ^ e.negate
}

/// All *required* edges that become determined when `role` is bound to `cand`
/// must hold. Edges with a still-unbound role endpoint are deferred (checked when
/// that endpoint binds). Returns false to prune the branch.
fn required_edges_ok(g: &CircuitGraph, p: &Pattern, bindings: &Bindings, role: &str, cand: usize) -> bool {
    let resolve = |r: &str| -> Option<usize> {
        if r == role { Some(cand) } else { one_node(bindings, r) }
    };
    for e in p.edges {
        if e.optional {
            continue;
        }
        let touches = e.a == role || matches!(&e.b, Target::Role(rb) if *rb == role);
        if !touches {
            continue;
        }
        let Some(a_node) = resolve(e.a) else { continue };
        match &e.b {
            Target::Rail(_) => {
                if !edge_holds(g, e, a_node, None) {
                    return false;
                }
            }
            Target::Role(rb) => {
                let Some(b_node) = resolve(rb) else { continue };
                if !edge_holds(g, e, a_node, Some(b_node)) {
                    return false;
                }
            }
        }
    }
    true
}

/// Can node `m` serve as a member of bank role `role`, given the `One` roles bound
/// so far? Every required edge incident to `role` (to a bound One role or a rail)
/// must hold for `m`.
fn member_ok(g: &CircuitGraph, p: &Pattern, bindings: &Bindings, role: &str, m: usize) -> bool {
    for e in p.edges {
        if e.optional {
            continue;
        }
        if e.a == role {
            match &e.b {
                Target::Rail(_) => {
                    if !edge_holds(g, e, m, None) {
                        return false;
                    }
                }
                Target::Role(rb) => {
                    if let Some(bn) = one_node(bindings, rb)
                        && !edge_holds(g, e, m, Some(bn))
                    {
                        return false;
                    }
                }
            }
        } else if let Target::Role(rb) = &e.b
            && *rb == role
            && let Some(an) = one_node(bindings, e.a)
            && !edge_holds(g, e, an, Some(m))
        {
            return false;
        }
    }
    true
}

/// Order the `One` roles for backtracking: anchor first, then breadth-first along
/// role↔role edges (so each newly bound role connects to an already-bound one,
/// maximising pruning), then any unreachable singletons. `Many` roles are bound
/// after all singletons, so they are excluded here.
fn one_role_order(p: &Pattern) -> Vec<&'static str> {
    let singles: BTreeSet<&str> =
        p.roles.iter().filter(|r| matches!(r.mult, Mult::One)).map(|r| r.name).collect();
    let mut order: Vec<&'static str> = Vec::new();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut queue: Vec<&'static str> = Vec::new();
    if singles.contains(p.anchor_role) {
        queue.push(p.anchor_role);
        seen.insert(p.anchor_role);
    }
    while !queue.is_empty() {
        let r = queue.remove(0);
        order.push(r);
        for e in p.edges {
            // The role on the other end of any edge incident to `r`.
            let nbr = if e.a == r {
                if let Target::Role(rb) = &e.b { Some(*rb) } else { None }
            } else if let Target::Role(rb) = &e.b {
                (*rb == r).then_some(e.a)
            } else {
                None
            };
            if let Some(nbr) = nbr
                && singles.contains(nbr)
                && seen.insert(nbr)
            {
                queue.push(nbr);
            }
        }
    }
    // Any singleton roles not reachable from the anchor via edges.
    for r in &singles {
        if seen.insert(r) {
            order.push(r);
        }
    }
    order
}

/// Final similarity score for a complete assignment: fraction of all roles + edges
/// that are satisfied. Required ones are satisfied by construction; this rewards
/// satisfied *optional* roles/edges so a fuller match outranks a sparser one.
fn score(g: &CircuitGraph, p: &Pattern, bindings: &Bindings) -> f64 {
    let total = (p.roles.len() + p.edges.len()) as f64;
    if total == 0.0 {
        return 1.0;
    }
    let mut sat = 0.0;
    for r in p.roles {
        if bindings.get(r.name).is_some_and(|v| !v.is_empty()) {
            sat += 1.0;
        }
    }
    for e in p.edges {
        // An edge holds when it holds for *every* bound node of role `a` (a bank
        // role satisfies it only if all its members do); the far side may itself be
        // a bank (hold against any member) or a rail.
        let a_nodes: &[usize] = bindings.get(e.a).map(Vec::as_slice).unwrap_or(&[]);
        if a_nodes.is_empty() {
            continue;
        }
        let ok = a_nodes.iter().all(|&an| match &e.b {
            Target::Rail(_) => edge_holds(g, e, an, None),
            Target::Role(rb) => {
                let b_nodes: &[usize] = bindings.get(*rb).map(Vec::as_slice).unwrap_or(&[]);
                !b_nodes.is_empty() && b_nodes.iter().any(|&bn| edge_holds(g, e, an, Some(bn)))
            }
        });
        if ok {
            sat += 1.0;
        }
    }
    sat / total
}

/// Recursive backtracking over the `One` roles. On a complete singleton
/// assignment, collect the `Many` roles and emit a candidate match.
fn backtrack(
    g: &CircuitGraph,
    p: &Pattern,
    order: &[&'static str],
    depth: usize,
    bindings: &mut Bindings,
    used: &mut BTreeSet<usize>,
    out: &mut Vec<Match>,
) {
    if depth == order.len() {
        if let Some(m) = finish_many(g, p, bindings, used) {
            out.push(m);
        }
        return;
    }
    let role = order[depth];
    let pred = &p.role(role).expect("role in order exists").pred;
    for (idx, node) in g.nodes.iter().enumerate() {
        if used.contains(&idx) || !node_matches(pred, node) {
            continue;
        }
        if !required_edges_ok(g, p, bindings, role, idx) {
            continue;
        }
        bindings.insert(role, vec![idx]);
        used.insert(idx);
        backtrack(g, p, order, depth + 1, bindings, used, out);
        used.remove(&idx);
        bindings.remove(role);
    }
}

/// With all singletons bound, gather each bank role's members and assemble the
/// match (or `None` if a required bank is short of its minimum).
fn finish_many(g: &CircuitGraph, p: &Pattern, bindings: &Bindings, used: &BTreeSet<usize>) -> Option<Match> {
    let mut bindings = bindings.clone();
    let mut used = used.clone();
    for r in p.roles {
        let Mult::Many { min, max } = r.mult else { continue };
        let mut members: Vec<usize> = (0..g.nodes.len())
            .filter(|&i| !used.contains(&i) && node_matches(&r.pred, &g.nodes[i]))
            .filter(|&i| member_ok(g, p, &bindings, r.name, i))
            .collect();
        members.sort_unstable();
        members.truncate(max);
        if members.len() < min {
            if r.optional {
                continue;
            }
            return None;
        }
        for &m in &members {
            used.insert(m);
        }
        bindings.insert(r.name, members);
    }
    let s = score(g, p, &bindings);
    if s + 1e-9 < p.min_score {
        return None;
    }
    let anchor = one_node(&bindings, p.anchor_role).map(|i| g.refdes(i).to_string())?;
    let refdes_bindings: BTreeMap<&'static str, Vec<String>> = bindings
        .iter()
        .map(|(k, v)| (*k, v.iter().map(|&i| g.refdes(i).to_string()).collect()))
        .collect();
    Some(Match { pattern: p.name, anchor, bindings: refdes_bindings, score: s })
}

/// Every occurrence of `pattern` in `graph`. For each distinct anchor node only
/// the **highest-scoring** binding is kept (the fullest match), so two crystals on
/// one MCU yield two matches but a single crystal never yields duplicates.
pub fn find(graph: &CircuitGraph, pattern: &Pattern) -> Vec<Match> {
    let order = one_role_order(pattern);
    let mut raw: Vec<Match> = Vec::new();
    let mut bindings: Bindings = BTreeMap::new();
    let mut used: BTreeSet<usize> = BTreeSet::new();
    backtrack(graph, pattern, &order, 0, &mut bindings, &mut used, &mut raw);

    // Keep the best-scoring match per (anchor, member-set) signature.
    let mut best: BTreeMap<String, Match> = BTreeMap::new();
    for m in raw {
        let key = m.anchor.clone();
        match best.get(&key) {
            Some(prev) if prev.score >= m.score => {}
            _ => {
                best.insert(key, m);
            }
        }
    }
    let mut out: Vec<Match> = best.into_values().collect();
    out.sort_by(|a, b| a.anchor.cmp(&b.anchor));
    out
}

/// Match `graph` against many patterns, resolving cross-pattern contention: a
/// component is claimed by at most one idiom (the higher-scoring match wins; ties
/// break by earlier library order, i.e. pattern index). Returns the surviving
/// matches in a stable order.
pub fn find_all(graph: &CircuitGraph, patterns: &[Pattern]) -> Vec<Match> {
    let mut all: Vec<(usize, Match)> = Vec::new();
    for (pi, p) in patterns.iter().enumerate() {
        for m in find(graph, p) {
            all.push((pi, m));
        }
    }
    // Greedy claim: strongest first. A member already claimed kills the weaker match.
    all.sort_by(|(ia, a), (ib, b)| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(ia.cmp(ib))
            .then(a.anchor.cmp(&b.anchor))
    });
    let mut claimed: BTreeSet<String> = BTreeSet::new();
    let mut kept: Vec<Match> = Vec::new();
    for (pi, m) in all {
        let anchor_role = patterns[pi].anchor_role;
        let members = m.members(anchor_role);
        if members.iter().any(|r| claimed.contains(r)) {
            continue;
        }
        for r in &members {
            claimed.insert(r.clone());
        }
        kept.push(m);
    }
    kept.sort_by(|a, b| a.pattern.cmp(b.pattern).then(a.anchor.cmp(&b.anchor)));
    kept
}
