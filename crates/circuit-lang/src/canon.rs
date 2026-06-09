//! Deterministic canonical YAML emission of the kernel model.

use crate::model::*;
use indexmap::IndexMap;
use std::fmt::Write;

/// Quote a YAML scalar only when needed.
fn q(s: &str) -> String {
    let safe = !s.is_empty()
        && s.chars().next().unwrap().is_ascii_alphanumeric()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_.+:~/-".contains(c));
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "''"))
    }
}

/// Natural sort: alpha prefix, then numeric suffix (U2 < U10).
pub fn natural_lt(a: &str, b: &str) -> bool {
    fn split(s: &str) -> (&str, u64) {
        let i = s.find(|c: char| c.is_ascii_digit()).unwrap_or(s.len());
        (&s[..i], s[i..].parse().unwrap_or(0))
    }
    split(a) < split(b)
}

fn sorted<'a, V>(m: &'a IndexMap<String, V>) -> Vec<(&'a String, &'a V)> {
    let mut v: Vec<_> = m.iter().collect();
    v.sort_by(|(a, _), (b, _)| {
        if natural_lt(a, b) {
            std::cmp::Ordering::Less
        } else if natural_lt(b, a) {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    });
    v
}

fn pin_map_inline(pins: &IndexMap<String, PinTarget>) -> String {
    let parts: Vec<String> = sorted(pins)
        .into_iter()
        .map(|(k, t)| match t {
            PinTarget::Net(n) => format!("{}: {}", q(k), q(n)),
            PinTarget::NoConnect => format!("{}: nc", q(k)),
        })
        .collect();
    format!("{{{}}}", parts.join(", "))
}

pub fn to_canonical_yaml(d: &Design) -> String {
    let mut o = String::new();
    o.push_str("version: 1\n");
    if let Some(n) = &d.name {
        writeln!(o, "name: {}", q(n)).unwrap();
    }
    if let Some(desc) = &d.description {
        writeln!(o, "description: {}", q(desc)).unwrap();
    }
    o.push_str("blocks:\n");
    for (bname, block) in &d.blocks {
        writeln!(o, "  {}:", q(bname)).unwrap();
        if let Some(note) = &block.note {
            writeln!(o, "    note: {}", q(note)).unwrap();
        }
        let mut hints = Vec::new();
        if let Some(e) = block.layout.edge {
            hints.push(format!(
                "edge: {}",
                match e {
                    Edge::Left => "left",
                    Edge::Right => "right",
                    Edge::Top => "top",
                    Edge::Bottom => "bottom",
                }
            ));
        }
        if let Some(nb) = &block.layout.near {
            hints.push(format!("near: {}", q(nb)));
        }
        if !hints.is_empty() {
            writeln!(o, "    layout: {{{}}}", hints.join(", ")).unwrap();
        }
        o.push_str("    components:\n");

        // Re-sugar: collect decouple synths per parent (value -> count).
        let mut decouple: IndexMap<&str, IndexMap<&str, u32>> = IndexMap::new();
        for (_, c) in block.components.iter() {
            if let Origin::Synthesized { parent, role, .. } = &c.origin {
                if role == "decouple" {
                    *decouple
                        .entry(parent.as_str())
                        .or_default()
                        .entry(c.value.as_deref().unwrap_or("?"))
                        .or_default() += 1;
                }
            }
        }

        for (refdes, c) in sorted(&block.components) {
            if matches!(c.origin, Origin::Synthesized { .. }) {
                continue; // re-sugared onto parent
            }
            let mut fields = vec![format!("part: {}", q(&c.part))];
            if let Some(v) = &c.value {
                fields.push(format!("value: {}", q(v)));
            }
            if let Some(fpr) = &c.footprint {
                fields.push(format!("footprint: {}", q(fpr)));
            }
            if c.dnp {
                fields.push("dnp: true".into());
            }
            if !c.props.is_empty() {
                let ps: Vec<String> = sorted(&c.props)
                    .into_iter()
                    .map(|(k, v)| format!("{}: {}", q(k), q(v)))
                    .collect();
                fields.push(format!("props: {{{}}}", ps.join(", ")));
            }
            if let Some(dec) = decouple.get(refdes.as_str()) {
                let ds: Vec<String> = dec
                    .iter()
                    .map(|(v, n)| format!("{}: {}", q(v), n))
                    .collect();
                fields.push(format!("decouple: {{{}}}", ds.join(", ")));
            }
            if !c.pins.is_empty() {
                fields.push(format!("pins: {}", pin_map_inline(&c.pins)));
            }
            if c.units.is_empty() {
                writeln!(o, "      {}: {{{}}}", q(refdes), fields.join(", ")).unwrap();
            } else {
                writeln!(o, "      {}:", q(refdes)).unwrap();
                for f in &fields {
                    let (k, v) = f.split_once(": ").unwrap();
                    writeln!(o, "        {k}: {v}").unwrap();
                }
                o.push_str("        units:\n");
                for (u, pins) in sorted(&c.units) {
                    writeln!(o, "          {}: {{pins: {}}}", q(u), pin_map_inline(pins)).unwrap();
                }
            }
        }
    }
    if !d.nets.is_empty() {
        let mut nets: Vec<_> = d.nets.iter().collect();
        nets.sort_by(|(a, _), (b, _)| a.cmp(b));
        let mut wrote_header = false;
        for (net, attrs) in nets {
            let mut fields = Vec::new();
            if attrs.power {
                fields.push("power: true".to_string());
            }
            if let Some(c) = &attrs.class {
                fields.push(format!("class: {}", q(c)));
            }
            if fields.is_empty() {
                continue; // nets exist by reference; attribute-free entries add nothing
            }
            if !wrote_header {
                o.push_str("nets:\n");
                wrote_header = true;
            }
            writeln!(o, "  {}: {{{}}}", q(net), fields.join(", ")).unwrap();
        }
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desugar::desugar;
    use crate::parse::parse_str;
    use crate::provider::MockSymbolProvider;

    const SRC: &str = "
version: 1
name: t
rails: [3V3, GND]
blocks:
  mcu:
    layout: {edge: right}
    components:
      U1: {part: M:CPU, decouple: {100nF: 2}, pins: {VDD: 3V3, VSS: GND, PB6: SCL}}
      R7: {part: R, value: 4.7k, between: [SCL, 3V3]}
";

    fn compile(src: &str) -> crate::model::Design {
        let p = MockSymbolProvider::with_basics();
        let (s, diags) = parse_str(src);
        assert!(!diags.has_errors(), "{diags:?}");
        // M:CPU unknown to provider — between only needs Device:R; ok here
        let (d, ds) = desugar(&s.unwrap(), &p);
        assert!(!ds.has_errors(), "{ds:?}");
        d
    }

    #[test]
    fn canonical_resugars_decouple_and_is_idempotent() {
        let d1 = compile(SRC);
        let out1 = to_canonical_yaml(&d1);
        assert!(out1.contains("decouple: {100nF: 2}"));
        assert!(!out1.contains("__dec_"));
        let d2 = compile(&out1);
        assert_eq!(
            d1, d2,
            "canonical round-trip must preserve the kernel model"
        );
        assert_eq!(
            out1,
            to_canonical_yaml(&d2),
            "canonical emit must be a fixpoint"
        );
    }

    #[test]
    fn natural_refdes_ordering() {
        assert!(natural_lt("U2", "U10"));
        assert!(natural_lt("C9", "C12"));
        assert!(!natural_lt("R10", "R2"));
    }
}
