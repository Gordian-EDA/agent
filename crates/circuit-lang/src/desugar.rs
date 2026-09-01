//! Sugar -> kernel lowering. The reconciler and lints see
//! only the output of this pass.

use crate::surface::*;
use indexmap::IndexMap;
use sch_check::SymbolTable;
use sch_check::diag::{Diagnostic, Diagnostics};
use sch_check::model::*;

/// Closed alias table for terse built-ins plus one common KiCad library slip.
fn alias(part: &str) -> String {
    match part {
        "R" => "Device:R".into(),
        "C" => "Device:C".into(),
        "L" => "Device:L".into(),
        "D" => "Device:D".into(),
        "LED" => "Device:LED".into(),
        // KiCad's pushbutton symbol lives in Switch, not Device. This exact
        // mistaken library-qualified spelling is common and unambiguous.
        "Device:SW_Push" => "Switch:SW_Push".into(),
        other => other.into(),
    }
}

/// Recover the schema-level slip `pins: {positive: ..., negative: ...}` for the
/// two standard polarized aliases. The net-to-polarity meaning is explicit, so
/// this does not guess orientation or mask a genuine electrical reversal.
fn lift_misplaced_polarity_fields(sc: &mut SurfaceComponent) {
    let part = alias(&sc.part);
    if !matches!(part.as_str(), "Device:D" | "Device:LED")
        || sc.between.is_some()
        || sc.positive.is_some()
        || sc.negative.is_some()
        || sc.pins.len() != 2
    {
        return;
    }
    let Some(positive) = sc.pins.shift_remove("positive") else {
        return;
    };
    let Some(negative) = sc.pins.shift_remove("negative") else {
        sc.pins.insert("positive".into(), positive);
        return;
    };
    sc.positive = Some(positive);
    sc.negative = Some(negative);
}

pub fn desugar(s: &SurfaceDesign, provider: &SymbolTable) -> (Design, Diagnostics) {
    let mut diags = Diagnostics::default();
    let mut d = Design {
        name: s.name.clone(),
        description: s.description.clone(),
        lint_allow: s.lint_allow.iter().cloned().collect(),
        ..Default::default()
    };

    // Power nets can be declared with `class: power` and are also DERIVED from
    // power-symbol COMPONENTS the author places (a part in KiCAD's `power:`
    // library, e.g. `power:GND`). The net each such symbol drives is a power net.
    // Symbol-derived nets are marked after `resolve_pins` below, once the symbols'
    // pins resolve to nets.
    for (net, attrs) in &s.nets {
        d.nets.entry(net.clone()).or_default().class = attrs.class.clone();
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
            layout: Vec::new(),
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
            lift_misplaced_polarity_fields(&mut sc);
            apply_two_pin(refdes, &mut sc, provider, &mut diags); // Task 7
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
        // Per-block `layout:` grid → kernel, validating each cell names one of
        // THIS block's refdes (a module's grid arranges only its own parts).
        block.layout = lower_block_layout(sb, &mut diags);
        d.blocks.insert(bname.clone(), block);
    }

    resolve_pins(&mut d, raw_pins, &mut diags);
    synth_decouple(&mut d, s, provider, &mut diags);
    sch_check::decouple::renumber(&mut d);
    materialize_auto_nc(&mut d, provider); // Task R6
    sch_check::nets::derive_attrs(&mut d);

    (d, diags)
}

/// Strip spans off one block's `layout:` grid into the kernel model, validating
/// that every named cell is a refdes declared in THAT block. A refdes may repeat
/// (a column span / float); `~` is a hole (`None`).
fn lower_block_layout(sb: &SurfaceBlock, diags: &mut Diagnostics) -> LayoutGrid {
    let refdes: std::collections::HashSet<&str> =
        sb.components.keys().map(String::as_str).collect();
    sb.layout
        .iter()
        .map(|row| {
            row.iter()
                .map(|(cell, span)| {
                    if let Some(name) = cell
                        && !refdes.contains(name.as_str())
                    {
                        diags.push(
                            Diagnostic::error(
                                "unknown-layout-cell",
                                format!("layout cell `{name}` is not a refdes in this block"),
                            )
                            .with_span(*span),
                        );
                    }
                    cell.clone()
                })
                .collect()
        })
        .collect()
}

/// Final desugar pass: for every component whose symbol is known, any physical
/// pin not covered by an author key and whose `etype` is not `PowerInput`
/// becomes an explicit `nc`. A net-mapped pin, an explicit `nc`,
/// or a stacked name covering the pin all count as coverage; power-input pins
/// are skipped (lint.rs already errors when they are left unconnected).
/// Markers are keyed by pin number and inserted in symbol pin order, so the
/// pass is deterministic and idempotent across a canonical round-trip.
fn materialize_auto_nc(d: &mut Design, provider: &SymbolTable) {
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
                .filter(|p| p.etype != sch_check::PinType::PowerInput)
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
    span: sch_check::diag::Span,
}

/// Lower the 2-pin connection sugars to a pin map:
/// - `between: [a, b]` — SYMMETRIC parts; maps by numeric pin number.
/// - `positive:`/`negative:` — POLARIZED parts; maps to the anode (`A`/`+`) and
///   cathode (`K`/`-`) pins.
///
/// Enforces the symmetric/polarized split: `between` on a polarized part and
/// `positive`/`negative` on a symmetric part are both hard errors (no silent
/// wrong-way-round diodes).
fn apply_two_pin(
    refdes: &str,
    sc: &mut SurfaceComponent,
    provider: &SymbolTable,
    diags: &mut Diagnostics,
) {
    let has_between = sc.between.is_some();
    let has_pol = sc.positive.is_some() || sc.negative.is_some();
    if !has_between && !has_pol {
        return;
    }
    // A span pointing at whichever sugar is present, for diagnostics.
    let span = sc
        .between
        .as_ref()
        .map(|((_, s), _)| *s)
        .or_else(|| sc.positive.as_ref().map(|(_, s)| *s))
        .or_else(|| sc.negative.as_ref().map(|(_, s)| *s))
        .expect("a 2-pin sugar is present");

    let part = alias(&sc.part);
    let Some(meta) = provider.symbol(&part) else {
        let mut d = Diagnostic::error(
            "between-unknown-symbol",
            format!("{refdes}: cannot desugar a 2-pin connection — unknown symbol `{part}`"),
        )
        .with_span(span);
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
                    "{refdes}: a 2-pin connection needs a 2-pin symbol; `{part}` has {} pins",
                    meta.pins.len()
                ),
            )
            .with_span(span),
        );
        return;
    }
    if has_between && has_pol {
        diags.push(
            Diagnostic::error(
                "two-pin-conflict",
                format!("{refdes}: use either `between` or `positive`/`negative`, not both"),
            )
            .with_span(span),
        );
        return;
    }

    // Polarity from the symbol: anode `A`/`+`, cathode `K`/`-`.
    let anode = meta.pins.iter().find(|p| p.name == "A" || p.name == "+");
    let cathode = meta.pins.iter().find(|p| p.name == "K" || p.name == "-");
    let polarized = matches!(part.as_str(), "Device:D" | "Device:LED" | "Device:CP")
        || (anode.is_some() && cathode.is_some());

    // The two (pin number, net, span) bindings to write.
    let bindings: [(String, String, sch_check::diag::Span); 2] = if has_pol {
        if !polarized {
            diags.push(
                Diagnostic::error(
                    "polarity-on-symmetric",
                    format!("{refdes}: `{part}` is not polarized — use `between`"),
                )
                .with_span(span),
            );
            return;
        }
        let (Some(a), Some(k)) = (anode, cathode) else {
            diags.push(
                Diagnostic::error(
                    "polarity-unknown-pins",
                    format!(
                        "{refdes}: `{part}` is polarized but its anode/cathode pins \
                         can't be identified — write explicit `pins:`"
                    ),
                )
                .with_span(span),
            );
            return;
        };
        let (Some((pnet, pspan)), Some((nnet, nspan))) = (sc.positive.take(), sc.negative.take())
        else {
            diags.push(
                Diagnostic::error(
                    "polarity-incomplete",
                    format!("{refdes}: a polarized part needs both `positive:` and `negative:`"),
                )
                .with_span(span),
            );
            return;
        };
        [
            (a.number.clone(), pnet, pspan),
            (k.number.clone(), nnet, nspan),
        ]
    } else {
        if polarized {
            diags.push(
                Diagnostic::error(
                    "between-on-polarized",
                    format!(
                        "{refdes}: `{part}` is polarized — use `positive`/`negative` (not `between`)"
                    ),
                )
                .with_span(span),
            );
            return;
        }
        let ((a, aspan), (b, bspan)) = sc.between.take().unwrap();
        // Map `between` args by numeric pin NUMBER, not library order: first arg →
        // lowest-numbered pin, second arg → highest. Fall back to
        // string order for non-numeric pin numbers.
        let mut ordered: Vec<&sch_check::PinMeta> = meta.pins.iter().collect();
        ordered.sort_by(
            |x, y| match (x.number.parse::<u64>(), y.number.parse::<u64>()) {
                (Ok(nx), Ok(ny)) => nx.cmp(&ny),
                _ => x.number.cmp(&y.number),
            },
        );
        [
            (ordered[0].number.clone(), a, aspan),
            (ordered[1].number.clone(), b, bspan),
        ]
    };

    for (pin, target, span) in bindings {
        if sc.pins.insert(pin.clone(), (target, span)).is_some() {
            diags.push(
                Diagnostic::error(
                    "pin-conflict",
                    format!("{refdes}: pin `{pin}` set by both a 2-pin sugar and `pins`"),
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

/// Union-find over pin nodes keyed by `(refdes, pin)`. `make` interns a node;
/// the disjoint-set core runs over the interned `parent` slice. Union keeps the
/// second node's root as the survivor.
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
    fn find(&mut self, i: usize) -> usize {
        geom::ParentForest::new(&mut self.parent).find(i)
    }
    fn union(&mut self, i: usize, j: usize) {
        geom::ParentForest::new(&mut self.parent).union_to(i, j);
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
        // pin-ref? "<REFDES>.<pin>" where REFDES is refdes-shaped and exists.
        // A dotted target whose left side is not a refdes (e.g. a net literally
        // named `3.3V`) is a plain net name, not a pin-ref.
        let is_ref = rp
            .target
            .split_once('.')
            .is_some_and(|(r, _)| crate::parse::looks_like_refdes(r) && comp_block.contains_key(r));
        let dotted_pinref = rp
            .target
            .split_once('.')
            .is_some_and(|(r, _)| crate::parse::looks_like_refdes(r));
        if dotted_pinref && !is_ref {
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
    let node_span: std::collections::HashMap<usize, sch_check::diag::Span> =
        placement.iter().map(|(rp, i)| (*i, rp.span)).collect();
    // Collect all author names per group root, deterministically. HashMap
    // iteration order is randomized, so accumulate into a sorted set per root
    // and pick the lexicographically smallest name as the group's winner;
    // emit one `net-conflict` per distinct extra name (names listed sorted).
    // `name -> span` lets the conflict diag point at a node naming the loser.
    let mut group_names: std::collections::HashMap<usize, std::collections::BTreeSet<String>> =
        std::collections::HashMap::new();
    let mut name_span: std::collections::HashMap<(usize, String), sch_check::diag::Span> =
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

/// Expand each component's `decouple:` sugar into real caps
/// ([`sch_check::decouple`]), carrying the surface span onto an
/// ambiguous-rails error.
fn synth_decouple(
    d: &mut Design,
    s: &SurfaceDesign,
    provider: &SymbolTable,
    diags: &mut Diagnostics,
) {
    for (bname, sb) in &s.blocks {
        for (refdes, sc) in &sb.components {
            if sc.decouple.is_empty() {
                continue;
            }
            let comp = &d.blocks[bname].components[refdes];
            let rails = match sch_check::decouple::rails(refdes, comp, provider) {
                Ok(rails) => rails,
                Err(mut diag) => {
                    if let Some(span) = sc.span {
                        diag = diag.with_span(span);
                    }
                    diags.push(diag);
                    continue;
                }
            };
            let block = d.blocks.get_mut(bname).unwrap();
            for (key, cap) in sch_check::decouple::expand(refdes, &sc.decouple, &rails) {
                block.components.insert(key, cap);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_str;
    use sch_check::SymbolTable;

    pub(crate) fn run(src: &str) -> (sch_check::model::Design, sch_check::diag::Diagnostics) {
        let (s, mut diags) = parse_str(src);
        let (d, ds) = desugar(&s.expect("parse failed"), &SymbolTable::with_basics());
        diags.extend(ds);
        (d, diags)
    }

    #[test]
    fn label_global_component_marks_a_port_net() {
        // A `label:global` component on a DEGREE-2 net (the divider tap OUT) marks it
        // a board I/O port — the degree-1 heuristic alone could never see it.
        let (d, diags) = run("
version: 1
blocks:
  io:
    components:
      LBL1: {part: label:global, pins: {1: OUT}}
  main:
    components:
      R1: {part: R, value: 10k, between: [VCC, OUT]}
      R2: {part: R, value: 10k, between: [OUT, MID]}
");
        assert!(!diags.has_errors(), "{:?}", diags);
        assert!(d.nets["OUT"].port, "label:global marks OUT a port");
        // An unlabelled internal node (MID, degree-2) is NOT a port.
        assert!(!d.nets.get("MID").map(|a| a.port).unwrap_or(false));
        // The label component round-trips as an ordinary authored component.
        assert_eq!(d.blocks["io"].components["LBL1"].part, "label:global");
        // Re-derive on the canonical round-trip: port flag is recomputed, not stored.
        let canon = crate::canon::to_canonical_yaml(&d);
        assert!(
            canon.contains("label:global"),
            "label component survives canon"
        );
        let (d2, _) = run(&canon);
        assert!(
            d2.nets["OUT"].port,
            "port re-derived after canon round-trip"
        );
    }

    #[test]
    fn aliases_rails_and_nc() {
        let (d, diags) = run("
version: 1
blocks:
  rails:
    components:
      PWR1: {part: power:VCC, pins: {1: 3V3}}
      PWR2: {part: power:GND, pins: {1: GND}}
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
    fn common_pushbutton_library_slip_is_canonicalized() {
        assert_eq!(alias("Device:SW_Push"), "Switch:SW_Push");
        assert_eq!(alias("Device:SW_SPST"), "Device:SW_SPST");
    }

    #[test]
    fn class_power_marks_net_as_power_rail() {
        let (d, diags) = run("
version: 1
blocks:
  main:
    components:
      R1: {part: R, between: [VBUS_FUSED, GND]}
nets:
  VBUS_FUSED: {class: power}
  GND: {class: power}
");
        assert!(!diags.has_errors(), "{:?}", diags);
        assert!(d.nets["VBUS_FUSED"].power);
        assert!(d.nets["GND"].power);
    }

    #[test]
    fn dotted_net_name_is_not_a_pin_ref() {
        // A net literally named `3.3V` has a non-refdes left segment (`3`), so it
        // is a plain net name, not a `U1.5`-style pin-ref. It must compile and
        // survive a canonical round-trip.
        let src = "
version: 1
blocks:
  main:
    components:
      R1: {part: R, value: 10k, between: ['3.3V', OUT]}
      R2: {part: R, value: 20k, between: [OUT, GND]}
";
        let (d, diags) = run(src);
        assert!(!diags.has_errors(), "{:?}", diags);
        assert_eq!(
            d.blocks["main"].components["R1"].pins["1"],
            PinTarget::Net("3.3V".into())
        );
        let canon = crate::canon::to_canonical_yaml(&d);
        let (d2, diags2) = run(&canon);
        assert!(!diags2.has_errors(), "{:?}", diags2);
        assert_eq!(
            d2.blocks["main"].components["R1"].pins["1"],
            PinTarget::Net("3.3V".into())
        );
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
        use sch_check::{PinType, SymbolTable};
        let mut p = SymbolTable::with_basics();
        p.mock_add(
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
        assert_eq!(x1.pins["1"], sch_check::model::PinTarget::Net("AAA".into())); // a -> lowest pin number
        assert_eq!(x1.pins["2"], sch_check::model::PinTarget::Net("BBB".into()));
    }

    #[test]
    fn between_on_polarized_part_errors() {
        let (_, diags) = run("
version: 1
blocks:
  main:
    components:
      D1: {part: LED, between: [STATUS, GND]}
");
        assert!(diags.0.iter().any(|d| d.code == "between-on-polarized"));
    }

    #[test]
    fn positive_negative_maps_anode_cathode() {
        // Device:LED has pin 1 = K (cathode), pin 2 = A (anode).
        let (d, diags) = run("
version: 1
blocks:
  main:
    components:
      D1: {part: LED, positive: VPLUS, negative: SIG}
");
        assert!(!diags.has_errors(), "{diags:?}");
        let d1 = &d.blocks["main"].components["D1"];
        assert_eq!(d1.pins["2"], PinTarget::Net("VPLUS".into())); // anode A = pin 2
        assert_eq!(d1.pins["1"], PinTarget::Net("SIG".into())); // cathode K = pin 1
    }

    #[test]
    fn misplaced_polarity_pin_fields_are_lifted_without_guessing_orientation() {
        // This exact shape appeared in a live draft. `positive` still means
        // anode and `negative` cathode; only their nesting was wrong.
        let (d, diags) = run("
version: 1
blocks:
  main:
    components:
      D1: {part: Device:LED, pins: {positive: STATUS, negative: GND}}
");
        assert!(!diags.has_errors(), "{diags:?}");
        let led = &d.blocks["main"].components["D1"];
        assert_eq!(led.pins["2"], PinTarget::Net("STATUS".into()));
        assert_eq!(led.pins["1"], PinTarget::Net("GND".into()));
        assert!(!led.pins.contains_key("positive"));
        assert!(!led.pins.contains_key("negative"));
    }

    #[test]
    fn polarity_lift_refuses_partial_or_conflicting_shapes() {
        let (mut partial, _) = crate::parse::parse_str(
            "
version: 1
blocks: {main: {components: {D1: {part: Device:LED, pins: {positive: STATUS, 1: GND}}}}}
",
        );
        let partial = partial
            .as_mut()
            .unwrap()
            .blocks
            .get_mut("main")
            .unwrap()
            .components
            .get_mut("D1")
            .unwrap();
        let before = partial.clone();
        lift_misplaced_polarity_fields(partial);
        assert_eq!(*partial, before);

        let (mut explicit, _) = crate::parse::parse_str("
version: 1
blocks: {main: {components: {D1: {part: Device:LED, positive: STATUS, negative: GND, pins: {1: OTHER}}}}}
");
        let explicit = explicit
            .as_mut()
            .unwrap()
            .blocks
            .get_mut("main")
            .unwrap()
            .components
            .get_mut("D1")
            .unwrap();
        let before = explicit.clone();
        lift_misplaced_polarity_fields(explicit);
        assert_eq!(*explicit, before);
    }

    #[test]
    fn positive_negative_on_symmetric_part_errors() {
        let (_, diags) = run("
version: 1
blocks:
  main:
    components:
      R1: {part: R, positive: A, negative: B}
");
        assert!(diags.0.iter().any(|d| d.code == "polarity-on-symmetric"));
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
            .filter(|(_, c)| matches!(c.origin, sch_check::model::Origin::Synthesized { .. }))
            .collect();
        assert_eq!(caps.len(), 3);
        let (key, c) = &caps[0];
        // Synth decouple caps are re-annotated to a real C<n> refdes (no authored
        // C here, so the first is C1) — the `__dec_` key never reaches the sheet.
        assert_eq!(*key, "C1");
        assert_eq!(c.part, "Device:C");
        assert_eq!(c.value.as_deref(), Some("100nF"));
        assert_eq!(c.pins["1"], PinTarget::Net("3V3".into()));
        assert_eq!(c.pins["2"], PinTarget::Net("GND".into()));
        assert_eq!(
            c.origin,
            sch_check::model::Origin::Synthesized {
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
                sch_check::model::PinTarget::Net("NET_A".into())
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
        use sch_check::{PinType, SymbolTable};
        let mut p = SymbolTable::with_basics();
        p.mock_add(
            "M:CPU",
            vec![
                ("1", "VDD", PinType::PowerInput, 1),
                ("2", "VSS", PinType::PowerInput, 1),
            ],
        );
        let (s, _) = crate::parse::parse_str(
            "
version: 1
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
            .filter(|c| matches!(c.origin, sch_check::model::Origin::Synthesized { .. }))
            .count();
        assert_eq!(caps, 1);
    }

    #[test]
    fn unmentioned_non_power_pins_become_no_connect() {
        use sch_check::{PinType, SymbolTable};
        let mut p = SymbolTable::with_basics();
        p.mock_add(
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
        assert_eq!(u1.pins["2"], sch_check::model::PinTarget::NoConnect);
        // mentioned pin still on its net
        assert_eq!(
            u1.pins["PA0"],
            sch_check::model::PinTarget::Net("SIG".into())
        );
    }

    #[test]
    fn auto_nc_is_idempotent_through_canon() {
        use sch_check::{PinType, SymbolTable};
        let mut p = SymbolTable::with_basics();
        p.mock_add(
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
