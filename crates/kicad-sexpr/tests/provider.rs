use kicad_cli_rs::env::KicadEnv;
use kicad_sexpr::provider::RealSymbolProvider;
use symbol_contract::SymbolProvider;

#[test]
fn compiles_validated_bedrock_design_against_real_libs() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP");
        return;
    };
    let provider = RealSymbolProvider::new(env);
    let src = include_str!("../../../docs/validation/bedrock-oneshot-bluepill.circuit.yaml");
    let result = circuit_lang::compile(src, &provider);
    assert!(!result.diagnostics.has_errors(), "{:?}", result.diagnostics);
    assert!(result.design.is_some());
}

#[test]
fn unknown_part_gets_real_suggestions() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP");
        return;
    };
    let provider = RealSymbolProvider::new(env);
    // stale KiCAD-8-era name an LLM will emit (validated failure mode)
    assert!(
        provider
            .symbol("Connector:USB_C_Receptacle_USB2.0")
            .is_none()
    );
    let sugg = provider.suggest("Connector:USB_C_Receptacle_USB2.0");
    assert!(
        sugg.iter()
            .any(|s| s.contains("USB_C_Receptacle_USB2.0_16P")
                || s.contains("USB_C_Receptacle_USB2.0_14P")),
        "{sugg:?}"
    );
}
