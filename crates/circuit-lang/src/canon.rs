//! Deterministic canonical YAML emission of the kernel model.

use crate::model::*;
use indexmap::IndexMap;
use std::fmt::Write;

/// Quote a YAML scalar only when needed.
///
/// Note: `':'` is treated as a safe (unquoted) character. This is valid ONLY
/// because emission is always flow-style (`{…}`), where `:` is the key/value
/// separator and a bare `:` inside a flow scalar is unambiguous. If block-style
/// mapping values are ever introduced, `':'` must be removed from the safe set
/// (or such scalars quoted), since `a: b` in block context parses as a mapping.
fn q(s: &str) -> String {
    // YAML null tokens are all-alphanumeric but parse back as null, so they
    // must be quoted to survive round-trip (mirrors `yaml::is_null`).
    let safe = !s.is_empty()
        && !matches!(s, "null" | "Null" | "NULL")
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

/// Three-way natural ordering: `Less` / `Equal` / `Greater`.
///
/// Use this wherever a comparator is needed (e.g. `sort_by`); it avoids
/// repeating the two-call `natural_lt` pattern.
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    if natural_lt(a, b) {
        std::cmp::Ordering::Less
    } else if natural_lt(b, a) {
        std::cmp::Ordering::Greater
    } else {
        std::cmp::Ordering::Equal
    }
}

fn sorted<V>(m: &IndexMap<String, V>) -> Vec<(&String, &V)> {
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
    // Power nets are no longer emitted as a `power:` list — they're implied by the
    // placed power-symbol components (round-tripped as ordinary components).
    if !d.lint_allow.is_empty() {
        // `lint_allow` is a BTreeSet, so iteration is already sorted/deterministic.
        let codes: Vec<String> = d.lint_allow.iter().map(|c| q(c)).collect();
        writeln!(o, "lint: {{allow: [{}]}}", codes.join(", ")).unwrap();
    }
    o.push_str("blocks:\n");
    for (bname, block) in &d.blocks {
        writeln!(o, "  {}:", q(bname)).unwrap();
        if let Some(note) = &block.note {
            writeln!(o, "    note: {}", q(note)).unwrap();
        }
        if !block.layout.is_empty() {
            // Per-block placement grid, flow-style so it round-trips: a `~` hole
            // emits as the YAML null token, re-parsed back to `None`.
            let rows: Vec<String> = block
                .layout
                .iter()
                .map(|row| {
                    let cells: Vec<String> = row
                        .iter()
                        .map(|c| match c {
                            Some(name) => q(name),
                            None => "~".to_string(),
                        })
                        .collect();
                    format!("[{}]", cells.join(", "))
                })
                .collect();
            writeln!(o, "    layout: [{}]", rows.join(", ")).unwrap();
        }
        o.push_str("    components:\n");

        // Re-sugar: collect decouple synths per parent (value -> count).
        let mut decouple: IndexMap<&str, IndexMap<&str, u32>> = IndexMap::new();
        for (_, c) in block.components.iter() {
            if let Origin::Synthesized { parent, role, .. } = &c.origin
                && role == "decouple"
            {
                // Desugar always sets `value` on synthesized decouple caps.
                let value = c.value.as_deref().expect("decouple synth has value");
                *decouple
                    .entry(parent.as_str())
                    .or_default()
                    .entry(value)
                    .or_default() += 1;
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
                let mut entries: Vec<_> = dec.iter().collect();
                entries.sort_by(|(a, _), (b, _)| a.cmp(b));
                let ds: Vec<String> = entries
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
            // power-ness is emitted in the top-level `power:` list, not here.
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
    use crate::provider::SymbolTable;

    const SRC: &str = "
version: 1
name: t
blocks:
  mcu:
    layout:
      - [U1, R7]
    components:
      U1: {part: M:CPU, decouple: {100nF: 2}, pins: {VDD: 3V3, VSS: GND, PB6: SCL}}
      R7: {part: R, value: 4.7k, between: [SCL, 3V3]}
";

    fn compile(src: &str) -> crate::model::Design {
        let p = SymbolTable::with_basics();
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

    const SRC2: &str = "
version: 1
blocks:
  b:
    components:
      U1:
        part: M:OP
        units:
          A: {pins: {'+': X, OUT: nc}}
          B: {pins: {'-': Y}}
        pins: {VDD: 3V3}
      R1: {part: R, value: 'null', props: {note: 'a b', mpn: '@x'}, pins: {1: A, 2: '+5V'}}
nets:
  X: {class: analog}
";

    #[test]
    fn canonical_round_trips_units_nc_class_and_quoting() {
        let d1 = compile(SRC2);
        let out1 = to_canonical_yaml(&d1);
        // Quoting edge cases survive: null-like value, space, leading symbol.
        assert!(out1.contains("value: 'null'"));
        assert!(out1.contains("'a b'"));
        assert!(out1.contains("'+5V'"));
        // nc pin, net class, and multi-unit emission present.
        assert!(out1.contains("OUT: nc"));
        assert!(out1.contains("class: analog"));
        assert!(out1.contains("units:"));
        let d2 = compile(&out1);
        assert_eq!(d1, d2, "round-trip must preserve the kernel model");
        assert_eq!(out1, to_canonical_yaml(&d2), "emit must be a fixpoint");
    }

    #[test]
    fn decouple_multivalue_round_trip_is_model_stable() {
        // compile() lives in lib.rs; use parse+desugar here with a provider exposing VDD/VSS by name.
        use crate::provider::{SymbolTable, PinType};
        let mut p = SymbolTable::with_basics();
        p.mock_add(
            "M:CPU",
            vec![
                ("VDD", "VDD", PinType::PowerInput, 1),
                ("VSS", "VSS", PinType::PowerInput, 1),
            ],
        );
        let src = "
version: 1
blocks:
  mcu:
    components:
      U1: {part: M:CPU, decouple: {10uF: 1, 100nF: 1}, pins: {VDD: 3V3, VSS: GND}}
";
        let (s1, _) = crate::parse::parse_str(src);
        let (d1, _) = crate::desugar::desugar(&s1.unwrap(), &p);
        let out1 = to_canonical_yaml(&d1);
        let (s2, _) = crate::parse::parse_str(&out1);
        let (d2, _) = crate::desugar::desugar(&s2.unwrap(), &p);
        assert_eq!(
            d1, d2,
            "kernel model must be stable across canonical round-trip"
        );
        assert_eq!(
            out1,
            to_canonical_yaml(&d2),
            "canonical emit must be a text fixpoint"
        );
    }

    #[test]
    fn canonical_round_trips_lint_allow() {
        let src = "
version: 1
lint: {allow: [near-name, single-pin-net]}
blocks:
  main:
    components:
      R1: {part: R, pins: {1: A, 2: GND}}
";
        let d1 = compile(src);
        let out1 = to_canonical_yaml(&d1);
        // Emitted sorted (BTreeSet order) and present.
        assert!(out1.contains("lint: {allow: [near-name, single-pin-net]}"));
        let d2 = compile(&out1);
        assert_eq!(d1, d2, "lint.allow must survive the round-trip");
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

    #[test]
    fn natural_cmp_orders_correctly() {
        use std::cmp::Ordering;
        assert_eq!(natural_cmp("N2", "N10"), Ordering::Less);
        assert_eq!(natural_cmp("N10", "N2"), Ordering::Greater);
        assert_eq!(natural_cmp("R1", "R1"), Ordering::Equal);
    }
}
