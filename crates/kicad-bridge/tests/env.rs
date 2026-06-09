use kicad_bridge::env::KicadEnv;

#[test]
fn detects_installed_kicad() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: kicad not found");
        return;
    };
    assert!(env.symbol_dir.join("Device.kicad_sym").exists());
    assert!(
        env.cli_version.starts_with("10."),
        "got {}",
        env.cli_version
    );
}

#[test]
fn env_override_wins() {
    // AUTO_PCB_SYMBOL_DIR overrides discovery (used by tests/other distros)
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("Fake.kicad_sym"), "(kicad_symbol_lib)").unwrap();
    let env = KicadEnv::with_symbol_dir(tmp.path().to_path_buf());
    assert_eq!(env.symbol_dir, tmp.path());
}
