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

/// The library half is the half an LLM gets wrong; the symbol name is the half it
/// gets right. Suggestions have to be reachable from that.
#[test]
fn a_wrong_library_still_finds_the_right_symbol() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP");
        return;
    };
    let table = SymbolTable::from_symbol_dir(env.symbol_dir().to_path_buf());
    for (wrong, wanted) in [
        ("Device:Conn_01x02", "Connector_Generic:Conn_01x02"),
        ("Regulator_Switching:TPS62160", "TPS62160"),
    ] {
        assert!(table.symbol(wrong).is_none(), "{wrong} resolved after all");
        let suggestions = table.suggest(wrong);
        assert!(
            suggestions.iter().any(|hit| hit.contains(wanted)),
            "{wrong}: {suggestions:?}"
        );
    }
}
