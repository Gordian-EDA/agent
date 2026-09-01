//! Idiom-engine regression: `infer_ir` must recognize the crystal-network and
//! decoupling-bank idioms from connectivity alone (no new YAML syntax) and report
//! them on `LayoutIr.idioms`, pinning their members in `LayoutIr.frozen`.

use kicad::KicadInstallation;
use kicad_symbol::SymbolTable;
use sch_floorplan::floorplan;
use serde_json::json;
use std::path::Path;

fn validation_corpus_available() -> bool {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/validation")
        .is_dir()
}

fn compile_fixture(provider: &SymbolTable, name: &str) -> sch_check::Design {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(format!("tests/fixtures/validation/{name}.place-parts.json"));
    let src = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {name}: {e}"));
    let input: sch_check::PlacePartsInput = serde_json::from_str(&src).unwrap();
    let (design, diagnostics, _) = sch_check::into_design(&input, provider, &Default::default());
    assert!(!diagnostics.has_errors(), "{name}: {:#?}", diagnostics);
    design
}

fn compile_source(
    provider: &SymbolTable,
    name: &str,
    input: serde_json::Value,
) -> sch_check::Design {
    let input: sch_check::PlacePartsInput = serde_json::from_value(input).unwrap();
    let (design, diagnostics, _) = sch_check::into_design(&input, provider, &Default::default());
    assert!(!diagnostics.has_errors(), "{name}: {:#?}", diagnostics);
    design
}

#[test]
fn infer_ir_recognizes_crystal_and_decoupling_idioms() {
    if !validation_corpus_available() {
        eprintln!("docs/validation corpus not present; skipping idiom detection test");
        return;
    }
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("no KiCAD environment; skipping idiom detection test");
        return;
    };
    let provider = SymbolTable::from_symbol_dir(env.symbol_dir().to_path_buf());
    let design = compile_fixture(&provider, "idiom-stm32");
    let ir = floorplan::infer_ir(&env, &design);

    // A crystal cluster (Y1 + its two 22pF load caps) is recognized.
    let crystal = ir
        .idioms
        .iter()
        .find(|i| i.kind == "crystal")
        .expect("crystal idiom detected on idiom-stm32");
    assert_eq!(crystal.anchor, "U1");
    assert!(
        crystal.parts.contains(&"Y1".to_string()),
        "crystal includes Y1: {:?}",
        crystal.parts
    );
    assert_eq!(
        crystal.parts.len(),
        3,
        "crystal = Y1 + 2 load caps: {:?}",
        crystal.parts
    );

    // A decoupling bank (>=3 rail-to-rail caps on the +3V3 rail) is recognized.
    let deco = ir
        .idioms
        .iter()
        .find(|i| i.kind == "decoupling")
        .expect("decoupling idiom detected on idiom-stm32");
    assert_eq!(deco.anchor, "U1");
    assert!(
        deco.parts.len() >= 3,
        "decoupling bank >=3 caps: {:?}",
        deco.parts
    );

    // FROZEN idioms (crystal/decoupling) pin their members so the search ships the
    // cluster intact; a REPORT-ONLY idiom (led_indicator) is recognized but flows
    // through normal placement (tidied by an mm post-pass), so it is NOT frozen.
    for idiom in &ir.idioms {
        let must_freeze = idiom.kind != "led_indicator";
        for p in &idiom.parts {
            assert_eq!(
                ir.frozen.contains(p),
                must_freeze,
                "idiom {} member {p} frozen?",
                idiom.kind
            );
        }
    }

    // A board with NO crystal/decoupling-bank fires no idiom (additive engine).
    let plain = compile_fixture(&provider, "divider-filter");
    assert!(
        floorplan::infer_ir(&env, &plain).idioms.is_empty(),
        "divider-filter has no idioms"
    );
}

/// Regression: the decoupling bank must still fire when the supply rail it sits on
/// reaches a SECOND IC — the universal case of a regulator (U2, an AMS1117 LDO)
/// feeding the MCU (U1) it decouples. A bypass cap's nets are BOTH rails (V+ and
/// GND), and that V+ rail necessarily reaches the LDO too; an earlier guard dropped
/// any cap whose net touched another IC, which collapsed the whole bank on every
/// real LDO+MCU board and scattered the caps. Only a shared SIGNAL net should
/// disqualify a cap, never a shared rail.
#[test]
fn decoupling_bank_survives_a_shared_rail_to_a_second_ic() {
    if !validation_corpus_available() {
        eprintln!("docs/validation corpus not present; skipping idiom detection test");
        return;
    }
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("no KiCAD environment; skipping idiom detection test");
        return;
    };
    let provider = SymbolTable::from_symbol_dir(env.symbol_dir().to_path_buf());
    let design = compile_fixture(&provider, "idiom-stm32-ldo");
    let ir = floorplan::infer_ir(&env, &design);

    let deco = ir
        .idioms
        .iter()
        .find(|i| i.kind == "decoupling")
        .expect("decoupling bank still detected with an LDO on the same +3V3 rail");
    assert_eq!(
        deco.anchor, "U1",
        "bank decouples the MCU, not the regulator"
    );
    assert!(
        deco.parts.len() >= 3,
        "the full bank survives the shared rail (>=3 caps): {:?}",
        deco.parts
    );
}

#[test]
fn repeated_pc817_channels_are_frozen_as_signal_flow_rows() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("no KiCAD environment; skipping PC817 channel-layout test");
        return;
    };
    let provider = SymbolTable::from_symbol_dir(env.symbol_dir().to_path_buf());
    let design = compile_source(
        &provider,
        "pc817-bank",
        json!({"parts": [
            {"ref":"P1","part":"power:+5V","pins":{"1":"+5V"}},
            {"ref":"P2","part":"power:GND","pins":{"1":"FIELD_GND"}},
            {"ref":"P3","part":"power:GND","pins":{"1":"LOGIC_GND"}},
            {"ref":"RIN1","part":"Device:R","value":"4.7k","pins":{"1":"IN1","2":"OPTO_IN1"}},
            {"ref":"U1","part":"Isolator:PC817","pins":{"1":"OPTO_IN1","2":"FIELD_GND","3":"LOGIC_GND","4":"OUT1"}},
            {"ref":"RPU1","part":"Device:R","value":"10k","pins":{"1":"+5V","2":"OUT1"}},
            {"ref":"RLED1","part":"Device:R","value":"1k","pins":{"1":"+5V","2":"LED_A1"}},
            {"ref":"DLED1","part":"Device:LED","pins":{"A":"LED_A1","K":"OUT1"}},
            {"ref":"RIN2","part":"Device:R","value":"4.7k","pins":{"1":"IN2","2":"OPTO_IN2"}},
            {"ref":"U2","part":"Isolator:PC817","pins":{"1":"OPTO_IN2","2":"FIELD_GND","3":"LOGIC_GND","4":"OUT2"}},
            {"ref":"RPU2","part":"Device:R","value":"10k","pins":{"1":"+5V","2":"OUT2"}},
            {"ref":"RLED2","part":"Device:R","value":"1k","pins":{"1":"+5V","2":"LED_A2"}},
            {"ref":"DLED2","part":"Device:LED","pins":{"A":"LED_A2","K":"OUT2"}}
        ]}),
    );
    let ir = floorplan::infer_ir(&env, &design);

    let channels: Vec<_> = ir
        .idioms
        .iter()
        .filter(|idiom| idiom.kind == "pc817_channel")
        .collect();
    assert_eq!(
        channels.len(),
        2,
        "one recognized idiom per complete channel"
    );
    assert_eq!(channels[0].anchor, "U1");
    assert_eq!(channels[1].anchor, "U2");
    for (n, channel) in channels.iter().enumerate() {
        let n = n + 1;
        let expected = [
            format!("RIN{n}"),
            format!("U{n}"),
            format!("RPU{n}"),
            format!("RLED{n}"),
            format!("DLED{n}"),
        ];
        assert_eq!(channel.parts, expected);
        assert!(expected.iter().all(|rd| ir.frozen.contains(rd)));
    }

    let u1 = ir.place["U1"];
    let u2 = ir.place["U2"];
    assert_eq!(u1.col, u2.col, "opto bodies form one aligned column");
    assert!(u1.row < u2.row, "channel numbers flow top to bottom");
    assert_eq!(ir.place["RIN1"].row, u1.row);
    assert!(ir.place["RIN1"].col < u1.col);
    assert_eq!(ir.place["RPU1"].row, u1.row - 1);
    assert_eq!(ir.place["RLED1"].row, u1.row - 1);
    assert_eq!(ir.place["DLED1"].row, u1.row);
    assert_eq!(ir.place["RIN1"].orient, floorplan::Orient::Right);
    assert_eq!(ir.place["RPU1"].orient, floorplan::Orient::Down);
    assert_eq!(ir.place["RLED1"].orient, floorplan::Orient::Down);
    assert_eq!(ir.place["DLED1"].orient, floorplan::Orient::Up);
}

#[test]
fn large_pc817_bank_folds_into_bounded_channel_columns() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("no KiCAD environment; skipping PC817 channel-layout test");
        return;
    };
    let provider = SymbolTable::from_symbol_dir(env.symbol_dir().to_path_buf());
    let mut parts = vec![
        json!({"ref":"P1","part":"power:+5V","pins":{"1":"+5V"}}),
        json!({"ref":"P2","part":"power:GND","pins":{"1":"FIELD_GND"}}),
        json!({"ref":"P3","part":"power:GND","pins":{"1":"LOGIC_GND"}}),
        json!({"ref":"J1","part":"Connector:Conn_01x08_Pin","pins":{"1":"IN1","2":"IN2","3":"IN3","4":"IN4","5":"IN5","6":"IN6","7":"IN7","8":"IN8"}}),
        json!({"ref":"J2","part":"Connector:Conn_01x08_Pin","pins":{"1":"OUT1","2":"OUT2","3":"OUT3","4":"OUT4","5":"OUT5","6":"OUT6","7":"OUT7","8":"OUT8"}}),
        json!({"ref":"J3","part":"Connector:Conn_01x02_Pin","pins":{"1":"+5V","2":"FIELD_GND"}}),
        json!({"ref":"C1","part":"Device:C","value":"100n","pins":{"1":"+5V","2":"LOGIC_GND"}}),
        json!({"ref":"H1","part":"Mechanical:MountingHole"}),
    ];
    for n in 1..=8 {
        parts.extend([
            json!({"ref":format!("RIN{n}"),"part":"Device:R","value":"4.7k","pins":{"1":format!("IN{n}"),"2":format!("OPTO_IN{n}")}}),
            json!({"ref":format!("U{n}"),"part":"Isolator:PC817","pins":{"1":format!("OPTO_IN{n}"),"2":"FIELD_GND","3":"LOGIC_GND","4":format!("OUT{n}")}}),
            json!({"ref":format!("RPU{n}"),"part":"Device:R","value":"10k","pins":{"1":"+5V","2":format!("OUT{n}")}}),
            json!({"ref":format!("RLED{n}"),"part":"Device:R","value":"1k","pins":{"1":"+5V","2":format!("LED_A{n}")}}),
            json!({"ref":format!("DLED{n}"),"part":"Device:LED","pins":{"A":format!("LED_A{n}"),"K":format!("OUT{n}")}}),
        ]);
    }
    let design = compile_source(&provider, "pc817-bank-8", json!({"parts": parts}));
    let ir = floorplan::infer_ir(&env, &design);

    let optos: Vec<_> = (1..=8).map(|n| ir.place[&format!("U{n}")]).collect();
    let cols: std::collections::BTreeSet<_> = optos.iter().map(|cell| cell.col).collect();
    let rows: std::collections::BTreeSet<_> = optos.iter().map(|cell| cell.row).collect();
    assert_eq!(
        cols.len(),
        2,
        "eight wide channel cells fold into two banks"
    );
    assert_eq!(rows.len(), 4, "each bank is bounded to four channel rows");
    assert!(optos[..4].windows(2).all(|pair| pair[0].row < pair[1].row));
    assert!(optos[4..].windows(2).all(|pair| pair[0].row < pair[1].row));
    assert!(optos[..4].iter().all(|cell| cell.col == optos[0].col));
    assert!(optos[4..].iter().all(|cell| cell.col == optos[4].col));
    assert!(optos[4].col > optos[0].col);

    let min_channel_col = optos.iter().map(|cell| cell.col).min().unwrap();
    let max_channel_col = optos.iter().map(|cell| cell.col).max().unwrap();
    assert!(ir.place["J1"].col < min_channel_col);
    assert!(ir.place["J2"].col > max_channel_col);
    for refdes in ["J3", "C1", "H1"] {
        let cell = ir.place[refdes];
        assert!(
            cell.col > ir.place["J2"].col && cell.col <= ir.place["J2"].col + 6,
            "{refdes} stays in the bounded support island beside the bank: {cell:?}"
        );
        assert!(
            cell.row <= *rows.iter().max().unwrap() + 2,
            "{refdes} does not create a detached lower page island: {cell:?}"
        );
    }
}
