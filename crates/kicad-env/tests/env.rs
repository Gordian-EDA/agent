use kicad_env::KicadEnv;

#[test]
fn detects_installed_kicad() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: kicad not found");
        return;
    };
    assert!(
        env.symbol_dir.join("Device.kicad_sym").is_file()
            || env.symbol_dir.join("Device.kicad_symdir").is_dir(),
        "{}",
        env.symbol_dir.display()
    );
    assert!(
        env.footprint_dir.is_dir(),
        "{}",
        env.footprint_dir.display()
    );
    let major: u32 = env
        .cli_version
        .split('.')
        .next()
        .and_then(|m| m.parse().ok())
        .unwrap_or(0);
    assert!(major >= 8, "unsupported KiCAD {}", env.cli_version);
}

#[test]
fn with_symbol_dir_derives_sibling_footprint_dir() {
    let root = tempfile::tempdir().unwrap();
    let symbol_dir = root.path().join("symbols");
    let footprint_dir = root.path().join("footprints");

    let env = KicadEnv::with_symbol_dir(symbol_dir.clone());

    assert_eq!(env.symbol_dir, symbol_dir);
    assert_eq!(env.footprint_dir, footprint_dir);
}

#[test]
fn explicit_library_dirs_are_preserved() {
    let root = tempfile::tempdir().unwrap();
    let symbol_dir = root.path().join("custom-symbols");
    let footprint_dir = root.path().join("custom-footprints");

    let env = KicadEnv::with_library_dirs(symbol_dir.clone(), footprint_dir.clone());

    assert_eq!(env.symbol_dir, symbol_dir);
    assert_eq!(env.footprint_dir, footprint_dir);
}
