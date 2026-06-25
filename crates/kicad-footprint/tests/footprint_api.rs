use kicad_footprint::{
    CourtyardSource, Footprint, FootprintCatalog, LibraryId, PadTechnology, SearchQuery,
};

#[test]
fn footprint_crate_exposes_parser_and_catalog_api() {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/footprints/R_0603_1608Metric.kicad_mod");
    let fp = Footprint::from_file(&fixture).expect("fixture footprint");

    assert_eq!(fp.name, "R_0603_1608Metric");
    assert_eq!(fp.pad_count(), 2);
    assert_eq!(fp.courtyard_source, CourtyardSource::ExplicitCourtyard);
    assert!(
        fp.pads
            .iter()
            .all(|pad| pad.technology == PadTechnology::Smd)
    );

    let tmp = tempfile::tempdir().expect("temp dir");
    let pretty = tmp.path().join("Resistor_SMD.pretty");
    std::fs::create_dir(&pretty).expect("pretty dir");
    std::fs::copy(&fixture, pretty.join("R_0603_1608Metric.kicad_mod")).expect("copy fixture");

    let catalog = FootprintCatalog::from_root(tmp.path()).expect("fixture catalog");
    assert!(catalog.contains(&"Resistor_SMD:R_0603_1608Metric".parse().unwrap()));
    assert_eq!(
        catalog
            .entries_in(&LibraryId::new("Resistor_SMD").unwrap())
            .count(),
        1
    );

    let hits = catalog.search(SearchQuery::new("0603 resistor").limit(3));
    assert!(hits.iter().any(|hit| hit.id.name().contains("0603")));
}
