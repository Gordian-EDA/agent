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
    for (bname, sb) in &s.blocks {
        let mut block = Block {
            note: sb.note.clone(),
            layout: sb.layout.clone(),
            components: IndexMap::new(),
        };
        for (refdes, sc) in &sb.components {
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
    synth_decouple(&mut d, s, &mut diags); // Task 8

    (d, diags)
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
                    meta.pins[0].number, meta.pins[1].number, meta.pins[0].name, meta.pins[1].name
                ),
            )
            .with_span(aspan),
        );
    }
    for (pin, target, span) in [
        (meta.pins[0].number.clone(), a, aspan),
        (meta.pins[1].number.clone(), b, bspan),
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

fn resolve_pins(d: &mut Design, raw: Vec<RawPin>, diags: &mut Diagnostics) {
    // refdes -> block (for pin-ref targets and on-demand pin creation)
    let comp_block: std::collections::HashMap<String, String> = d
        .blocks
        .iter()
        .flat_map(|(b, bl)| bl.components.keys().map(move |r| (r.clone(), b.clone())))
        .collect();

    // Union-find over pin nodes keyed by (refdes, pin).
    let mut nodes: Vec<(String, String)> = Vec::new();
    let mut index: std::collections::HashMap<(String, String), usize> =
        std::collections::HashMap::new();
    let mut parent: Vec<usize> = Vec::new();
    let node = |r: &str,
                p: &str,
                nodes: &mut Vec<(String, String)>,
                index: &mut std::collections::HashMap<(String, String), usize>,
                parent: &mut Vec<usize>|
     -> usize {
        *index
            .entry((r.to_string(), p.to_string()))
            .or_insert_with(|| {
                nodes.push((r.to_string(), p.to_string()));
                parent.push(nodes.len() - 1);
                nodes.len() - 1
            })
    };
    fn find(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }

    let mut named: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
    let mut placement: Vec<(RawPin, usize)> = Vec::new(); // node idx per raw pin
    let mut extra_nodes: Vec<usize> = Vec::new(); // pin-ref targets (may be unmapped)

    for rp in raw {
        if rp.target.eq_ignore_ascii_case("nc") {
            write_pin(d, &rp, PinTarget::NoConnect);
            continue;
        }
        let i = node(&rp.refdes, &rp.pin, &mut nodes, &mut index, &mut parent);
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
            let j = node(tr, tp, &mut nodes, &mut index, &mut parent);
            let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
            parent[ri] = rj;
            extra_nodes.push(j);
        } else {
            let root = find(&mut parent, i);
            named.insert(root, rp.target.clone());
        }
        placement.push((rp, i));
    }

    // consolidate names after all unions
    let mut group_name: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
    for (i, name) in &named {
        let root = find(&mut parent, *i);
        if let Some(prev) = group_name.insert(root, name.clone())
            && &prev != name
        {
            diags.push(Diagnostic::error(
                "net-conflict",
                format!("nets `{prev}` and `{name}` joined by pin-refs"),
            ));
        }
    }
    // unnamed groups: N_<smallest member>
    for i in 0..nodes.len() {
        let root = find(&mut parent, i);
        group_name.entry(root).or_insert_with(|| {
            let mut members: Vec<String> = (0..nodes.len())
                .filter(|&j| find(&mut parent, j) == root)
                .map(|j| format!("{}_{}", nodes[j].0, sanitize(&nodes[j].1)))
                .collect();
            members.sort();
            format!("N_{}", members[0])
        });
    }

    for (rp, i) in placement {
        let root = find(&mut parent, i);
        write_pin(d, &rp, PinTarget::Net(group_name[&root].clone()));
    }
    // pin-ref targets that had no own mapping: create one on the component
    for j in extra_nodes {
        let (r, p) = nodes[j].clone();
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
            let root = find(&mut parent, j);
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

fn synth_decouple(d: &mut Design, s: &SurfaceDesign, diags: &mut Diagnostics) {
    for (bname, sb) in &s.blocks {
        for (refdes, sc) in &sb.components {
            if sc.decouple.is_empty() {
                continue;
            }
            let comp = &d.blocks[bname].components[refdes];
            let rail = |prefixes: &[&str]| -> Vec<NetName> {
                let mut nets: Vec<NetName> = comp
                    .pins
                    .iter()
                    .chain(comp.units.values().flatten())
                    .filter(|(k, _)| {
                        let k = k.to_ascii_uppercase();
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
                diags.push(Diagnostic::error(
                    "decouple-ambiguous",
                    format!(
                        "{refdes}: decouple needs exactly one VDD*/VCC* net and one \
                         VSS*/GND* net (found {vdd:?} / {gnd:?}) — write the caps explicitly"
                    ),
                ));
                continue;
            }
            let (vdd, gnd) = (vdd[0].clone(), gnd[0].clone());
            let mut idx = 0u32;
            let mut synths = Vec::new();
            for (value, count) in &sc.decouple {
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
