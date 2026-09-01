//! The checkers over a [`Design`] built directly — no parse, no text.
//!
//! An extracted or tool-built design keys its pins by NUMBER, an authored one by
//! NAME, so the rules that read pin names are exercised both ways here.

use sch_check::model::*;
use sch_check::{Diagnostics, PinType, SymbolTable, erc, lint, nets, pins};

fn provider() -> SymbolTable {
    use PinType::*;
    let mut p = SymbolTable::with_basics();
    p.mock_add(
        "M:CPU",
        vec![
            ("1", "VDD", PowerInput, 1),
            ("2", "VDD", PowerInput, 1),
            ("3", "VSS", PowerInput, 1),
            ("4", "PB6", Other, 1),
            ("5", "NRST", Other, 1),
            ("6", "NC/PA9", NoConnect, 1),
        ],
    );
    p.mock_add(
        "M:BIG",
        vec![
            ("1", "VDD", PowerInput, 1),
            ("2", "VSS", PowerInput, 1),
            ("3", "EN", Other, 1),
            ("4", "SDA", Other, 1),
            ("5", "SCL", Other, 1),
            ("6", "OUT", Other, 1),
            ("7", "IN", Other, 1),
            ("8", "GPIO", Other, 1),
        ],
    );
    p.mock_add(
        "M:REG",
        vec![
            ("1", "IN", PowerInput, 1),
            ("2", "OUT", PowerOutput, 1),
            ("3", "GND", PowerInput, 1),
        ],
    );
    p
}

fn part(lib_id: &str, pins: &[(&str, &str)]) -> Component {
    let mut comp = Component {
        part: lib_id.into(),
        ..Default::default()
    };
    for (key, net) in pins {
        comp.pins
            .insert((*key).into(), PinTarget::Net((*net).into()));
    }
    comp
}

fn design(parts: &[(&str, Component)]) -> Design {
    let mut block = Block::default();
    for (refdes, comp) in parts {
        block.components.insert((*refdes).into(), comp.clone());
    }
    let mut d = Design::default();
    d.blocks.insert("main".into(), block);
    for comp in d.blocks["main"].components.values() {
        for target in comp.pins.values() {
            if let PinTarget::Net(net) = target {
                d.nets.entry(net.clone()).or_default();
            }
        }
    }
    pins::mark_unused_no_connect(&mut d, &provider());
    nets::derive_attrs(&mut d);
    d
}

fn codes(diags: &Diagnostics) -> Vec<&str> {
    diags.0.iter().map(|d| d.code).collect()
}

#[test]
fn unconnected_power_pin_is_an_error() {
    let d = design(&[("U1", part("M:CPU", &[("1", "3V3"), ("3", "GND")]))]);
    let diags = lint::lint(&d, &provider());
    // Physical pin 2 (the second VDD) is on no net.
    assert!(codes(&diags).contains(&"power-pin-unconnected"));
}

#[test]
fn a_connected_library_no_connect_pin_is_an_error() {
    let d = design(&[("U1", part("M:CPU", &[("6", "XTAL2")]))]);
    let diags = lint::lint(&d, &provider());
    let diagnostic = diags
        .0
        .iter()
        .find(|diagnostic| diagnostic.code == "library-no-connect-wired")
        .expect("connected no-connect pin must be rejected");
    assert!(diagnostic.message.contains("U1"), "{diagnostic:?}");
    assert!(diagnostic.message.contains("NC/PA9"), "{diagnostic:?}");
    assert!(diagnostic.message.contains("XTAL2"), "{diagnostic:?}");
}

#[test]
fn a_symbol_outside_the_table_is_not_a_lint_error() {
    let d = design(&[
        ("U9", part("Vendor:UnknownPart", &[("A1", "SIG")])),
        ("U8", part("Vendor:UnknownPart", &[("A1", "SIG")])),
    ]);
    let diags = lint::lint(&d, &provider());
    assert!(!diags.has_errors(), "{:?}", codes(&diags));
    assert!(!codes(&diags).contains(&"unknown-part"));
}

#[test]
fn an_unresolvable_pin_key_is_not_a_lint_error() {
    let d = design(&[("U1", part("M:CPU", &[("PB66", "SIG")]))]);
    let diags = lint::lint(&d, &provider());
    assert!(!codes(&diags).contains(&"unknown-pin"));
}

#[test]
fn a_power_symbol_sources_the_rail_it_drives() {
    let unsourced = design(&[("U1", part("M:CPU", &[("VDD", "+3V3"), ("VSS", "GND")]))]);
    assert!(codes(&lint::lint(&unsourced, &provider())).contains(&"unsourced-power-net"));

    let sourced = design(&[
        ("U1", part("M:CPU", &[("VDD", "+3V3"), ("VSS", "GND")])),
        ("#PWR01", part("power:+3V3", &[("1", "+3V3")])),
        ("#PWR02", part("power:GND", &[("1", "GND")])),
    ]);
    assert!(!codes(&lint::lint(&sourced, &provider())).contains(&"unsourced-power-net"));
}

#[test]
fn a_power_net_is_exempt_from_the_single_pin_rule() {
    let d = design(&[
        ("U1", part("M:CPU", &[("VDD", "+3V3"), ("VSS", "GND")])),
        ("#PWR01", part("power:+5V", &[("1", "+5V")])),
    ]);
    assert!(d.nets["+5V"].power);
    let diags = lint::lint(&d, &provider());
    let single: Vec<&String> = diags
        .0
        .iter()
        .filter(|x| x.code == "single-pin-net")
        .map(|x| &x.message)
        .collect();
    assert!(
        !single.iter().any(|m| m.contains("+5V")),
        "the rail is exempt: {single:?}"
    );
}

#[test]
fn erc_sees_a_dangling_part() {
    let d = design(&[
        ("U1", part("M:REG", &[("IN", "VIN"), ("OUT", "+3V3")])),
        ("R9", part("Device:R", &[("1", "ORPHAN"), ("2", "ORPHAN")])),
    ]);
    let defects = erc::erc_checks(&d, &provider());
    assert!(
        defects
            .iter()
            .any(|s| s.contains("R9") && s.contains("shorted")),
        "expected R9 flagged, got {defects:?}"
    );
}

#[test]
fn erc_reads_a_feedback_divider_from_the_model() {
    let mut reg = part("M:REG", &[("IN", "VIN"), ("OUT", "+5V"), ("GND", "GND")]);
    reg.pins.insert("FB".into(), PinTarget::Net("FB".into()));
    let mut top = part("Device:R", &[("1", "+5V"), ("2", "FB")]);
    top.value = Some("10k".into());
    let mut bottom = part("Device:R", &[("1", "FB"), ("2", "GND")]);
    bottom.value = Some("10k".into());
    let d = design(&[("U1", reg), ("R1", top), ("R2", bottom)]);
    // A 1:1 divider off a 0.8 V reference cannot make 5 V.
    let defects = erc::erc_checks(&d, &provider());
    assert!(
        defects.iter().any(|s| s.contains("feedback-divider")),
        "expected a divider defect, got {defects:?}"
    );
}

#[test]
fn name_reading_rules_survive_number_keyed_pins() {
    // EN (pin 3) is on a net nothing else touches — a floating enable.
    let d = design(&[(
        "U5",
        part(
            "M:BIG",
            &[
                ("1", "+3V3"),
                ("2", "GND"),
                ("3", "EN_FLOAT"),
                ("6", "SIG"),
                ("7", "SIG"),
            ],
        ),
    )]);
    let defects = erc::erc_checks(&d, &provider());
    assert!(
        defects.iter().any(|s| s.contains("floating")),
        "the floating-enable rule must read pin 3 as EN: {defects:?}"
    );
}

#[test]
fn a_global_label_marks_its_net_as_a_port() {
    let mut label = Component {
        part: "label:global".into(),
        ..Default::default()
    };
    label.pins.insert("1".into(), PinTarget::Net("TX".into()));
    let d = design(&[("U1", part("M:CPU", &[("PB6", "TX")])), ("#LBL01", label)]);
    assert!(d.nets["TX"].port);
    // The label itself sits on the net, so the port is not a lone pin.
    let diags = lint::lint(&d, &provider());
    assert!(
        !diags
            .0
            .iter()
            .any(|x| x.code == "single-pin-net" && x.message.contains("TX")),
        "{:?}",
        diags.0
    );
}

#[test]
fn correct_led_indicator_has_no_polarity_finding() {
    let d = design(&[
        ("R1", part("Device:R", &[("1", "+3V3"), ("2", "LED_A")])),
        ("D1", part("Device:LED", &[("1", "GND"), ("2", "LED_A")])),
    ]);

    assert!(
        !erc::erc_checks(&d, &provider())
            .iter()
            .any(|finding| finding.contains("LED is reversed"))
    );
}

#[test]
fn reversed_led_indicator_is_a_blocking_error() {
    let d = design(&[
        ("R1", part("Device:R", &[("1", "+3V3"), ("2", "LED_K")])),
        ("D1", part("Device:LED", &[("1", "LED_K"), ("2", "GND")])),
    ]);

    let findings = erc::defects(&d, &provider());
    let polarity = findings
        .iter()
        .find(|finding| finding.line.contains("LED is reversed"))
        .expect("reversed LED finding");

    assert!(polarity.blocking);
    assert!(polarity.line.contains("swap D1: anode should face +3V3"));
}

#[test]
fn signal_clamp_diode_has_no_led_polarity_finding() {
    let d = design(&[
        ("R1", part("Device:R", &[("1", "+3V3"), ("2", "SIGNAL")])),
        ("D1", part("Device:D", &[("1", "SIGNAL"), ("2", "GND")])),
    ]);

    assert!(
        !erc::erc_checks(&d, &provider())
            .iter()
            .any(|finding| finding.contains("LED is reversed"))
    );
}

/// The same design with every pin key rewritten to its physical pin number —
/// what an extractor or `place_parts` hands the checkers.
fn rekeyed_by_number(d: &Design, provider: &SymbolTable) -> Design {
    let mut out = d.clone();
    for block in out.blocks.values_mut() {
        for comp in block.components.values_mut() {
            let Some(meta) = provider.symbol(&comp.part) else {
                continue;
            };
            let mut renamed = indexmap::IndexMap::new();
            for (key, target) in &comp.pins {
                for pin in pins::resolve(&meta, key) {
                    renamed.insert(pin.number.clone(), target.clone());
                }
            }
            comp.pins = renamed;
        }
    }
    out
}

/// The invariant behind the whole extraction: how a design spells its pin keys
/// must not change what the checkers say about it.
#[test]
fn erc_reads_both_spellings_identically() {
    let provider = provider();
    let cases = [
        design(&[
            ("U1", part("M:CPU", &[("VDD", "+3V3"), ("VSS", "GND")])),
            ("D1", part("Device:LED", &[("A", "+3V3"), ("K", "LED_K")])),
            ("R1", part("Device:R", &[("1", "LED_K"), ("2", "GND")])),
        ]),
        design(&[(
            "U5",
            part(
                "M:BIG",
                &[
                    ("VDD", "+3V3"),
                    ("VSS", "GND"),
                    ("EN", "EN_FLOAT"),
                    ("OUT", "SIG"),
                    ("IN", "SIG"),
                ],
            ),
        )]),
        design(&[("D2", part("Device:LED", &[("A", "GND"), ("K", "+3V3")]))]),
    ];
    for (i, d) in cases.iter().enumerate() {
        let by_name = erc::erc_checks(d, &provider);
        let by_number = erc::erc_checks(&rekeyed_by_number(d, &provider), &provider);
        assert_eq!(by_name, by_number, "case {i}");
    }
}
