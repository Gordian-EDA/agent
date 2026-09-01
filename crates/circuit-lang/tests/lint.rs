//! The semantic lints of `sch_check::lint`, driven through the YAML front end.

use circuit_lang::desugar::desugar;
use circuit_lang::parse::parse_str;
use sch_check::lint::lint;
use sch_check::{PinType, SymbolTable};

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
    p.mock_add(
        "Switch:SW_Push",
        vec![("1", "1", Passive, 1), ("2", "2", Passive, 1)],
    );
    p.mock_add(
        "M:REG",
        vec![
            ("1", "IN", PowerInput, 1),
            ("2", "OUT", PowerOutput, 1),
            ("3", "GND", PowerInput, 1),
        ],
    );
    p.mock_add(
        "Connector:Conn_01x02_Pin",
        vec![("1", "Pin_1", Passive, 1), ("2", "Pin_2", Passive, 1)],
    );
    p.mock_add(
        "Device:Polyfuse",
        vec![("1", "~", Passive, 1), ("2", "~", Passive, 1)],
    );
    p
}

fn run(src: &str) -> sch_check::diag::Diagnostics {
    let p = provider();
    let (s, mut diags) = parse_str(src);
    let (d, ds) = desugar(&s.unwrap(), &p);
    diags.extend(ds);
    diags.extend(lint(&d, &p));
    diags
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
fn wide_cross_block_signal_bank_warns_about_fragmented_floorplan() {
    let left = (1..=12)
        .map(|i| format!("R{i}: {{part: R, between: [CH{i}, LEFT{i}]}}"))
        .collect::<Vec<_>>()
        .join(", ");
    let right = (1..=12)
        .map(|i| format!("R{}: {{part: R, between: [CH{i}, RIGHT{i}]}}", i + 12))
        .collect::<Vec<_>>()
        .join(", ");
    let src = format!(
        "version: 1\nblocks:\n  inputs:\n    components: {{{left}}}\n  isolators:\n    components: {{{right}}}\n"
    );
    let diags = run(&src);
    let warning = diags
        .0
        .iter()
        .find(|d| d.code == "fragmented-block-floorplan")
        .expect("wide cross-block bank must be regrouped");
    assert!(warning.message.contains("12 signal nets"));

    let allowed = format!("{src}lint:\n  allow: [fragmented-block-floorplan]\n");
    assert!(
        !run(&allowed)
            .0
            .iter()
            .any(|d| d.code == "fragmented-block-floorplan")
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
fn control_passive_island_warns() {
    let diags = run("
version: 1
blocks:
  main:
    components:
      U1: {part: M:CPU, pins: {VDD: 3V3, VSS: GND, PB6: UART_TX, PB7: UART_RX}}
      R1: {part: R, between: [GPIO0_BOOT, 3V3]}
      S1: {part: Switch:SW_Push, pins: {1: GPIO0_BOOT, 2: GND}}
");
    let w = diags
        .0
        .iter()
        .find(|d| d.code == "control-passive-island")
        .expect("control-passive-island warning");
    assert!(w.message.contains("GPIO0_BOOT"));
}

#[test]
fn unsourced_power_net_warns_for_power_input_island() {
    let diags = run("
version: 1
blocks:
  main:
    components:
      U1: {part: M:CPU, pins: {VDD: DVDD, VSS: GND, PB6: A, PB7: B}}
      C1: {part: C, between: [DVDD, GND]}
nets:
  DVDD: {class: power}
  GND: {class: power}
");
    let w = diags
        .0
        .iter()
        .find(|d| d.code == "unsourced-power-net")
        .expect("unsourced-power-net warning");
    assert!(w.message.contains("DVDD"));
}

#[test]
fn sourced_power_net_does_not_warn() {
    let diags = run("
version: 1
blocks:
  main:
    components:
      U1: {part: M:CPU, pins: {VDD: 3V3, VSS: GND, PB6: A, PB7: B}}
      U2: {part: M:REG, pins: {IN: VIN, OUT: 3V3, GND: GND}}
      J1: {part: Connector:Conn_01x02_Pin, pins: {1: VIN, 2: GND}}
nets:
  3V3: {class: power}
  GND: {class: power}
  VIN: {class: power}
");
    assert!(
        !diags.0.iter().any(|d| d.code == "unsourced-power-net"),
        "sourced rails must not warn: {diags:?}"
    );
}

#[test]
fn connector_power_propagates_through_passive_polyfuse() {
    let diags = run("
version: 1
blocks:
  main:
    components:
      J1: {part: Connector:Conn_01x02_Pin, pins: {1: VBUS, 2: GND}}
      F1: {part: Device:Polyfuse, between: [VBUS, VIN]}
      U1: {part: M:REG, pins: {IN: VIN, OUT: 3V3, GND: GND}}
nets:
  VBUS: {class: power}
  VIN: {class: power}
  3V3: {class: power}
  GND: {class: power}
");
    assert!(
        !diags
            .0
            .iter()
            .any(|d| { d.code == "unsourced-power-net" && d.message.contains("`VIN`") }),
        "connector-fed VIN behind a polyfuse is sourced: {diags:?}"
    );
}

#[test]
fn passive_polyfuse_does_not_source_a_floating_input_rail() {
    let diags = run("
version: 1
blocks:
  main:
    components:
      F1: {part: Device:Polyfuse, between: [VBUS, VIN]}
      U1: {part: M:REG, pins: {IN: VIN, OUT: 3V3, GND: GND}}
nets:
  VBUS: {class: power}
  VIN: {class: power}
  3V3: {class: power}
  GND: {class: power}
");
    assert!(
        diags
            .0
            .iter()
            .any(|d| { d.code == "unsourced-power-net" && d.message.contains("`VIN`") }),
        "a fuse without an upstream source must not hide floating VIN: {diags:?}"
    );
}

#[test]
fn arbitrary_series_resistor_does_not_propagate_power_source() {
    let diags = run("
version: 1
blocks:
  main:
    components:
      J1: {part: Connector:Conn_01x02_Pin, pins: {1: VBUS, 2: GND}}
      R1: {part: R, between: [VBUS, VIN]}
      U1: {part: M:REG, pins: {IN: VIN, OUT: 3V3, GND: GND}}
nets:
  VBUS: {class: power}
  VIN: {class: power}
  3V3: {class: power}
  GND: {class: power}
");
    assert!(
        diags
            .0
            .iter()
            .any(|d| { d.code == "unsourced-power-net" && d.message.contains("`VIN`") }),
        "non-fuse series passives must not mark VIN sourced: {diags:?}"
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
