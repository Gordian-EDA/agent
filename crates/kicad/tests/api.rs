use kicad::{DrcReport, ErcReport, KicadInstallation, Netlist};

#[test]
fn crate_root_exports_primary_api_types() {
    fn accepts_api_types(
        _installation: Option<KicadInstallation>,
        _erc: ErcReport,
        _drc: DrcReport,
        _netlist: Netlist,
    ) {
    }

    accepts_api_types(
        None,
        ErcReport::default(),
        DrcReport::default(),
        Netlist::default(),
    );
}
