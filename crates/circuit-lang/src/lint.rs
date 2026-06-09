//! Semantic lints over the kernel model (spec §6).

use crate::diag::{Diagnostic, Diagnostics};
use crate::model::*;
use crate::provider::{PinType, SymbolProvider};

pub fn lint(d: &Design, provider: &dyn SymbolProvider) -> Diagnostics {
    let mut diags = Diagnostics::default();
    let mut net_pins: indexmap::IndexMap<&str, Vec<String>> = indexmap::IndexMap::new();

    for (_bname, block) in &d.blocks {
        for (refdes, comp) in block.components.iter() {
            let all_pins = comp.pins.iter().chain(comp.units.values().flatten());
            for (key, target) in all_pins.clone() {
                if let PinTarget::Net(n) = target {
                    net_pins
                        .entry(n.as_str())
                        .or_default()
                        .push(format!("{refdes}.{key}"));
                }
            }

            let Some(meta) = provider.symbol(&comp.part) else {
                let mut e = Diagnostic::error(
                    "unknown-part",
                    format!("{refdes}: symbol `{}` not found in any library", comp.part),
                );
                if let Some(s) = provider.suggest(&comp.part).into_iter().next() {
                    e = e.with_suggestion(s);
                }
                diags.push(e);
                continue; // pin checks impossible without the symbol
            };

            // Resolve each map key to physical pins: exact number, else name.
            let mut covered: std::collections::HashMap<&str, &str> = Default::default(); // number -> key
            for (key, _) in all_pins.clone() {
                let by_number: Vec<&crate::provider::PinMeta> =
                    meta.pins.iter().filter(|p| p.number == *key).collect();
                let hits = if by_number.is_empty() {
                    meta.pins.iter().filter(|p| p.name == *key).collect()
                } else {
                    by_number
                };
                if hits.is_empty() {
                    let names: Vec<&str> = meta
                        .pins
                        .iter()
                        .flat_map(|p| [p.name.as_str(), p.number.as_str()])
                        .collect();
                    let mut e = Diagnostic::error(
                        "unknown-pin",
                        format!("pin `{key}` not found on {refdes} ({})", comp.part),
                    );
                    if let Some(s) = names
                        .iter()
                        .map(|n| (strsim::levenshtein(key, n), *n))
                        .filter(|(d, _)| *d <= 2)
                        .min_by_key(|(d, _)| *d)
                    {
                        e = e.with_suggestion(s.1);
                    }
                    diags.push(e);
                }
                for p in hits {
                    if let Some(prev) = covered.insert(&p.number, key) {
                        if prev != key.as_str() {
                            diags.push(Diagnostic::error(
                                "pin-conflict",
                                format!(
                                    "{refdes}: physical pin {} claimed by both `{prev}` and `{key}`",
                                    p.number
                                ),
                            ));
                        }
                    }
                }
            }

            // Every power-input pin must be covered AND on a net.
            for p in meta.pins.iter().filter(|p| p.etype == PinType::PowerInput) {
                let on_net = covered.get(p.number.as_str()).is_some_and(|key| {
                    comp.pins
                        .get(*key)
                        .or_else(|| comp.units.values().find_map(|u| u.get(*key)))
                        .is_some_and(|t| matches!(t, PinTarget::Net(_)))
                });
                if !on_net {
                    diags.push(Diagnostic::error(
                        "power-pin-unconnected",
                        format!(
                            "{refdes}: power-input pin {} ({}) is not connected to a net",
                            p.number, p.name
                        ),
                    ));
                }
            }
        }
    }

    for (net, pins) in &net_pins {
        if pins.len() == 1 && !d.nets.get(*net).map(|a| a.power).unwrap_or(false) {
            diags.push(Diagnostic::warning(
                "single-pin-net",
                format!("net `{net}` has only one pin ({}) — typo?", pins[0]),
            ));
        }
    }
    let names: Vec<&str> = net_pins.keys().copied().collect();
    for (i, a) in names.iter().enumerate() {
        for b in &names[i + 1..] {
            if strsim::levenshtein(a, b) == 1 {
                diags.push(Diagnostic::warning(
                    "near-name",
                    format!("nets `{a}` and `{b}` differ by one character — intentional?"),
                ));
            }
        }
    }
    for (net, _) in &d.nets {
        if !net_pins.contains_key(net.as_str()) {
            diags.push(Diagnostic::warning(
                "unreferenced-net",
                format!("net `{net}` is declared in `nets:` but no pin references it"),
            ));
        }
    }
    diags
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desugar::desugar;
    use crate::parse::parse_str;
    use crate::provider::{MockSymbolProvider, PinType};

    fn provider() -> MockSymbolProvider {
        use PinType::*;
        let mut p = MockSymbolProvider::with_basics();
        p.add(
            "M:CPU",
            vec![
                ("1", "VDD", PowerInput, 1),
                ("2", "VDD", PowerInput, 1), // stacked
                ("3", "VSS", PowerInput, 1),
                ("4", "PB6", Other, 1),
                ("5", "PB7", Other, 1),
            ],
        );
        p
    }

    fn run(src: &str) -> crate::diag::Diagnostics {
        let p = provider();
        let (s, mut diags) = parse_str(src);
        let (d, ds) = desugar(&s.unwrap(), &p);
        diags.extend(ds);
        diags.extend(lint(&d, &p));
        diags
    }

    #[test]
    fn unknown_part_and_pin_get_suggestions() {
        let diags = run("
version: 1
blocks:
  main:
    components:
      U1: {part: M:CPU, pins: {VDD: V, VSS: G, PB66: X, PB7: X}}
      U2: {part: M:CPX, pins: {}}
");
        let pin = diags.0.iter().find(|d| d.code == "unknown-pin").unwrap();
        assert_eq!(pin.suggestion.as_deref(), Some("PB6"));
        let part = diags.0.iter().find(|d| d.code == "unknown-part").unwrap();
        assert_eq!(part.suggestion.as_deref(), Some("M:CPU"));
    }

    #[test]
    fn unconnected_power_input_is_an_error() {
        let diags = run("
version: 1
blocks:
  main:
    components:
      U1: {part: M:CPU, pins: {VDD: 3V3, PB6: X, PB7: X}}
"); // VSS missing
        let e = diags
            .0
            .iter()
            .find(|d| d.code == "power-pin-unconnected")
            .unwrap();
        assert!(e.message.contains("VSS"));
    }

    #[test]
    fn warnings_single_pin_near_name_unreferenced() {
        let diags = run("
version: 1
blocks:
  main:
    components:
      U1: {part: M:CPU, pins: {VDD: 3V3, VSS: GND, PB6: I2C_SDA, PB7: I2C1_SDA}}
      R1: {part: R, between: [I2C_SDA, 3V3]}
nets:
  UNUSED: {class: x}
");
        assert!(diags.0.iter().any(|d| d.code == "single-pin-net")); // I2C1_SDA
        assert!(diags.0.iter().any(|d| d.code == "near-name")); // I2C_SDA vs I2C1_SDA
        assert!(diags.0.iter().any(|d| d.code == "unreferenced-net"));
        assert!(!diags.has_errors());
    }

    #[test]
    fn stacked_power_name_counts_as_connected() {
        let diags = run("
version: 1
blocks:
  main:
    components:
      U1: {part: M:CPU, pins: {VDD: 3V3, VSS: GND, PB6: A, PB7: B}}
      R1: {part: R, between: [A, B]}
");
        assert!(!diags.has_errors(), "{:?}", diags); // VDD name covers pins 1 AND 2
    }
}
