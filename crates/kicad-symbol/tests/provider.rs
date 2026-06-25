use kicad_cli::env::KicadEnv;
use kicad_symbol::provider::RealSymbolProvider;
use kicad_symbol::SymbolProvider;

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
