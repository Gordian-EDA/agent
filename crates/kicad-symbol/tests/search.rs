use kicad::KicadInstallation;
use kicad_symbol::search::SymbolIndex;

#[test]
fn finds_stm32h743_by_substring() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCAD installation detected");
        return;
    };
    let idx = SymbolIndex::build(env.symbol_dir()).unwrap();
    let hits = idx.search("STM32H743VI", 5);
    assert!(
        hits.iter()
            .any(|h| h.lib_id == "MCU_ST_STM32H7:STM32H743VITx"),
        "{hits:?}"
    );
    let top = &hits[0];
    assert!(top.pin_count > 0, "{top:?}");
}

#[test]
fn fuzzy_finds_usb_c_receptacle() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCAD installation detected");
        return;
    };
    let idx = SymbolIndex::build(env.symbol_dir()).unwrap();
    let hits = idx.search("usb-c receptacle usb2", 8);
    assert!(
        hits.iter()
            .any(|h| h.lib_id.contains("USB_C_Receptacle_USB2.0_16P")),
        "{hits:?}"
    );
}

/// Perf probe: full index build over all installed libs must stay under 2s.
#[test]
#[ignore]
fn build_completes_under_two_seconds() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCAD installation detected");
        return;
    };
    let start = std::time::Instant::now();
    let idx = SymbolIndex::build(env.symbol_dir()).unwrap();
    let elapsed = start.elapsed();
    eprintln!("indexed {} symbols in {elapsed:?}", idx.len());
    assert!(elapsed.as_secs_f64() < 2.0, "build took {elapsed:?}");
}

/// Sub-unit blocks like `NAME_0_1` / `NAME_1_1` are nested one level deeper
/// than top-level symbols and must never surface as search hits.
#[test]
fn sub_unit_blocks_are_not_indexed() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Tiny.kicad_sym"),
        r#"(kicad_symbol_lib (version 20241209) (generator "test")
  (symbol "OpAmp" (pin_names (offset 0.254))
    (property "Reference" "U" (at 0 0 0))
    (symbol "OpAmp_0_1" (rectangle (start -5 -5) (end 5 5)))
    (symbol "OpAmp_1_1"
      (pin output line (at 7 0 180) (length 2)
        (name "out" (effects)) (number "1" (effects)))))
  (symbol "Quoted\"Paren(" ))
"#,
    )
    .unwrap();
    let idx = SymbolIndex::build(dir.path()).unwrap();
    let hits = idx.search("OpAmp", 10);
    assert!(hits.iter().any(|h| h.lib_id == "Tiny:OpAmp"), "{hits:?}");
    assert!(
        !hits.iter().any(|h| h.lib_id.contains("OpAmp_")),
        "sub-unit block leaked into the index: {hits:?}"
    );
}
