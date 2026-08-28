use kicad::KicadInstallation;
use kicad_symbol::SymbolTable;

#[test]
fn unknown_part_gets_real_suggestions() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP");
        return;
    };
    let table = SymbolTable::from_symbol_dir(env.symbol_dir().to_path_buf());
    // stale KiCAD-8-era name an LLM will emit (validated failure mode)
    assert!(table.symbol("Connector:USB_C_Receptacle_USB2.0").is_none());
    let sugg = table.suggest("Connector:USB_C_Receptacle_USB2.0");
    assert!(
        sugg.iter()
            .any(|s| s.contains("USB_C_Receptacle_USB2.0_16P")
                || s.contains("USB_C_Receptacle_USB2.0_14P")),
        "{sugg:?}"
    );
}
