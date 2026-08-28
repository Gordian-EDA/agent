use kicad::KicadInstallation;

#[test]
fn erc_runs_on_blank_schematic_and_parses_report() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCAD installation detected");
        return;
    };
    let cli = env;
    let report = cli
        .erc(std::path::Path::new("tests/fixtures/blank.kicad_sch"))
        .unwrap();

    assert_eq!(report.error_count(), 0, "{report:?}");
}
