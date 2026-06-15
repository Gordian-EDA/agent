//! Sugar -> kernel lowering (spec §5.5). The reconciler and lints see
//! only the output of this pass.

use crate::diag::{Diagnostic, Diagnostics};
use crate::model::*;
use crate::provider::SymbolProvider;
use crate::surface::*;
use indexmap::IndexMap;

/// Closed alias table (spec §5.5) — exactly these five.
fn alias(part: &str) -> String {
    match part {
        "R" => "Device:R".into(),
        "C" => "Device:C".into(),
        "L" => "Device:L".into(),
        "D" => "Device:D".into(),
        "LED" => "Device:LED".into(),
        other => other.into(),
    }
}

pub fn desugar(s: &SurfaceDesign, provider: &dyn SymbolProvider) -> (Design, Diagnostics) {
    let mut diags = Diagnostics::default();
    let mut d = Design {
        name: s.name.clone(),
        description: s.description.clone(),
        lint_allow: s.lint_allow.iter().cloned().collect(),
        ..Default::default()
    };

    // rails -> power net attrs
    for (rail, _span) in &s.rails {
        d.nets.entry(rail.clone()).or_default().power = true;
    }
    for (net, attrs) in &s.nets {
        let e = d.nets.entry(net.clone()).or_default();
        e.power |= attrs.power;
        e.class = attrs.class.clone();
    }

    // surface components -> kernel components (pins still raw, resolved below)
    let mut raw_pins: Vec<RawPin> = Vec::new();
    // A refdes must be globally unique across all blocks; a second occurrence
    // would corrupt `comp_block` and pin-ref resolution, so it is a hard error.
    let mut seen_refdes: std::collections::HashSet<RefDes> = std::collections::HashSet::new();
    for (bname, sb) in &s.blocks {
        let mut block = Block {
            note: sb.note.clone(),
            components: IndexMap::new(),
        };
        for (refdes, sc) in &sb.components {
            if !seen_refdes.insert(refdes.clone()) {
                let mut diag = Diagnostic::error(
                    "duplicate-refdes",
                    format!("refdes `{refdes}` is declared more than once across blocks"),
                );
                if let Some(span) = sc.span {
                    diag = diag.with_span(span);
                }
                diags.push(diag);
                continue;
            }
            let mut sc = sc.clone();
            apply_between(refdes, &mut sc, provider, &mut diags); // Task 7
            let comp = Component {
                part: alias(&sc.part),
                value: sc.value.clone(),
                footprint: sc.footprint.clone(),
                dnp: sc.dnp,
                props: sc.props.clone(),
                origin: Origin::Authored,
                ..Default::default()
            };
            for (pin, (target, span)) in &sc.pins {
                raw_pins.push(RawPin {
                    block: bname.clone(),
                    refdes: refdes.clone(),
                    unit: None,
                    pin: pin.clone(),
                    target: target.clone(),
                    span: *span,
                });
            }
            for (unit, pins) in &sc.units {
                for (pin, (target, span)) in pins {
                    raw_pins.push(RawPin {
                        block: bname.clone(),
                        refdes: refdes.clone(),
                        unit: Some(unit.clone()),
                        pin: pin.clone(),
                        target: target.clone(),
                        span: *span,
                    });
                }
            }
            block.components.insert(refdes.clone(), comp);
        }
        d.blocks.insert(bname.clone(), block);
    }

    resolve_pins(&mut d, raw_pins, &mut diags);
    synth_decouple(&mut d, s, provider, &mut diags); // Task 8
    materialize_auto_nc(&mut d, provider); // Task R6
    lower_layout_grid(&mut d, s, &mut diags);

    (d, diags)
}

/// Strip spans off the surface `layout:` grid into the kernel model, validating
/// that every named cell resolves to an authored block or refdes.
fn lower_layout_grid(d: &mut Design, s: &SurfaceDesign, diags: &mut Diagnostics) {
    let block_names: std::collections::HashSet<&str> = s.blocks.keys().map(String::as_str).collect();
    let refdes: std::collections::HashSet<&str> = s
        .blocks
        .values()
        .flat_map(|b| b.components.keys())
        .map(String::as_str)
        .collect();
    d.layout = s
        .layout
        .iter()
        .map(|row| {
            row.iter()
                .map(|(cell, span)| {
                    if let Some(name) = cell
                        && !block_names.contains(name.as_str())
                        && !refdes.contains(name.as_str())
                    {
                        diags.push(
                            Diagnostic::error(
                                "unknown-layout-cell",
                                format!("layout cell `{name}` is not a known block or refdes"),
                            )
                            .with_span(*span),
                        );
                    }
                    cell.clone()
                })
                .collect()
        })
        .collect();
}

/// Final desugar pass: for every component whose symbol is known, any physical
/// pin not covered by an author key and whose `etype` is not `PowerInput`
/// becomes an explicit `nc` (spec §5.3.5). A net-mapped pin, an explicit `nc`,
/// or a stacked name covering the pin all count as coverage; power-input pins
/// are skipped (lint.rs already errors when they are left unconnected).
/// Markers are keyed by pin number and inserted in symbol pin order, so the
/// pass is deterministic and idempotent across a canonical round-trip.
fn materialize_auto_nc(d: &mut Design, provider: &dyn SymbolProvider) {
    for block in d.blocks.values_mut() {
        for comp in block.components.values_mut() {
            let Some(meta) = provider.symbol(&comp.part) else {
                continue; // unknown symbol — leave pins as authored
            };
            // Physical pin numbers already covered by an author key (number
            // first, then name; a stacked name covers all its physical pins).
            let mut covered: std::collections::HashSet<&str> = std::collections::HashSet::new();
            let keys: Vec<&String> = comp
                .pins
                .keys()
                .chain(comp.units.values().flatten().map(|(k, _)| k))
                .collect();
            for key in keys {
                let by_number = meta.pins.iter().filter(|p| p.number == *key);
                let mut matched = false;
                for p in by_number {
                    covered.insert(p.number.as_str());
                    matched = true;
                }
                if !matched {
                    for p in meta.pins.iter().filter(|p| p.name == *key) {
                        covered.insert(p.number.as_str());
                    }
                }
            }
            // Insert in symbol pin order for determinism.
            let to_nc: Vec<String> = meta
                .pins
                .iter()
                .filter(|p| p.etype != crate::provider::PinType::PowerInput)
                .filter(|p| !covered.contains(p.number.as_str()))
                .map(|p| p.number.clone())
                .collect();
            for number in to_nc {
                comp.pins.insert(number, PinTarget::NoConnect);
            }
        }
    }
}

struct RawPin {
    block: String,
    refdes: String,
    unit: Option<String>,
    pin: String,
    target: String,
    span: crate::diag::Span,
}

fn apply_between(
    refdes: &str,
    sc: &mut SurfaceComponent,
    provider: &dyn SymbolProvider,
    diags: &mut Diagnostics,
) {
    let Some(((a, aspan), (b, bspan))) = sc.between.take() else {
        return;
    };
    let part = alias(&sc.part);
    let Some(meta) = provider.symbol(&part) else {
        let mut d = Diagnostic::error(
            "between-unknown-symbol",
            format!("{refdes}: cannot desugar `between` — unknown symbol `{part}`"),
        )
        .with_span(aspan);
        if let Some(s) = provider.suggest(&part).into_iter().next() {
            d = d.with_suggestion(s);
        }
        diags.push(d);
        return;
    };
    if meta.pins.len() != 2 {
        diags.push(
            Diagnostic::error(
                "between-arity",
                format!(
                    "{refdes}: `between` needs a 2-pin symbol; `{part}` has {} pins",
                    meta.pins.len()
                ),
            )
            .with_span(aspan),
        );
        return;
    }
    // Map `between` args by numeric pin NUMBER, not library order: first arg →
    // lowest-numbered pin, second arg → highest (spec §5.5). Fall back to string
    // order for non-numeric pin numbers.
    let mut ordered: Vec<&crate::provider::PinMeta> = meta.pins.iter().collect();
    ordered.sort_by(
        |x, y| match (x.number.parse::<u64>(), y.number.parse::<u64>()) {
            (Ok(nx), Ok(ny)) => nx.cmp(&ny),
            _ => x.number.cmp(&y.number),
        },
    );
    // Polarized-part lint (spec §5.5): warn, suggest named pins.
    let polarized = matches!(part.as_str(), "Device:D" | "Device:LED" | "Device:CP")
        || meta.pins.iter().any(|p| p.name == "A" || p.name == "K");
    if polarized {
        diags.push(
            Diagnostic::warning(
                "between-polarized",
                format!(
                    "{refdes}: `{part}` is polarized; `between` maps pin order ({}, {}) — \
                     prefer named pins {{{}: …, {}: …}}",
                    ordered[0].number, ordered[1].number, ordered[0].name, ordered[1].name
                ),
            )
            .with_span(aspan),
        );
    }
    for (pin, target, span) in [
        (ordered[0].number.clone(), a, aspan),
        (ordered[1].number.clone(), b, bspan),
    ] {
        if sc.pins.insert(pin.clone(), (target, span)).is_some() {
            diags.push(
                Diagnostic::error(
                    "pin-conflict",
                    format!("{refdes}: pin `{pin}` set by both `between` and `pins`"),
                )
                .with_span(span),
            );
        }
    }
}

fn sanitize(pin: &str) -> String {
    pin.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Union-find over pin nodes keyed by `(refdes, pin)`. `make` interns a
/// node, `union` merges two, `find` returns a node's root (path-compressed).
#[derive(Default)]
struct PinUnionFind {
    nodes: Vec<(String, String)>,
    index: std::collections::HashMap<(String, String), usize>,
    parent: Vec<usize>,
}

impl PinUnionFind {
    fn make(&mut self, r: &str, p: &str) -> usize {
        let nodes = &mut self.nodes;
        let parent = &mut self.parent;
        *self
            .index
            .entry((r.to_string(), p.to_string()))
            .or_insert_with(|| {
                nodes.push((r.to_string(), p.to_string()));
                parent.push(nodes.len() - 1);
                nodes.len() - 1
            })
    }
    fn find(&mut self, mut i: usize) -> usize {
        while self.parent[i] != i {
            self.parent[i] = self.parent[self.parent[i]];
            i = self.parent[i];
        }
        i
    }
    fn union(&mut self, i: usize, j: usize) {
        let (ri, rj) = (self.find(i), self.find(j));
        self.parent[ri] = rj;
    }
}

fn resolve_pins(d: &mut Design, raw: Vec<RawPin>, diags: &mut Diagnostics) {
    // refdes -> block (for pin-ref targets and on-demand pin creation)
    let comp_block: std::collections::HashMap<String, String> = d
        .blocks
        .iter()
        .flat_map(|(b, bl)| bl.components.keys().map(move |r| (r.clone(), b.clone())))
        .collect();

    let mut uf = PinUnionFind::default();
    let mut named: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
    let mut placement: Vec<(RawPin, usize)> = Vec::new(); // node idx per raw pin
    let mut extra_nodes: Vec<usize> = Vec::new(); // pin-ref targets (may be unmapped)
    // First concrete net name assigned to each node (by node index). A
    // component-level pin and a unit-level pin collapse to the same node, so a
    // comp pin and a unit pin that name *different* nets are a hard conflict —
    // silently unioning/overwriting would corrupt connectivity.
    let mut node_net: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
    // (refdes, pin) explicitly declared `nc`; a pin-ref onto one of these is
    // electrically contradictory and must be diagnosed, not silently merged.
    let mut nc_pins: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();

    for rp in &raw {
        if rp.target.eq_ignore_ascii_case("nc") {
            nc_pins.insert((rp.refdes.clone(), rp.pin.clone()));
        }
    }

    for rp in raw {
        if rp.target.eq_ignore_ascii_case("nc") {
            write_pin(d, &rp, PinTarget::NoConnect);
            continue;
        }
        let i = uf.make(&rp.refdes, &rp.pin);
        // pin-ref? "<REFDES>.<pin>" where REFDES exists
        let is_ref = rp
            .target
            .split_once('.')
            .is_some_and(|(r, _)| comp_block.contains_key(r));
        if rp.target.contains('.') && !is_ref {
            diags.push(
                Diagnostic::error(
                    "bad-pin-ref",
                    format!(
                        "{}.{}: target `{}` looks like a pin-ref but no such component exists",
                        rp.refdes, rp.pin, rp.target
                    ),
                )
                .with_span(rp.span),
            );
            continue;
        }
        if is_ref {
            let (tr, tp) = rp.target.split_once('.').unwrap();
            if nc_pins.contains(&(tr.to_string(), tp.to_string())) {
                diags.push(
                    Diagnostic::error(
                        "pinref-to-nc",
                        format!(
                            "{}.{}: target `{}` refers to a pin declared `nc` (no-connect) — \
                             the two sides cannot share a net",
                            rp.refdes, rp.pin, rp.target
                        ),
                    )
                    .with_span(rp.span),
                );
                continue;
            }
            let j = uf.make(tr, tp);
            uf.union(i, j);
            extra_nodes.push(j);
        } else {
            if let Some(prev) = node_net.get(&i)
                && prev != &rp.target
            {
                diags.push(
                    Diagnostic::error(
                        "pin-conflict",
                        format!(
                            "{}.{}: pin maps to two different nets `{}` and `{}` \
                             (component- vs unit-level)",
                            rp.refdes, rp.pin, prev, rp.target
                        ),
                    )
                    .with_span(rp.span),
                );
            } else {
                node_net.insert(i, rp.target.clone());
            }
            let root = uf.find(i);
            named.insert(root, rp.target.clone());
        }
        placement.push((rp, i));
    }

    let group_name = name_groups(&mut uf, &named, &placement, diags);
    write_back(d, &mut uf, &comp_block, &group_name, placement, extra_nodes);
}

/// Assign one net name per union-find group: an explicit author name where one
/// exists (diagnosing conflicts), else `N_<smallest member>`.
fn name_groups(
    uf: &mut PinUnionFind,
    named: &std::collections::HashMap<usize, String>,
    placement: &[(RawPin, usize)],
    diags: &mut Diagnostics,
) -> std::collections::HashMap<usize, String> {
    let mut group_name: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
    let node_span: std::collections::HashMap<usize, crate::diag::Span> =
        placement.iter().map(|(rp, i)| (*i, rp.span)).collect();
    // Collect all author names per group root, deterministically. HashMap
    // iteration order is randomized, so accumulate into a sorted set per root
    // and pick the lexicographically smallest name as the group's winner;
    // emit one `net-conflict` per distinct extra name (names listed sorted).
    // `name -> span` lets the conflict diag point at a node naming the loser.
    let mut group_names: std::collections::HashMap<usize, std::collections::BTreeSet<String>> =
        std::collections::HashMap::new();
    let mut name_span: std::collections::HashMap<(usize, String), crate::diag::Span> =
        std::collections::HashMap::new();
    for (i, name) in named {
        let root = uf.find(*i);
        group_names.entry(root).or_default().insert(name.clone());
        if let Some(span) = node_span.get(i) {
            name_span.entry((root, name.clone())).or_insert(*span);
        }
    }
    for (root, names) in &group_names {
        let mut it = names.iter();
        let winner = it.next().expect("non-empty group name set").clone();
        for other in it {
            let mut diag = Diagnostic::error(
                "net-conflict",
                format!("nets `{winner}` and `{other}` joined by pin-refs"),
            );
            if let Some(span) = name_span.get(&(*root, other.clone())) {
                diag = diag.with_span(*span);
            }
            diags.push(diag);
        }
        group_name.insert(*root, winner);
    }
    // unnamed groups: N_<smallest member>. `sanitize` maps distinct pin names
    // (e.g. `A_B` and `A.B`) to the same string, so distinct roots can collide
    // on a base name. Compute (root, smallest-member, base) for every unnamed
    // root, then disambiguate collisions deterministically: sort colliding
    // roots by smallest member and append `_2`, `_3`, … to all but the first.
    let roots: Vec<usize> = (0..uf.nodes.len()).map(|i| uf.find(i)).collect();
    let mut unnamed: Vec<(usize, String, String)> = Vec::new(); // (root, smallest, base)
    let mut seen_roots: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for root in &roots {
        if group_name.contains_key(root) || !seen_roots.insert(*root) {
            continue;
        }
        let mut members: Vec<String> = (0..uf.nodes.len())
            .filter(|&j| roots[j] == *root)
            .map(|j| format!("{}_{}", uf.nodes[j].0, sanitize(&uf.nodes[j].1)))
            .collect();
        members.sort();
        let smallest = members.into_iter().next().expect("non-empty group");
        let base = format!("N_{smallest}");
        unnamed.push((*root, smallest, base));
    }
    // Group roots by base name; disambiguate within each colliding group.
    let mut by_base: std::collections::BTreeMap<String, Vec<(String, usize)>> =
        std::collections::BTreeMap::new();
    for (root, smallest, base) in unnamed {
        by_base.entry(base).or_default().push((smallest, root));
    }
    for (base, mut group) in by_base {
        if group.len() == 1 {
            group_name.insert(group[0].1, base);
            continue;
        }
        group.sort(); // by smallest member (then root) for determinism
        for (n, (_, root)) in group.into_iter().enumerate() {
            let name = if n == 0 {
                base.clone()
            } else {
                format!("{base}_{}", n + 1)
            };
            group_name.insert(root, name);
        }
    }
    group_name
}

/// Write resolved nets onto the design, creating pin-ref target pins on demand.
fn write_back(
    d: &mut Design,
    uf: &mut PinUnionFind,
    comp_block: &std::collections::HashMap<String, String>,
    group_name: &std::collections::HashMap<usize, String>,
    placement: Vec<(RawPin, usize)>,
    extra_nodes: Vec<usize>,
) {
    for (rp, i) in placement {
        let root = uf.find(i);
        write_pin(d, &rp, PinTarget::Net(group_name[&root].clone()));
    }
    // pin-ref targets that had no own mapping: create one on the component
    for j in extra_nodes {
        let (r, p) = uf.nodes[j].clone();
        let block = comp_block[&r].clone();
        let comp = d
            .blocks
            .get_mut(&block)
            .unwrap()
            .components
            .get_mut(&r)
            .unwrap();
        let already = comp.pins.contains_key(&p) || comp.units.values().any(|u| u.contains_key(&p));
        if !already {
            let root = uf.find(j);
            comp.pins
                .insert(p, PinTarget::Net(group_name[&root].clone()));
        }
    }
}

fn write_pin(d: &mut Design, rp: &RawPin, target: PinTarget) {
    let comp = d
        .blocks
        .get_mut(&rp.block)
        .and_then(|b| b.components.get_mut(&rp.refdes))
        .expect("raw pin refers to existing component");
    match &rp.unit {
        Some(u) => {
            comp.units
                .entry(u.clone())
                .or_default()
                .insert(rp.pin.clone(), target);
        }
        None => {
            comp.pins.insert(rp.pin.clone(), target);
        }
    }
}

fn synth_decouple(
    d: &mut Design,
    s: &SurfaceDesign,
    provider: &dyn SymbolProvider,
    diags: &mut Diagnostics,
) {
    for (bname, sb) in &s.blocks {
        for (refdes, sc) in &sb.components {
            if sc.decouple.is_empty() {
                continue;
            }
            let comp = &d.blocks[bname].components[refdes];
            // Author pin-map keys may be pin NUMBERS (e.g. `{1: 3V3}`), so the
            // VDD*/VSS* prefix test must run against the symbol's pin NAME, not
            // the raw key. Resolve each key via the provider (number-first, then
            // name); fall back to the raw key only when the symbol is unknown.
            let meta = provider.symbol(&comp.part);
            let resolved_name = |key: &str| -> String {
                let Some(meta) = meta else {
                    return key.to_string();
                };
                if let Some(pm) = meta.pins.iter().find(|p| p.number == key) {
                    return pm.name.clone();
                }
                if let Some(pm) = meta.pins.iter().find(|p| p.name == key) {
                    return pm.name.clone();
                }
                key.to_string()
            };
            let rail = |prefixes: &[&str]| -> Vec<NetName> {
                let mut nets: Vec<NetName> = comp
                    .pins
                    .iter()
                    .chain(comp.units.values().flatten())
                    .filter(|(k, _)| {
                        let k = resolved_name(k).to_ascii_uppercase();
                        prefixes.iter().any(|p| k.starts_with(p))
                    })
                    .filter_map(|(_, t)| match t {
                        PinTarget::Net(n) => Some(n.clone()),
                        PinTarget::NoConnect => None,
                    })
                    .collect();
                nets.sort();
                nets.dedup();
                nets
            };
            let vdd = rail(&["VDD", "VCC"]);
            let gnd = rail(&["VSS", "GND"]);
            if vdd.len() != 1 || gnd.len() != 1 {
                let mut diag = Diagnostic::error(
                    "decouple-ambiguous",
                    format!(
                        "{refdes}: decouple needs exactly one VDD*/VCC* net and one \
                         VSS*/GND* net (found {vdd:?} / {gnd:?}) — write the caps explicitly"
                    ),
                );
                if let Some(span) = sc.span {
                    diag = diag.with_span(span);
                }
                diags.push(diag);
                continue;
            }
            let (vdd, gnd) = (vdd[0].clone(), gnd[0].clone());
            let mut idx = 0u32;
            let mut synths = Vec::new();
            // Assign `Origin::Synthesized { index }` in the SAME order `canon`
            // re-sugars decouple — by value string — so `compile(canon(d)) == d`
            // holds even when the author lists values out of sorted order.
            let mut entries: Vec<(&String, &u32)> = sc.decouple.iter().collect();
            entries.sort_by(|(a, _), (b, _)| a.cmp(b));
            for (value, count) in entries {
                for _ in 0..*count {
                    idx += 1;
                    let mut c = Component {
                        part: "Device:C".into(),
                        value: Some(value.clone()),
                        origin: Origin::Synthesized {
                            parent: refdes.clone(),
                            role: "decouple".into(),
                            index: idx,
                        },
                        ..Default::default()
                    };
                    c.pins.insert("1".into(), PinTarget::Net(vdd.clone()));
                    c.pins.insert("2".into(), PinTarget::Net(gnd.clone()));
                    synths.push((format!("__dec_{refdes}_{idx}"), c));
                }
            }
            let block = d.blocks.get_mut(bname).unwrap();
            for (key, c) in synths {
                block.components.insert(key, c);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_str;
    use crate::provider::MockSymbolProvider;

    pub(crate) fn run(src: &str) -> (crate::model::Design, crate::diag::Diagnostics) {
        let (s, mut diags) = parse_str(src);
        let (d, ds) = desugar(
            &s.expect("parse failed"),
            &MockSymbolProvider::with_basics(),
        );
        diags.extend(ds);
        (d, diags)
    }

    #[test]
    fn aliases_rails_and_nc() {
        let (d, diags) = run("
version: 1
rails: [3V3, GND]
blocks:
  main:
    components:
      R1: {part: R, pins: {1: 3V3, 2: OUT}}
      U1: {part: M:X, pins: {EN: nc}}
");
        assert!(!diags.has_errors(), "{:?}", diags);
        assert!(d.nets["3V3"].power);
        assert!(d.nets["GND"].power);
        let main = &d.blocks["main"];
        assert_eq!(main.components["R1"].part, "Device:R");
        assert_eq!(main.components["U1"].part, "M:X"); // full lib_id passthrough
        assert_eq!(
            main.components["R1"].pins["1"],
            PinTarget::Net("3V3".into())
        );
        assert_eq!(main.components["U1"].pins["EN"], PinTarget::NoConnect);
    }

    #[test]
    fn between_desugars_in_pin_number_order() {
        let (d, diags) = run("
version: 1
blocks:
  main:
    components:
      C1: {part: C, value: 10uF, between: [VBUS, GND]}
");
        assert!(!diags.has_errors(), "{:?}", diags);
        let c1 = &d.blocks["main"].components["C1"];
        assert_eq!(c1.pins["1"], PinTarget::Net("VBUS".into()));
        assert_eq!(c1.pins["2"], PinTarget::Net("GND".into()));
    }

    #[test]
    fn between_assigns_by_numeric_pin_order() {
        // symbol whose library lists pins out of numeric order: index0=number "2", index1=number "1"
        use crate::provider::{MockSymbolProvider, PinType};
        let mut p = MockSymbolProvider::with_basics();
        p.add(
            "My:Weird",
            vec![
                ("2", "~", PinType::Passive, 1),
                ("1", "~", PinType::Passive, 1),
            ],
        );
        let (s, _) = crate::parse::parse_str(
            "
version: 1
blocks:
  main:
    components:
      X1: {part: My:Weird, between: [AAA, BBB]}
",
        );
        let (d, diags) = desugar(&s.unwrap(), &p);
        assert!(!diags.has_errors(), "{:?}", diags);
        let x1 = &d.blocks["main"].components["X1"];
        assert_eq!(x1.pins["1"], crate::model::PinTarget::Net("AAA".into())); // a -> lowest pin number
        assert_eq!(x1.pins["2"], crate::model::PinTarget::Net("BBB".into()));
    }

    #[test]
    fn between_on_polarized_part_warns() {
        let (_, diags) = run("
version: 1
blocks:
  main:
    components:
      D1: {part: LED, between: [STATUS, GND]}
");
        assert!(!diags.has_errors());
        assert!(diags.0.iter().any(|d| d.code == "between-polarized"));
    }

    #[test]
    fn between_on_unknown_or_non_2pin_symbol_errors() {
        let (_, diags) = run("
version: 1
blocks:
  main:
    components:
      X1: {part: Nope:Nada, between: [A, B]}
");
        assert!(diags.0.iter().any(|d| d.code == "between-unknown-symbol"));
    }

    #[test]
    fn pin_ref_joins_existing_net() {
        let (d, diags) = run("
version: 1
blocks:
  main:
    components:
      U1: {part: M:X, pins: {PB6: I2C_SCL}}
      J2: {part: M:Conn, pins: {3: U1.PB6}}
");
        assert!(!diags.has_errors(), "{:?}", diags);
        assert_eq!(
            d.blocks["main"].components["J2"].pins["3"],
            PinTarget::Net("I2C_SCL".into())
        );
    }

    #[test]
    fn pin_ref_to_unmapped_pin_synthesizes_net_on_both_sides() {
        let (d, diags) = run("
version: 1
blocks:
  main:
    components:
      J1: {part: M:Usb, pins: {VBUS: VBUS}}
      R1: {part: R, between: [J1.CC1, GND]}
");
        assert!(!diags.has_errors(), "{:?}", diags);
        let main = &d.blocks["main"];
        assert_eq!(
            main.components["R1"].pins["1"],
            PinTarget::Net("N_J1_CC1".into())
        );
        assert_eq!(
            main.components["J1"].pins["CC1"],
            PinTarget::Net("N_J1_CC1".into())
        );
    }

    #[test]
    fn pin_ref_to_nc_pin_errors() {
        // Referring to a pin its owner declared `nc` is electrically
        // contradictory; it must be diagnosed, not silently split into a
        // dangling single-pin net (regression: the two sides used to disagree).
        let (_, diags) = run("
version: 1
blocks:
  main:
    components:
      U1: {part: M:X, pins: {PB6: nc}}
      J2: {part: M:Conn, pins: {3: U1.PB6}}
");
        assert!(
            diags.0.iter().any(|d| d.code == "pinref-to-nc"),
            "{diags:?}"
        );
    }

    #[test]
    fn pin_ref_to_unknown_component_errors() {
        let (_, diags) = run("
version: 1
blocks:
  main:
    components:
      R1: {part: R, pins: {1: U9.PA0, 2: GND}}
");
        assert!(diags.0.iter().any(|d| d.code == "bad-pin-ref"));
    }

    #[test]
    fn decouple_synthesizes_tagged_caps() {
        let (d, diags) = run("
version: 1
rails: [3V3, GND]
blocks:
  mcu:
    components:
      U1: {part: M:CPU, decouple: {100nF: 2, 4.7uF: 1},
           pins: {VDD: 3V3, VSS: GND}}
");
        assert!(!diags.has_errors(), "{:?}", diags);
        let mcu = &d.blocks["mcu"];
        let caps: Vec<_> = mcu
            .components
            .iter()
            .filter(|(_, c)| matches!(c.origin, crate::model::Origin::Synthesized { .. }))
            .collect();
        assert_eq!(caps.len(), 3);
        let (key, c) = &caps[0];
        assert_eq!(*key, "__dec_U1_1");
        assert_eq!(c.part, "Device:C");
        assert_eq!(c.value.as_deref(), Some("100nF"));
        assert_eq!(c.pins["1"], PinTarget::Net("3V3".into()));
        assert_eq!(c.pins["2"], PinTarget::Net("GND".into()));
        assert_eq!(
            c.origin,
            crate::model::Origin::Synthesized {
                parent: "U1".into(),
                role: "decouple".into(),
                index: 1
            }
        );
    }

    #[test]
    fn comp_and_unit_same_pin_different_nets_is_hard_error() {
        let (_, diags) = run("
version: 1
blocks:
  main:
    components:
      U1:
        part: M:Op
        pins: {1: NET_A}
        units:
          A: {pins: {1: NET_B}}
");
        assert!(
            diags.0.iter().any(|d| d.code == "pin-conflict"),
            "comp+unit duplicate pin must hard-error, got {:?}",
            diags
        );
    }

    #[test]
    fn duplicate_refdes_across_blocks_errors() {
        let (_, diags) = run("
version: 1
blocks:
  a: {components: {R1: {part: R, pins: {1: NA1, 2: NA2}}}}
  b: {components: {R1: {part: R, pins: {1: NB1, 2: NB2}}}}
");
        assert!(diags.0.iter().any(|d| d.code == "duplicate-refdes"));
    }

    #[test]
    fn net_name_for_joined_named_groups_is_deterministic() {
        // run many times; the conflict-resolved winner must be stable (lexicographically smallest)
        for _ in 0..50 {
            let (d, _) = run("
version: 1
blocks:
  main:
    components:
      U1: {part: M:X, pins: {1: NET_A}}
      J2: {part: M:Y, pins: {1: NET_B, 2: U1.1}}
");
            // U1.1 and J2.2 are joined; both groups named -> deterministic winner NET_A (smallest)
            assert_eq!(
                d.blocks["main"].components["J2"].pins["2"],
                crate::model::PinTarget::Net("NET_A".into())
            );
        }
    }

    #[test]
    fn generated_unnamed_net_names_are_unique() {
        let (d, _) = run("
version: 1
blocks:
  main:
    components:
      U1: {part: M:X, pins: {VDD: 3V3}}
      Z8: {part: M:Y, pins: {1: U1.A_B}}
      Z9: {part: M:Y, pins: {1: U1.A.B}}
");
        let n8 = &d.blocks["main"].components["Z8"].pins["1"];
        let n9 = &d.blocks["main"].components["Z9"].pins["1"];
        assert_ne!(
            n8, n9,
            "distinct unnamed nets must get distinct generated names"
        );
    }

    #[test]
    fn decouple_resolves_power_pins_by_number() {
        use crate::provider::{MockSymbolProvider, PinType};
        let mut p = MockSymbolProvider::with_basics();
        p.add(
            "M:CPU",
            vec![
                ("1", "VDD", PinType::PowerInput, 1),
                ("2", "VSS", PinType::PowerInput, 1),
            ],
        );
        let (s, _) = crate::parse::parse_str(
            "
version: 1
rails: [3V3, GND]
blocks:
  mcu:
    components:
      U1: {part: M:CPU, decouple: {100nF: 1}, pins: {1: 3V3, 2: GND}}
",
        );
        let (d, diags) = desugar(&s.unwrap(), &p);
        assert!(
            !diags.0.iter().any(|x| x.code == "decouple-ambiguous"),
            "{:?}",
            diags
        );
        let caps = d.blocks["mcu"]
            .components
            .values()
            .filter(|c| matches!(c.origin, crate::model::Origin::Synthesized { .. }))
            .count();
        assert_eq!(caps, 1);
    }

    #[test]
    fn unmentioned_non_power_pins_become_no_connect() {
        use crate::provider::{MockSymbolProvider, PinType};
        let mut p = MockSymbolProvider::with_basics();
        p.add(
            "M:Chip",
            vec![
                ("1", "PA0", PinType::Other, 1),
                ("2", "PB6", PinType::Other, 1),
            ],
        );
        let (s, _) = crate::parse::parse_str(
            "
version: 1
blocks:
  main:
    components:
      U1: {part: M:Chip, pins: {PA0: SIG}}
",
        );
        let (d, diags) = desugar(&s.unwrap(), &p);
        assert!(!diags.has_errors(), "{:?}", diags);
        let u1 = &d.blocks["main"].components["U1"];
        // unmentioned non-power pin PB6 (number "2") is auto-NC
        assert_eq!(u1.pins["2"], crate::model::PinTarget::NoConnect);
        // mentioned pin still on its net
        assert_eq!(u1.pins["PA0"], crate::model::PinTarget::Net("SIG".into()));
    }

    #[test]
    fn auto_nc_is_idempotent_through_canon() {
        use crate::provider::{MockSymbolProvider, PinType};
        let mut p = MockSymbolProvider::with_basics();
        p.add(
            "M:Chip",
            vec![
                ("1", "PA0", PinType::Other, 1),
                ("2", "PB6", PinType::Other, 1),
            ],
        );
        let (s1, _) = crate::parse::parse_str(
            "version: 1\nblocks: {main: {components: {U1: {part: M:Chip, pins: {PA0: SIG}}}}}",
        );
        let (d1, _) = desugar(&s1.unwrap(), &p);
        let out1 = crate::canon::to_canonical_yaml(&d1);
        let (s2, _) = crate::parse::parse_str(&out1);
        let (d2, _) = desugar(&s2.unwrap(), &p);
        assert_eq!(d1, d2);
        assert_eq!(out1, crate::canon::to_canonical_yaml(&d2));
    }

    #[test]
    fn decouple_with_ambiguous_rails_errors() {
        let (_, diags) = run("
version: 1
blocks:
  mcu:
    components:
      U1: {part: M:CPU, decouple: {100nF: 1}, pins: {VDD: 3V3, VDDA: AVDD, VSS: GND}}
");
        assert!(diags.0.iter().any(|d| d.code == "decouple-ambiguous"));
    }
}
