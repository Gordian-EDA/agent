//! Semantic lints over the kernel model.

use crate::diag::{Diagnostic, Diagnostics};
use crate::model::*;
use crate::provider::{PinType, SymbolTable};

pub fn lint(d: &Design, provider: &SymbolTable) -> Diagnostics {
    let mut diags = Diagnostics::default();
    let mut net_pins: indexmap::IndexMap<&str, Vec<String>> = indexmap::IndexMap::new();
    // Net-sanity: nets carrying a crystal/oscillator pin, and nets carrying a
    // reset/boot control pin. A net with BOTH shorts the oscillator to reset —
    // almost always a mis-wire (an OSC_OUT net the model named after a reset pin).
    let mut osc_nets: std::collections::BTreeSet<String> = Default::default();
    let mut ctrl_pins: indexmap::IndexMap<String, Vec<String>> = Default::default();

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
                    if let Some(prev) = covered.insert(&p.number, key)
                        && prev != key.as_str()
                    {
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

            // Every power-input pin must be covered AND on a net.
            for p in meta.pins.iter().filter(|p| p.etype == PinType::PowerInput) {
                let on_net = covered.get(p.number.as_str()).is_some_and(|key| {
                    pin_target_for(comp, key).is_some_and(|t| matches!(t, PinTarget::Net(_)))
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

            // Record oscillator nets (any pin of a crystal/resonator) and reset/boot
            // control pins (by resolved pin NAME) for the net-sanity check below.
            let is_osc = comp.part.contains("Crystal") || comp.part.contains("Resonator");
            for (key, target) in all_pins.clone() {
                let PinTarget::Net(n) = target else { continue };
                if is_osc {
                    osc_nets.insert(n.clone());
                }
                let pin_name = meta
                    .pins
                    .iter()
                    .find(|p| p.number == *key || p.name == *key)
                    .map(|p| p.name.as_str())
                    .unwrap_or(key.as_str());
                let u = pin_name.to_ascii_uppercase();
                if matches!(u.as_str(), "NRST" | "RESET" | "RST" | "NMI") || u.starts_with("BOOT") {
                    ctrl_pins
                        .entry(n.clone())
                        .or_default()
                        .push(format!("{refdes}.{pin_name}"));
                }
            }
        }
    }

    // A crystal/oscillator net that also carries a reset/boot pin is almost certainly
    // a mis-wire — the oscillator output shorted to reset. Surface it so the design is
    // corrected rather than shipping a non-oscillating board.
    if !d.lint_allow.contains("osc-reset-short") {
        for net in &osc_nets {
            if let Some(rpins) = ctrl_pins.get(net) {
                diags.push(Diagnostic::warning(
                    "osc-reset-short",
                    format!(
                        "net `{net}` joins a crystal/oscillator pin and a reset/boot pin ({}) — \
                         likely a mis-wire shorting the oscillator to a control pin",
                        rpins.join(", ")
                    ),
                ));
            }
        }
    }

    let allow = |code: &str| d.lint_allow.contains(code);

    if !allow("single-pin-net") {
        for (net, pins) in &net_pins {
            // A power rail or an author-marked PORT legitimately has one pin (the
            // symbol/label is the connection) — not a typo, so don't warn on it.
            let attrs = d.nets.get(*net);
            let exempt = attrs.map(|a| a.power || a.port).unwrap_or(false);
            if pins.len() == 1 && !exempt {
                diags.push(Diagnostic::warning(
                    "single-pin-net",
                    format!("net `{net}` has only one pin ({}) — typo?", pins[0]),
                ));
            }
        }
    }
    if !allow("near-name") {
        // Compare against the dedup union of referenced and declared-only nets,
        // so a typo'd `nets:` entry one edit away from a wired net still warns.
        let mut names: Vec<&str> = net_pins.keys().copied().collect();
        for net in d.nets.keys() {
            if !net_pins.contains_key(net.as_str()) {
                names.push(net.as_str());
            }
        }
        // A genuine typo forks a net, leaving one side with 0–1 pins. When BOTH
        // nets have ≥2 pins they are deliberate (GPIO buses: PA0/PA1/…), and
        // warning floods real designs — skip those pairs.
        let pin_count = |n: &str| net_pins.get(n).map_or(0, Vec::len);
        for (i, a) in names.iter().enumerate() {
            for b in &names[i + 1..] {
                if pin_count(a) >= 2 && pin_count(b) >= 2 {
                    continue;
                }
                if strsim::levenshtein(a, b) == 1 {
                    diags.push(Diagnostic::warning(
                        "near-name",
                        format!("nets `{a}` and `{b}` differ by one character — intentional?"),
                    ));
                }
            }
        }
    }
    if !allow("unreferenced-net") {
        for (net, _) in &d.nets {
            if !net_pins.contains_key(net.as_str()) {
                diags.push(Diagnostic::warning(
                    "unreferenced-net",
                    format!("net `{net}` is declared in `nets:` but no pin references it"),
                ));
            }
        }
    }
    diags
}

/// Resolve a component pin map key to its `PinTarget`, searching the top-level
/// pin map first and then any unit pin maps.
fn pin_target_for<'a>(comp: &'a Component, key: &str) -> Option<&'a PinTarget> {
    comp.pins
        .get(key)
        .or_else(|| comp.units.values().find_map(|u| u.get(key)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desugar::desugar;
    use crate::parse::parse_str;
    use crate::provider::{PinType, SymbolTable};

    fn provider() -> SymbolTable {
        use PinType::*;
        let mut p = SymbolTable::with_basics();
        p.mock_add(
            "M:CPU",
            vec![
                ("1", "VDD", PowerInput, 1),
                ("2", "VDD", PowerInput, 1), // stacked
                ("3", "VSS", PowerInput, 1),
                ("4", "PB6", Other, 1),
                ("5", "PB7", Other, 1),
                ("6", "NRST", Other, 1),
            ],
        );
        p.mock_add(
            "Device:Crystal",
            vec![("1", "1", Other, 1), ("2", "2", Other, 1)],
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
    fn crystal_pin_shorted_to_reset_warns() {
        // U1.NRST and the crystal Y1 are both on net OSC — the OSC_OUT-shorted-to-reset
        // mis-wire. The other crystal pin (OSC2) is clean.
        let diags = run("
version: 1
blocks:
  main:
    components:
      U1: {part: M:CPU, pins: {VDD: 3V3, VSS: GND, NRST: OSC, PB7: X}}
      Y1: {part: Device:Crystal, pins: {1: OSC, 2: OSC2}}
");
        let w = diags
            .0
            .iter()
            .find(|d| d.code == "osc-reset-short")
            .expect("osc-reset-short warning");
        assert!(w.message.contains("OSC"));
    }

    #[test]
    fn correctly_wired_crystal_does_not_warn() {
        let diags = run("
version: 1
blocks:
  main:
    components:
      U1: {part: M:CPU, pins: {VDD: 3V3, VSS: GND, NRST: RESET, PB6: OSCIN, PB7: OSCOUT}}
      Y1: {part: Device:Crystal, pins: {1: OSCIN, 2: OSCOUT}}
");
        assert!(
            !diags.0.iter().any(|d| d.code == "osc-reset-short"),
            "clean crystal must not warn: {diags:?}"
        );
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
    fn near_name_skips_pairs_where_both_nets_are_multi_pin() {
        // GPIO-bus pattern: PA0/PA1 each connect MCU + header — intentional,
        // not a typo. Validated against real LLM output: without this rule a
        // bluepill design produces 400+ false near-name warnings.
        let diags = run("
version: 1
blocks:
  main:
    components:
      U1: {part: M:CPU, pins: {VDD: 3V3, VSS: GND, PB6: PA0, PB7: PA1}}
      R1: {part: R, pins: {1: PA0, 2: PA1}}
");
        assert!(
            !diags.0.iter().any(|d| d.code == "near-name"),
            "multi-pin near-named nets must not warn: {diags:?}"
        );
    }

    #[test]
    fn near_name_compares_declared_only_nets() {
        let diags = run("
version: 1
blocks:
  main:
    components:
      R1: {part: R, pins: {1: I2C_SDA, 2: GND}}
nets:
  I2C1_SDA: {class: x}
");
        assert!(
            diags.0.iter().any(|d| d.code == "near-name"),
            "declared-only net one edit away must warn"
        );
    }

    #[test]
    fn lint_allow_suppresses_codes() {
        let diags = run("
version: 1
lint: {allow: [single-pin-net, near-name, unreferenced-net]}
blocks:
  main:
    components:
      R1: {part: R, pins: {1: I2C_SDA, 2: GND}}
      TP1: {part: R, pins: {1: PROBE_ONLY, 2: PROBE_ONLY}}
nets:
  I2C1_SDA: {class: x}
");
        assert!(!diags.0.iter().any(|d| d.code == "single-pin-net"));
        assert!(!diags.0.iter().any(|d| d.code == "near-name"));
        assert!(!diags.0.iter().any(|d| d.code == "unreferenced-net"));
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
