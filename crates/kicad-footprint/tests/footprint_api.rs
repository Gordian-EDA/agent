use kicad_footprint::{
    CourtyardSource, Footprint, FootprintCatalog, LibraryId, PadTechnology, SearchQuery,
    unknown_footprint_message,
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

/// A catalog over freshly-created empty `.kicad_mod` files; suggestion
/// ranking never parses footprint bodies.
fn suggestion_catalog(tmp: &std::path::Path, lib_ids: &[&str]) -> FootprintCatalog {
    for lib_id in lib_ids {
        let (lib, name) = lib_id.split_once(':').expect("Lib:Name fixture id");
        let pretty = tmp.join(format!("{lib}.pretty"));
        std::fs::create_dir_all(&pretty).expect("pretty dir");
        std::fs::write(pretty.join(format!("{name}.kicad_mod")), "").expect("fixture file");
    }
    FootprintCatalog::from_root(tmp).expect("fixture catalog")
}

fn suggested(catalog: &FootprintCatalog, id: &str) -> Vec<String> {
    catalog
        .suggest(id)
        .iter()
        .map(ToString::to_string)
        .collect()
}

#[test]
fn suggest_recovers_from_wrong_or_invented_library() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let catalog = suggestion_catalog(
        tmp.path(),
        &[
            "Button_Switch_SMD:SW_SPST_TL3342",
            "Button_Switch_SMD:SW_SPST_PTS645",
            "Package_LGA:Bosch_LGA-8_2.5x2.5mm_P0.65mm_ClockwisePinNumbering",
            "Package_LGA:Bosch_LGA-8_3x3mm_P0.8mm_ClockwisePinNumbering",
            "Package_LGA:LGA-12_2x2mm_P0.5mm",
            "Resistor_SMD:R_0603_1608Metric",
            "Resistor_SMD:R_0805_2012Metric",
            "Battery:BatteryHolder_Keystone_3002_1x2032",
        ],
    );

    // Right name, invented library: the exact-name match ranks first.
    assert_eq!(
        suggested(&catalog, "Button_SMD_SW_SPST:SW_SPST_TL3342").first(),
        Some(&"Button_Switch_SMD:SW_SPST_TL3342".to_string())
    );

    // Right library, invented part-prefixed name: the near-identical
    // dimension tokens surface the real footprint.
    assert!(
        suggested(&catalog, "Package_LGA:BME280_LGA-8_2.5x2.5mm_P0.65mm")
            .contains(&"Package_LGA:Bosch_LGA-8_2.5x2.5mm_P0.65mm_ClockwisePinNumbering".into())
    );

    // Invented generic name in a real library still yields the family.
    assert!(
        suggested(&catalog, "Battery:BatteryHolder_CoinCell_CR2032")
            .contains(&"Battery:BatteryHolder_Keystone_3002_1x2032".into())
    );

    // In-library typo keeps its did-you-mean.
    assert_eq!(
        suggested(&catalog, "Resistor_SMD:R_0805_2012Metri").first(),
        Some(&"Resistor_SMD:R_0805_2012Metric".to_string())
    );

    // Nothing close: no candidates rather than arbitrary neighbors.
    assert!(suggested(&catalog, "Zzz:Qqqwww").is_empty());

    // Missing-colon shape: the embedded name suffix wins.
    assert_eq!(
        catalog
            .suggest("Device_R_0805")
            .first()
            .map(ToString::to_string),
        Some("Resistor_SMD:R_0805_2012Metric".to_string())
    );
}

#[test]
fn suggest_recovers_live_mistakes_against_system_library() {
    let Some(installation) = kicad::KicadInstallation::detect() else {
        eprintln!("SKIP: no configured KiCad 10 footprint library");
        return;
    };
    let root = installation.footprint_dir();
    let catalog = FootprintCatalog::from_root(root).expect("system catalog");

    assert_eq!(
        suggested(&catalog, "Button_SMD_SW_SPST:SW_SPST_TL3342").first(),
        Some(&"Button_Switch_SMD:SW_SPST_TL3342".to_string())
    );
    assert!(
        suggested(&catalog, "Package_LGA:BME280_LGA-8_2.5x2.5mm_P0.65mm")
            .contains(&"Package_LGA:Bosch_LGA-8_2.5x2.5mm_P0.65mm_ClockwisePinNumbering".into())
    );
    let battery = suggested(&catalog, "Battery:BatteryHolder_CoinCell_CR2032");
    assert!(
        battery.iter().any(|s| s.starts_with("Battery:")),
        "battery holder mistake must suggest the Battery family, got {battery:?}"
    );
    assert!(
        catalog
            .suggest("Device_R_0805")
            .iter()
            .any(|id| id.to_string() == "Resistor_SMD:R_0805_2012Metric")
    );
}

#[test]
fn suggest_adds_the_missing_solder_jumper_pad_shape() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let catalog = suggestion_catalog(
        tmp.path(),
        &[
            "Jumper:SolderJumper-2_P1.3mm_Open_Pad1.0x1.5mm",
            "Jumper:SolderJumper-2_P1.3mm_Open_RoundedPad1.0x1.5mm",
            "Jumper:SolderJumper-2_P1.3mm_Open_TrianglePad1.0x1.5mm",
        ],
    );

    let suggestions = suggested(&catalog, "Jumper:SolderJumper-2_P1.3mm_Open");
    assert_eq!(suggestions.len(), 3);
    assert!(
        suggestions
            .iter()
            .all(|id| id.contains("Jumper:SolderJumper-2_P1.3mm_Open_")),
        "expected pad-shape suffixed ids, got {suggestions:?}"
    );
    let ids = catalog.suggest("Jumper:SolderJumper-2_P1.3mm_Open");
    let message = unknown_footprint_message("Jumper:SolderJumper-2_P1.3mm_Open", &ids);
    assert!(
        message.starts_with(
            "unknown footprint 'Jumper:SolderJumper-2_P1.3mm_Open'; did you mean \
             Jumper:SolderJumper-2_P1.3mm_Open_"
        ),
        "unexpected diagnostic: {message}"
    );
}
