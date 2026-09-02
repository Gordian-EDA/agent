use kicad::KicadInstallation;

#[test]
fn detects_installed_kicad() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: kicad not found");
        return;
    };
    assert!(
        env.symbol_dir().join("Device.kicad_sym").is_file()
            || env.symbol_dir().join("Device.kicad_symdir").is_dir(),
        "{}",
        env.symbol_dir().display()
    );
    assert!(
        env.footprint_dir().is_dir(),
        "{}",
        env.footprint_dir().display()
    );
    let major: u32 = env
        .version()
        .split('.')
        .next()
        .and_then(|m| m.parse().ok())
        .unwrap_or(0);
    assert!(
        major >= 10,
        "unsupported KiCAD {}; Gordian requires KiCad 10 or newer",
        env.version()
    );
}

#[test]
fn explicit_library_dirs_are_preserved() {
    let root = tempfile::tempdir().unwrap();
    let symbol_dir = root.path().join("custom-symbols");
    let footprint_dir = root.path().join("custom-footprints");

    let env = KicadInstallation::for_library_tests(symbol_dir.clone(), footprint_dir.clone());

    assert_eq!(env.symbol_dir(), symbol_dir);
    assert_eq!(env.footprint_dir(), footprint_dir);
}

#[cfg(unix)]
#[test]
fn explicit_cli_and_library_paths_are_selected() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir().unwrap();
    let symbols = root.path().join("symbols");
    let footprints = root.path().join("footprints");
    let bin = root.path().join("bin");
    std::fs::create_dir_all(&symbols).unwrap();
    std::fs::create_dir_all(&footprints).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    let cli = bin.join("kicad-cli");
    std::fs::write(&cli, "#!/bin/sh\necho 10.0.5\n").unwrap();
    std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();

    let env = KicadInstallation::detect_with(Some(&symbols), Some(&footprints), Some(&cli)).unwrap();

    assert_eq!(env.version(), "10.0.5");
    assert_eq!(env.major_version(), Some(10));
}

#[cfg(unix)]
#[test]
fn kicad_nine_is_rejected_with_configuration_guidance() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir().unwrap();
    let symbols = root.path().join("symbols");
    let footprints = root.path().join("footprints");
    std::fs::create_dir_all(&symbols).unwrap();
    std::fs::create_dir_all(&footprints).unwrap();
    let cli = root.path().join("kicad-cli");
    std::fs::write(&cli, "#!/bin/sh\necho 9.0.9\n").unwrap();
    std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();

    let error = KicadInstallation::detect_with(Some(&symbols), Some(&footprints), Some(&cli))
        .unwrap_err()
        .to_string();
    assert!(error.contains("version 9.0.9"), "{error}");
    for key in ["kicad.cliPath", "kicad.symbolDir", "kicad.footprintDir"] {
        assert!(error.contains(key), "{error}");
    }
}
