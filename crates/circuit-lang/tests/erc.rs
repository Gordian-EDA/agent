//! `sch_check::erc`, driven through the YAML front end.

use circuit_lang::desugar::desugar;
use circuit_lang::parse::parse_str;
use sch_check::SymbolTable;
use sch_check::erc::*;
use sch_check::model::Design;

fn design(src: &str) -> Design {
    let p = SymbolTable::with_basics();
    let (s, _) = parse_str(src);
    let (d, _) = desugar(&s.unwrap(), &p);
    d
}

#[test]
fn value_parser() {
    let approx = |a: Option<f64>, b: f64| a.is_some_and(|x| (x - b).abs() <= b.abs() * 1e-9);
    assert!(approx(parse_value("10k"), 10_000.0));
    assert!(approx(parse_value("1.5k"), 1500.0));
    assert!(approx(parse_value("330R"), 330.0));
    assert!(approx(parse_value("4R7"), 4.7));
    assert!(approx(parse_value("22pF"), 22e-12));
    assert!(approx(parse_value("100nF"), 100e-9));
    assert!(approx(parse_value("4.7uF"), 4.7e-6));
    assert!(approx(parse_value("2M2"), 2.2e6));
    assert!(approx(parse_value("100"), 100.0));
    assert_eq!(parse_value("notavalue"), None);
}

#[test]
fn rail_voltage_parser() {
    assert_eq!(rail_voltage("3V3"), Some(3.3));
    assert_eq!(rail_voltage("+5V"), Some(5.0));
    assert_eq!(rail_voltage("1V8"), Some(1.8));
    assert_eq!(rail_voltage("12V"), Some(12.0));
    assert_eq!(rail_voltage("3.3V"), Some(3.3));
    assert_eq!(rail_voltage("GND"), Some(0.0));
    assert_eq!(rail_voltage("V12"), Some(1.2)); // SoC core-rail convention
    assert_eq!(rail_voltage("V33"), Some(3.3));
    assert_eq!(rail_voltage("V3V3"), Some(3.3));
    assert_eq!(rail_voltage("VOUT"), None); // ambiguous → skip
    assert_eq!(rail_voltage("VCC"), None);
    assert_eq!(rail_voltage("V5"), None); // single digit → ambiguous
}

#[test]
fn direct_555_discharge_on_timing_cap_is_flagged() {
    let d = design(
        "
version: 1
blocks:
  main:
    components:
      U1: {part: Timer:NE555P, pins: {2: TIM, 6: TIM, 7: TIM}}
      C1: {part: Device:C, value: 10uF, between: [TIM, GND]}
",
    );
    let out = erc_checks(&d);
    assert!(
        out.iter().any(|finding| finding.contains("555 DIS")),
        "{out:?}"
    );
}

#[test]
fn normal_555_astable_and_monostable_topologies_are_not_flagged() {
    let astable = design(
        "
version: 1
blocks:
  main:
    components:
      U1: {part: Timer:NE555P, pins: {TR: TIM, THR: TIM, DIS: DISCH}}
      R1: {part: Device:R, value: 10k, between: [VCC, DISCH]}
      R2: {part: Device:R, value: 68k, between: [DISCH, TIM]}
      C1: {part: Device:C, value: 10uF, between: [TIM, GND]}
",
    );
    let monostable = design(
        "
version: 1
blocks:
  main:
    components:
      U1: {part: Timer:LMC555xN, pins: {2: TRIGGER, 6: TIM, 7: TIM}}
      C1: {part: Device:C, value: 10uF, between: [TIM, GND]}
",
    );
    for design in [&astable, &monostable] {
        let out = erc_checks(design);
        assert!(
            !out.iter().any(|finding| finding.contains("555 DIS")),
            "{out:?}"
        );
    }
}

#[test]
fn wrong_v12_divider_flagged() {
    // The recall harness's proven LLM miss: a 1.2 V (V12) core rail whose FB divider is 100x off.
    let d = design(
        "
version: 1
blocks:
  main:
    components:
      R5: {part: Device:R, value: 150k, pins: {1: V12, 2: V12_FB}}
      R6: {part: Device:R, value: 10k, pins: {1: V12_FB, 2: GND}}
",
    );
    assert!(
        erc_checks(&d)
            .iter()
            .any(|s| s.contains("feedback-divider")),
        "{:?}",
        erc_checks(&d)
    );
}

#[test]
fn led_overcurrent_flagged() {
    // 5V rail, 22R series, red LED → (5-1.8)/22 ≈ 145 mA: excessive. Forward-biased so this
    // models a real overcurrent LED and doesn't also trip the polarity check: anode (pin 2 / A)
    // driven from +5V through R, cathode (pin 1 / K) → GND.
    let d = design(
        "
version: 1
blocks:
  main:
    components:
      D1: {part: Device:LED, pins: {1: GND, 2: LED_A}}
      R1: {part: Device:R, value: 22R, pins: {1: '+5V', 2: LED_A}}
",
    );
    let out = erc_checks(&d);
    assert!(
        out.iter()
            .any(|s| s.contains("D1") && s.contains("excessive")),
        "{out:?}"
    );
    assert!(!out.iter().any(|s| s.contains("BACKWARDS")), "{out:?}");
}

#[test]
fn sane_led_not_flagged() {
    // 5V, 330R → ~10 mA: fine. Forward-biased: anode (pin 2 / A) driven from
    // +5V through R, cathode (pin 1 / K) to GND.
    let d = design(
        "
version: 1
blocks:
  main:
    components:
      D1: {part: Device:LED, pins: {1: GND, 2: LED_A}}
      R1: {part: Device:R, value: 330R, pins: {1: '+5V', 2: LED_A}}
",
    );
    assert!(erc_checks(&d).is_empty(), "{:?}", erc_checks(&d));
}

#[test]
fn wrong_fb_divider_flagged() {
    // 3V3 rail; correct ~ Rtop 22k / Rbot 10k with Vref 0.8 → 0.8*(1+2.2)=2.56 (not 3.3, but
    // within none?) — use a clearly-wrong ratio: Rtop 220k / Rbot 10k → 0.8*23=18 V, way off 3.3.
    let d = design(
        "
version: 1
blocks:
  main:
    components:
      U1: {part: Device:R, value: 220k, pins: {1: '3V3', 2: VFB}}
      R2: {part: Device:R, value: 10k, pins: {1: VFB, 2: GND}}
",
    );
    let out = erc_checks(&d);
    assert!(
        out.iter().any(|s| s.contains("feedback-divider")),
        "{out:?}"
    );
}

#[test]
fn correct_fb_divider_not_flagged() {
    // 3V3, Rtop 31.6k / Rbot 10k, Vref 0.8 → 0.8*(1+3.16)=3.33 ≈ 3.3 ✓
    let d = design(
        "
version: 1
blocks:
  main:
    components:
      R1: {part: Device:R, value: 31.6k, pins: {1: '3V3', 2: VFB}}
      R2: {part: Device:R, value: 10k, pins: {1: VFB, 2: GND}}
",
    );
    assert!(erc_checks(&d).is_empty(), "{:?}", erc_checks(&d));
}

#[test]
fn dangling_pin_flagged() {
    let d = design(
        "
version: 1
blocks:
  main:
    components:
      C1: {part: Device:C, value: 100nF, pins: {1: SIG, 2: nc}}
",
    );
    assert!(
        erc_checks(&d)
            .iter()
            .any(|s| s.contains("C1") && s.contains("unconnected")),
        "{:?}",
        erc_checks(&d)
    );
}

#[test]
fn shorted_part_flagged() {
    let d = design(
        "
version: 1
blocks:
  main:
    components:
      R1: {part: Device:R, value: 10k, pins: {1: A, 2: A}}
",
    );
    assert!(
        erc_checks(&d)
            .iter()
            .any(|s| s.contains("R1") && s.contains("shorted")),
        "{:?}",
        erc_checks(&d)
    );
}

#[test]
fn crystal_without_load_caps_flagged() {
    let d = design(
        "
version: 1
blocks:
  main:
    components:
      Y1: {part: Device:Crystal, value: 8MHz, pins: {1: OSC1, 2: OSC2}}
",
    );
    assert!(
        erc_checks(&d)
            .iter()
            .any(|s| s.contains("Y1") && s.contains("load cap")),
        "{:?}",
        erc_checks(&d)
    );
}

#[test]
fn crystal_with_load_caps_ok() {
    let d = design(
        "
version: 1
blocks:
  main:
    components:
      Y1: {part: Device:Crystal, value: 8MHz, pins: {1: OSC1, 2: OSC2}}
      C1: {part: Device:C, value: 22pF, pins: {1: OSC1, 2: GND}}
      C2: {part: Device:C, value: 22pF, pins: {1: OSC2, 2: GND}}
",
    );
    assert!(erc_checks(&d).is_empty(), "{:?}", erc_checks(&d));
}

#[test]
fn reversed_led_flagged() {
    // anode (pin 2) on the ground-via-resistor side ⇒ backwards.
    let d = design(
        "
version: 1
blocks:
  main:
    components:
      D1: {part: Device:LED, pins: {1: DRIVE, 2: LED_K}}
      R1: {part: Device:R, value: 330R, pins: {1: LED_K, 2: GND}}
",
    );
    assert!(
        erc_checks(&d)
            .iter()
            .any(|s| s.contains("D1") && s.contains("BACKWARDS")),
        "{:?}",
        erc_checks(&d)
    );
}

#[test]
fn reversed_pc817_output_transistor_is_flagged() {
    let d = design(
        "
version: 1
blocks:
  main:
    components:
      U1: {part: Isolator:PC817, pins: {1: FIELD_P, 2: FIELD_N, 3: OUT1, 4: GND}}
      R1: {part: Device:R, value: 10k, pins: {1: 3V3, 2: OUT1}}
",
    );
    assert!(
        erc_checks(&d)
            .iter()
            .any(|s| s.contains("U1") && s.contains("output transistor is BACKWARDS")),
        "{:?}",
        erc_checks(&d)
    );
}

#[test]
fn correct_pc817_and_emitter_follower_are_not_flagged() {
    let common_emitter = design(
        "
version: 1
blocks:
  main:
    components:
      U1: {part: Isolator:PC817, pins: {1: FIELD_P, 2: FIELD_N, 3: GND, 4: OUT1}}
      R1: {part: Device:R, value: 10k, pins: {1: 3V3, 2: OUT1}}
",
    );
    assert!(
        !erc_checks(&common_emitter)
            .iter()
            .any(|s| s.contains("output transistor is BACKWARDS")),
        "{:?}",
        erc_checks(&common_emitter)
    );

    // Collector-to-rail / emitter-output is a valid emitter follower and must
    // remain outside this deliberately conservative rule.
    let emitter_follower = design(
        "
version: 1
blocks:
  main:
    components:
      U1: {part: Isolator:LTV-817, pins: {1: FIELD_P, 2: FIELD_N, 3: OUT1, 4: 3V3}}
      R1: {part: Device:R, value: 10k, pins: {1: OUT1, 2: GND}}
",
    );
    assert!(
        !erc_checks(&emitter_follower)
            .iter()
            .any(|s| s.contains("output transistor is BACKWARDS")),
        "{:?}",
        erc_checks(&emitter_follower)
    );
}

#[test]
fn reversed_led_anode_on_gnd_flagged() {
    // anode (pin 2) tied DIRECTLY to GND while the cathode (pin 1) sits on a positive rail
    // (+5V) ⇒ fed backwards, no current path anode→cathode.
    let d = design(
        "
version: 1
blocks:
  main:
    components:
      D1: {part: Device:LED, pins: {1: '+5V', 2: GND}}
",
    );
    assert!(
        erc_checks(&d)
            .iter()
            .any(|s| s.contains("D1") && s.contains("BACKWARDS")),
        "{:?}",
        erc_checks(&d)
    );
}

#[test]
fn reversed_led_anode_on_gnd_cathode_resistor_to_rail_flagged() {
    // Exact indicator shape seen in a lifted design: Device:LED pin 2/A is grounded while
    // pin 1/K reaches 3V3 through the intended current-limiting resistor.
    let d = design(
        "
version: 1
blocks:
  main:
    components:
      LED1: {part: Device:LED, pins: {1: LED_ANODE, 2: GND}}
      R3: {part: Device:R, value: 1k, pins: {1: V3V3, 2: LED_ANODE}}
",
    );
    assert!(
        erc_checks(&d)
            .iter()
            .any(|s| s.contains("LED1") && s.contains("BACKWARDS")),
        "{:?}",
        erc_checks(&d)
    );
}

#[test]
fn clamp_diode_anode_on_gnd_not_flagged() {
    // Legitimate negative-going clamp/protection diode: anode (pin 2) → GND, cathode (pin 1) →
    // a signal net (not a positive rail). It conducts when the signal swings below ground, so it
    // is NOT backwards and must not be flagged.
    let d = design(
        "
version: 1
blocks:
  main:
    components:
      D1: {part: Device:D, pins: {1: SIG, 2: GND}}
",
    );
    assert!(
        !erc_checks(&d).iter().any(|s| s.contains("BACKWARDS")),
        "{:?}",
        erc_checks(&d)
    );
}

#[test]
fn pulled_up_clamp_diode_anode_on_gnd_not_flagged() {
    // A signal clamp can legitimately share this graph shape; only LEDs make the
    // resistor-to-positive-rail branch an unambiguous backwards indicator.
    let d = design(
        "
version: 1
blocks:
  main:
    components:
      D1: {part: Device:D, pins: {1: SIG, 2: GND}}
      R1: {part: Device:R, value: 10k, pins: {1: 3V3, 2: SIG}}
",
    );
    assert!(
        !erc_checks(&d).iter().any(|s| s.contains("BACKWARDS")),
        "{:?}",
        erc_checks(&d)
    );
}

#[test]
fn correct_led_not_flagged() {
    // anode (pin 2) on the driven side, cathode (pin 1) to ground via R ⇒ correct.
    let d = design(
        "
version: 1
blocks:
  main:
    components:
      D1: {part: Device:LED, pins: {1: LED_K, 2: DRIVE}}
      R1: {part: Device:R, value: 330R, pins: {1: LED_K, 2: GND}}
",
    );
    assert!(
        !erc_checks(&d).iter().any(|s| s.contains("BACKWARDS")),
        "{:?}",
        erc_checks(&d)
    );
}

/// A 16-pin IC with VDD/VSS and 14 GPIOs to ground (enough pins to be a
/// decoupling anchor). Used as the body for the missing-/with-decoupling cases.
const IC16: &str = "
      U1: {part: MCU:Generic, pins: {VDD: 3V3, VSS: GND,
            P1: GND, P2: GND, P3: GND, P4: GND, P5: GND, P6: GND, P7: GND,
            P8: GND, P9: GND, P10: GND, P11: GND, P12: GND, P13: GND, P14: GND}}";

#[test]
fn missing_decoupling_flagged() {
    // 16-pin IC on +3V3 with NO rail-to-GND bypass cap → flagged.
    let d = design(&format!(
        "
version: 1
blocks:
  main:
    components:{IC16}
      PWR1: {{part: power:+3V3, pins: {{1: '3V3'}}}}
"
    ));
    assert!(
        erc_checks(&d)
            .iter()
            .any(|s| s.contains("U1") && s.contains("decoupling")),
        "{:?}",
        erc_checks(&d)
    );
}

#[test]
fn decoupled_ic_not_flagged() {
    // Same IC, now with a 100nF from its 3V3 rail to GND → no finding.
    let d = design(&format!(
        "
version: 1
blocks:
  main:
    components:{IC16}
      C1: {{part: Device:C, value: 100nF, pins: {{1: '3V3', 2: GND}}}}
      PWR1: {{part: power:+3V3, pins: {{1: '3V3'}}}}
"
    ));
    assert!(
        !erc_checks(&d).iter().any(|s| s.contains("decoupling")),
        "{:?}",
        erc_checks(&d)
    );
}

#[test]
fn small_ic_without_decoupling_not_flagged() {
    // An 8-pin part is BELOW the anchor threshold — requiring decoupling on it would
    // be noise (a 555/op-amp), so it must not fire.
    let d = design(
        "
version: 1
blocks:
  main:
    components:
      U1: {part: Timer:NE555P, pins: {VCC: 9V, GND: GND, TR: T, THR: T, DIS: D, CV: C, R: 9V, Q: Q}}
      PWR1: {part: power:VCC, pins: {1: 9V}}
",
    );
    assert!(
        !erc_checks(&d).iter().any(|s| s.contains("decoupling")),
        "{:?}",
        erc_checks(&d)
    );
}

#[test]
fn missing_pullup_flagged() {
    // An I2C bus (SDA/SCL) driven by two parts but with no pull-up to a rail.
    let d = design(
        "
version: 1
blocks:
  main:
    components:
      U1: {part: MCU:Generic, pins: {VDD: 3V3, VSS: GND, PB6: I2C1_SCL, PB7: I2C1_SDA}}
      U2: {part: Sensor:Generic, pins: {SCL: I2C1_SCL, SDA: I2C1_SDA, VCC: 3V3, GND: GND}}
",
    );
    let out = erc_checks(&d);
    assert!(
        out.iter()
            .any(|s| s.contains("I2C1_SDA") && s.contains("pull-up")),
        "{out:?}"
    );
    assert!(
        out.iter()
            .any(|s| s.contains("I2C1_SCL") && s.contains("pull-up")),
        "{out:?}"
    );
}

#[test]
fn pulled_up_i2c_not_flagged() {
    // Same bus, now with 4.7k pull-ups to 3V3 → no finding.
    let d = design(
        "
version: 1
blocks:
  main:
    components:
      U1: {part: MCU:Generic, pins: {VDD: 3V3, VSS: GND, PB6: I2C1_SCL, PB7: I2C1_SDA}}
      U2: {part: Sensor:Generic, pins: {SCL: I2C1_SCL, SDA: I2C1_SDA, VCC: 3V3, GND: GND}}
      R1: {part: Device:R, value: 4.7k, pins: {1: I2C1_SCL, 2: '3V3'}}
      R2: {part: Device:R, value: 4.7k, pins: {1: I2C1_SDA, 2: '3V3'}}
",
    );
    assert!(
        !erc_checks(&d).iter().any(|s| s.contains("pull-up")),
        "{:?}",
        erc_checks(&d)
    );
}

#[test]
fn floating_input_flagged() {
    // A reset input (NRST) on an 8-pin IC reaching nothing else → floating.
    let d = design(
        "
version: 1
blocks:
  main:
    components:
      U1: {part: MCU:Generic, pins: {VDD: 3V3, VSS: GND, NRST: RESET_N,
            PA0: A0, PA1: A1, PA2: A2, PA3: A3, PA4: A4}}
",
    );
    assert!(
        erc_checks(&d)
            .iter()
            .any(|s| s.contains("U1") && s.contains("floating")),
        "{:?}",
        erc_checks(&d)
    );
}

#[test]
fn driven_reset_not_flagged() {
    // Same NRST, now pulled up to 3V3 by R1 (degree-2 net) → not floating.
    let d = design(
        "
version: 1
blocks:
  main:
    components:
      U1: {part: MCU:Generic, pins: {VDD: 3V3, VSS: GND, NRST: RESET_N,
            PA0: A0, PA1: A1, PA2: A2, PA3: A3, PA4: A4}}
      R1: {part: Device:R, value: 10k, pins: {1: RESET_N, 2: '3V3'}}
",
    );
    assert!(
        !erc_checks(&d).iter().any(|s| s.contains("floating")),
        "{:?}",
        erc_checks(&d)
    );
}

#[test]
fn undriven_rail_flagged() {
    // GND has a power flag (convention in use), but the +5V rail consumed by the
    // IC has NO source — no power:+5V flag, connector, or regulator drives it.
    let d = design(
        "
version: 1
blocks:
  main:
    components:
      U1: {part: MCU:Generic, pins: {VDD: '+5V', VSS: GND, PA0: A0, PA1: A1}}
      G1: {part: power:GND, pins: {1: GND}}
",
    );
    assert!(
        erc_checks(&d)
            .iter()
            .any(|s| s.contains("+5V") && s.contains("sources it")),
        "{:?}",
        erc_checks(&d)
    );
}

#[test]
fn sourced_rail_not_flagged() {
    // Same, with a power:+5V flag declaring the rail → sourced, no finding.
    let d = design(
        "
version: 1
blocks:
  main:
    components:
      U1: {part: MCU:Generic, pins: {VDD: '+5V', VSS: GND, PA0: A0, PA1: A1}}
      PWR1: {part: power:+5V, pins: {1: '+5V'}}
      G1: {part: power:GND, pins: {1: GND}}
",
    );
    assert!(
        !erc_checks(&d).iter().any(|s| s.contains("sources it")),
        "{:?}",
        erc_checks(&d)
    );
}

#[test]
fn output_short_flagged() {
    // Two distinct regulators tie their outputs to the same VOUT node → conflict.
    let d = design(
        "
version: 1
blocks:
  main:
    components:
      U1: {part: Regulator_Linear:Reg1, pins: {VI: VIN, VO: VOUT, GND: GND}}
      U2: {part: Regulator_Linear:Reg2, pins: {VI: VIN, VO: VOUT, GND: GND}}
      PWR1: {part: power:GND, pins: {1: GND}}
",
    );
    assert!(
        erc_checks(&d)
            .iter()
            .any(|s| s.contains("VOUT") && s.contains("driver conflict")),
        "{:?}",
        erc_checks(&d)
    );
}

#[test]
fn single_regulator_output_not_flagged() {
    // One regulator driving VOUT → no conflict.
    let d = design(
        "
version: 1
blocks:
  main:
    components:
      U1: {part: Regulator_Linear:Reg1, pins: {VI: VIN, VO: VOUT, GND: GND}}
      C1: {part: Device:C, value: 10uF, pins: {1: VOUT, 2: GND}}
      PWR1: {part: power:GND, pins: {1: GND}}
",
    );
    assert!(
        !erc_checks(&d).iter().any(|s| s.contains("driver conflict")),
        "{:?}",
        erc_checks(&d)
    );
}
