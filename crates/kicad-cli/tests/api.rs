use kicad_cli::{DrcReport, ErcReport, KicadCli, Netlist};

#[test]
fn crate_root_exports_primary_api_types() {
    fn accepts_api_types(
        _cli: Option<KicadCli>,
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
