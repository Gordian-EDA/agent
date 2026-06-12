use kicad_bridge::env::KicadEnv;

#[test]
fn detects_installed_kicad() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: kicad not found");
        return;
    };
    assert!(env.symbol_dir.join("Device.kicad_sym").exists());
    // Everything we shell out to (sch erc, netlist, svg export, pcb drc)
    // exists from KiCAD 8 on; dev boxes run 9 or 10.
    let major: u32 = env
        .cli_version
        .split('.')
        .next()
        .and_then(|m| m.parse().ok())
        .unwrap_or(0);
    assert!(major >= 8, "unsupported KiCAD {}", env.cli_version);
}

#[test]
fn env_override_wins() {
    // AUTO_PCB_SYMBOL_DIR overrides discovery (used by tests/other distros)
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("Fake.kicad_sym"), "(kicad_symbol_lib)").unwrap();
    let env = KicadEnv::with_symbol_dir(tmp.path().to_path_buf());
    assert_eq!(env.symbol_dir, tmp.path());
}
