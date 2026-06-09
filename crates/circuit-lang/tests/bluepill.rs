use circuit_lang::{MockSymbolProvider, PinType, compile};

fn provider() -> MockSymbolProvider {
    use PinType::*;
    let mut p = MockSymbolProvider::with_basics();
    p.add(
        "Regulator_Linear:AMS1117-3.3",
        vec![
            ("1", "GND", PowerInput, 1),
            ("2", "VO", PowerOutput, 1),
            ("3", "VI", PowerInput, 1),
        ],
    );
    p.add(
        "Connector:USB_C_Receptacle_USB2.0",
        vec![
            ("A1", "GND", Passive, 1),
            ("A4", "VBUS", Passive, 1),
            ("A5", "CC1", Passive, 1),
            ("B5", "CC2", Passive, 1),
            ("A6", "DP1", Passive, 1),
            ("A7", "DN1", Passive, 1),
            ("B6", "DP2", Passive, 1),
            ("B7", "DN2", Passive, 1),
            ("S1", "SHIELD", Passive, 1),
        ],
    );
    p.add(
        "MCU_ST_STM32H7:STM32H743VITx",
        vec![
            ("17", "VDD", PowerInput, 1),
            ("39", "VDD", PowerInput, 1),
            ("16", "VSS", PowerInput, 1),
            ("38", "VSS", PowerInput, 1),
            ("70", "PA11", Other, 1),
            ("71", "PA12", Other, 1),
            ("92", "PB6", Other, 1),
            ("93", "PB7", Other, 1),
            ("14", "NRST", Other, 1),
        ],
    );
    p.add(
        "Connector_Generic:Conn_01x10",
        vec![
            ("1", "Pin_1", Passive, 1),
            ("2", "Pin_2", Passive, 1),
            ("3", "Pin_3", Passive, 1),
            ("4", "Pin_4", Passive, 1),
            ("5", "Pin_5", Passive, 1),
            ("6", "Pin_6", Passive, 1),
            ("7", "Pin_7", Passive, 1),
            ("8", "Pin_8", Passive, 1),
            ("9", "Pin_9", Passive, 1),
            ("10", "Pin_10", Passive, 1),
        ],
    );
    p
}

#[test]
fn bluepill_compiles_clean() {
    let src = include_str!("fixtures/bluepill-h7.circuit.yaml");
    let r = compile(src, &provider());
    let errors: Vec<_> = r
        .diagnostics
        .0
        .iter()
        .filter(|d| d.severity == circuit_lang::Severity::Error)
        .collect();
    assert!(errors.is_empty(), "{errors:#?}");
    let d = r.design.unwrap();

    // 12 decoupling caps synthesized, tagged to U1
    let mcu = &d.blocks["mcu"];
    let caps = mcu
        .components
        .values()
        .filter(|c| {
            matches!(&c.origin,
            circuit_lang::model::Origin::Synthesized { parent, role, .. }
                if parent == "U1" && role == "decouple")
        })
        .count();
    assert_eq!(caps, 12);

    // pin-refs: J2.3 joined I2C1_SCL via U1.PB6; CC pulldowns synthesized nets
    use circuit_lang::model::PinTarget;
    assert_eq!(
        d.blocks["headers"].components["J2"].pins["3"],
        PinTarget::Net("I2C1_SCL".into())
    );
    assert_eq!(
        d.blocks["usb"].components["R1"].pins["1"],
        PinTarget::Net("N_J1_CC1".into())
    );

    // canonical fixpoint on the real design
    let canon1 = circuit_lang::canon::to_canonical_yaml(&d);
    let r2 = compile(&canon1, &provider());
    let d2 = r2.design.unwrap();
    assert_eq!(d, d2);
    assert_eq!(canon1, circuit_lang::canon::to_canonical_yaml(&d2));
}
