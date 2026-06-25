use kicad_cli::KicadCli;
use kicad_env::KicadEnv;

#[test]
fn erc_runs_on_blank_schematic_and_parses_report() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD installation detected");
        return;
    };
    let cli = KicadCli::new(&env);
    let report = cli
        .erc(std::path::Path::new("tests/fixtures/blank.kicad_sch"))
        .unwrap();

    assert_eq!(report.error_count(), 0, "{report:?}");
}
