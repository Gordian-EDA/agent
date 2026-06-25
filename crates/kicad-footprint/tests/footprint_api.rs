use kicad_footprint::{CourtyardSource, Footprint, FootprintIndex, PadTechnology};

#[test]
fn footprint_crate_exposes_parser_and_search_api() {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/footprints/R_0603_1608Metric.kicad_mod");
    let fp = Footprint::load(&fixture).expect("fixture footprint");

    assert_eq!(fp.name, "R_0603_1608Metric");
    assert_eq!(fp.pad_count(), 2);
    assert_eq!(fp.courtyard_source, CourtyardSource::Crtyd);
    assert!(
        fp.pads
            .iter()
            .all(|pad| pad.technology == PadTechnology::Smd)
    );

    let tmp = tempfile::tempdir().expect("temp dir");
    let pretty = tmp.path().join("Resistor_SMD.pretty");
    std::fs::create_dir(&pretty).expect("pretty dir");
    std::fs::copy(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/footprints/R_0603_1608Metric.kicad_mod"),
        pretty.join("R_0603_1608Metric.kicad_mod"),
    )
    .expect("copy fixture");
    let index = FootprintIndex::build_from_dir(tmp.path()).expect("fixture index");
    let hits = index.search("0603 resistor", 3);
    assert!(hits.iter().any(|hit| hit.lib_id.contains("R_0603")));
}
