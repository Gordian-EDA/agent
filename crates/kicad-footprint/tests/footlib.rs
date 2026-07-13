//! Acceptance tests for footprint-library parsing, discovery, and search.
//!
//! Fixture-parse tests run **without** KiCAD installed against the vendored
//! `.kicad_mod` files under `tests/fixtures/footprints/` (see that dir's
//! `ATTRIBUTION.md`). Environment-dependent discovery/search tests gate on a
//! detected KiCAD install and SKIP visibly.

use std::path::{Path, PathBuf};

use kicad_env::KicadEnv;
use kicad_footprint::{
    CourtyardSource, Footprint, FootprintCatalog, FootprintId, LibraryId, PadTechnology,
    SearchQuery,
};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/footprints")
}

fn load(name: &str) -> Footprint {
    Footprint::from_file(fixtures().join(format!("{name}.kicad_mod"))).expect("parse fixture")
}

fn fid(lib_id: &str) -> FootprintId {
    FootprintId::parse(lib_id).expect("valid lib id")
}

/// Approximate float equality for parsed millimetre coordinates.
fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

/// Stage `names` into a `<lib>.pretty` directory under a fresh tempdir, and
/// return the tempdir guard plus the root that holds the `.pretty` dir.
fn staged_pretty(lib: &str, names: &[&str]) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join(format!("{lib}.pretty"));
    std::fs::create_dir(&dir).unwrap();
    for n in names {
        std::fs::copy(
            fixtures().join(format!("{n}.kicad_mod")),
            dir.join(format!("{n}.kicad_mod")),
        )
        .unwrap();
    }
    let root = tmp.path().to_path_buf();
    (tmp, root)
}

// ── fixture parse correctness (no KiCAD needed) ──────────────────────────────

#[test]
fn standalone_parse_has_no_id() {
    // A bare-file parse does not know its library.
    assert!(load("R_0603_1608Metric").id.is_none());
}

#[test]
fn labelled_dwgs_user_line_is_parsed_as_pcb_edge_datum() {
    let source = r#"(footprint "EdgeConnector"
      (version 20240108)
      (generator "test")
      (layer "F.Cu")
      (fp_line (start -5 4.34) (end 5 4.34)
        (stroke (width 0.1) (type solid)) (layer "Dwgs.User"))
      (fp_line (start -2 -2) (end 2 -2)
        (stroke (width 0.1) (type solid)) (layer "Dwgs.User"))
      (fp_text user "PCB Edge" (at 0 3.43) (layer "Dwgs.User")
        (effects (font (size 1 1) (thickness 0.15))))
      (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu" "F.Mask")))"#;
    let fp = Footprint::parse_str("EdgeConnector", source).expect("parse datum fixture");
    let datum = fp.pcb_edge_datum.expect("labelled edge datum");
    assert!(close(datum.start.y, 4.34));
    assert!(close(datum.end.y, 4.34));
}

#[test]
fn unlabelled_dwgs_user_line_is_not_an_edge_datum() {
    let source = r#"(footprint "ConstructionLine"
      (version 20240108)
      (generator "test")
      (layer "F.Cu")
      (fp_line (start -5 4) (end 5 4)
        (stroke (width 0.1) (type solid)) (layer "Dwgs.User"))
      (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu" "F.Mask")))"#;
    let fp = Footprint::parse_str("ConstructionLine", source).expect("parse fixture");
    assert!(fp.pcb_edge_datum.is_none());
}

#[test]
fn arc_courtyard_bulge_is_captured() {
    let fp = load("ArcCourtyard");
    assert_eq!(fp.courtyard_source, CourtyardSource::ExplicitCourtyard);
    assert!(
        fp.courtyard.max_x >= 8.46,
        "right arc bulge missed: max_x={}",
        fp.courtyard.max_x
    );
    assert!(
        fp.courtyard.min_x <= -3.58,
        "left arc bulge missed: min_x={}",
        fp.courtyard.min_x
    );
}

#[test]
fn circle_courtyard_radius_is_captured() {
    let fp = load("CircleCourtyard");
    assert_eq!(fp.courtyard_source, CourtyardSource::ExplicitCourtyard);
    assert!(
        close(fp.courtyard.width(), 8.0),
        "circle width {}",
        fp.courtyard.width()
    );
    assert!(
        close(fp.courtyard.height(), 8.0),
        "circle height {}",
        fp.courtyard.height()
    );
}

#[test]
fn r0603_two_smd_pads_and_rect_courtyard() {
    let fp = load("R_0603_1608Metric");
    assert_eq!(fp.pad_count(), 2);
    assert!(fp.pads.iter().all(|p| p.technology == PadTechnology::Smd));
    assert!(fp.pads.iter().all(|p| !p.is_through_hole()));
    assert!(fp.pads.iter().all(|p| p.drill.is_none()));

    let mut nums: Vec<_> = fp.pads.iter().map(|p| p.number.as_str()).collect();
    nums.sort();
    assert_eq!(nums, ["1", "2"]);
    assert!(
        fp.pads
            .iter()
            .any(|p| close(p.at.x, -0.825) && close(p.at.y, 0.0))
    );
    assert!(
        fp.pads
            .iter()
            .any(|p| close(p.at.x, 0.825) && close(p.at.y, 0.0))
    );
    assert!(
        fp.pads
            .iter()
            .all(|p| close(p.size.x, 0.8) && close(p.size.y, 0.95))
    );
    assert!(fp.pads.iter().all(|p| p.shape == "roundrect"));

    assert_eq!(fp.courtyard_source, CourtyardSource::ExplicitCourtyard);
    assert!(
        close(fp.courtyard.width(), 2.96),
        "{}",
        fp.courtyard.width()
    );
    assert!(
        close(fp.courtyard.height(), 1.46),
        "{}",
        fp.courtyard.height()
    );
}

#[test]
fn sot23_three_smd_pads_and_aggregated_courtyard() {
    let fp = load("SOT-23");
    assert_eq!(fp.pad_count(), 3);
    assert!(fp.pads.iter().all(|p| p.technology == PadTechnology::Smd));
    let mut nums: Vec<_> = fp.pads.iter().map(|p| p.number.as_str()).collect();
    nums.sort();
    assert_eq!(nums, ["1", "2", "3"]);

    assert_eq!(fp.courtyard_source, CourtyardSource::ExplicitCourtyard);
    assert!(close(fp.courtyard.min_x, -1.93) && close(fp.courtyard.max_x, 1.93));
    assert!(
        close(fp.courtyard.width(), 3.86),
        "{}",
        fp.courtyard.width()
    );
    assert!(
        close(fp.courtyard.height(), 3.4),
        "{}",
        fp.courtyard.height()
    );
}

#[test]
fn pinheader_1x02_two_thru_hole_pads_with_drill() {
    let fp = load("PinHeader_1x02_P2.54mm_Vertical");
    assert_eq!(fp.pad_count(), 2);
    assert!(
        fp.pads
            .iter()
            .all(|p| p.technology == PadTechnology::ThruHole)
    );
    assert!(fp.pads.iter().all(|p| p.is_through_hole()));
    assert!(
        fp.pads
            .iter()
            .all(|p| p.drill.is_some_and(|d| close(d, 1.0)))
    );
    assert!(
        fp.pads
            .iter()
            .all(|p| p.copper_layers().iter().any(|l| l == "*.Cu"))
    );
    // Pads on a 2.54 mm pitch along +y.
    assert!(fp.pads.iter().any(|p| close(p.at.y, 0.0)));
    assert!(fp.pads.iter().any(|p| close(p.at.y, 2.54)));
}

#[test]
fn overall_bounds_encloses_courtyard() {
    for name in [
        "R_0603_1608Metric",
        "SOT-23",
        "PinHeader_1x02_P2.54mm_Vertical",
    ] {
        let fp = load(name);
        assert!(fp.bounds.min_x <= fp.courtyard.min_x + 1e-9, "{name}");
        assert!(fp.bounds.max_x >= fp.courtyard.max_x - 1e-9, "{name}");
        assert!(fp.bounds.min_y <= fp.courtyard.min_y + 1e-9, "{name}");
        assert!(fp.bounds.max_y >= fp.courtyard.max_y - 1e-9, "{name}");
    }
}

// ── parse_str + parser edge cases (no KiCAD needed) ──────────────────────────

#[test]
fn parse_str_round_trips_a_fixture() {
    let source = std::fs::read_to_string(fixtures().join("SOT-23.kicad_mod")).unwrap();
    let from_str = Footprint::parse_str("SOT-23", &source).expect("parse_str");
    let from_file = load("SOT-23");
    assert_eq!(from_str.pad_count(), from_file.pad_count());
    assert_eq!(from_str.courtyard_source, from_file.courtyard_source);
}

#[test]
fn parse_str_recovers_custom_pad_extent() {
    // kiutils reports a `custom` pad's `size` as the anchor pad only; the parser
    // must grow it to enclose the primitive polygon (half-extents 1 x 2 -> 2 x 4).
    let source = r#"(footprint "Custom" (layer "F.Cu")
  (pad "1" smd custom (at 0 0) (size 0.5 0.5) (layers "F.Cu")
    (primitives (gr_poly (pts (xy -1 -2) (xy 1 -2) (xy 1 2) (xy -1 2)) (width 0))))
)"#;
    let fp = Footprint::parse_str("Custom", source).expect("parse_str custom pad");
    let pad = &fp.pads[0];
    assert_eq!(pad.shape, "custom");
    assert!(close(pad.size.x, 2.0), "custom pad width {}", pad.size.x);
    assert!(close(pad.size.y, 4.0), "custom pad height {}", pad.size.y);
}

#[test]
fn estimated_courtyard_when_no_crtyd_layer() {
    // No F.CrtYd graphics: courtyard is estimated from pads + silkscreen.
    let source = r#"(footprint "NoCrtYd" (layer "F.Cu")
  (pad "1" smd roundrect (at -1 0) (size 1 1) (layers "F.Cu"))
  (pad "2" smd roundrect (at 1 0) (size 1 1) (layers "F.Cu"))
  (fp_line (start -2 -1) (end 2 -1) (layer "F.SilkS"))
)"#;
    let fp = Footprint::parse_str("NoCrtYd", source).expect("parse_str no crtyd");
    assert_eq!(
        fp.courtyard_source,
        CourtyardSource::EstimatedFromPadsAndSilkscreen
    );
    assert!(fp.courtyard.width() > 0.0 && fp.courtyard.height() > 0.0);
}

// ── catalog over a synthetic `.pretty` layout (no KiCAD needed) ───────────────

#[test]
fn catalog_indexes_pretty_dirs_and_searches() {
    let tmp = tempfile::tempdir().unwrap();
    for (lib, name) in [
        ("Resistor_SMD", "R_0603_1608Metric"),
        ("Package_TO_SOT_SMD", "SOT-23"),
    ] {
        let dir = tmp.path().join(format!("{lib}.pretty"));
        std::fs::create_dir(&dir).unwrap();
        std::fs::copy(
            fixtures().join(format!("{name}.kicad_mod")),
            dir.join(format!("{name}.kicad_mod")),
        )
        .unwrap();
    }

    let catalog = FootprintCatalog::from_root(tmp.path()).unwrap();
    assert_eq!(catalog.library_count(), 2);
    assert_eq!(catalog.len(), 2);
    assert_eq!(
        catalog
            .libraries()
            .map(|l| l.id().as_str())
            .collect::<Vec<_>>(),
        ["Package_TO_SOT_SMD", "Resistor_SMD"]
    );
    let resistor = LibraryId::new("Resistor_SMD").unwrap();
    assert_eq!(
        catalog
            .entries_in(&resistor)
            .map(|e| e.id().to_string())
            .collect::<Vec<_>>(),
        ["Resistor_SMD:R_0603_1608Metric"]
    );

    // Search resolves the hit and lazily parses its pad count.
    let hits = catalog.search(SearchQuery::new("R_0603_1608Metric").limit(5));
    assert_eq!(hits[0].id.to_string(), "Resistor_SMD:R_0603_1608Metric");
    assert_eq!(hits[0].pad_count, Some(2));

    // Lazy detail lookup is tagged with its id.
    let fp = catalog
        .footprint(&fid("Package_TO_SOT_SMD:SOT-23"))
        .unwrap();
    assert_eq!(fp.pad_count(), 3);
    assert_eq!(
        fp.id.as_ref().map(|i| i.to_string()).as_deref(),
        Some("Package_TO_SOT_SMD:SOT-23")
    );

    // An unknown id is NotFound, not a parse failure.
    let err = catalog
        .footprint(&fid("Resistor_SMD:DoesNotExist"))
        .unwrap_err();
    assert!(err.is_not_found(), "{err}");
}

#[test]
fn catalog_search_ordering_is_deterministic() {
    let (_guard, root) = staged_pretty("Resistor_SMD", &["R_0603_1608Metric", "SOT-23"]);
    let catalog = FootprintCatalog::from_root(&root).unwrap();
    let a = catalog.search(SearchQuery::new("0603").limit(5));
    let b = catalog.search(SearchQuery::new("0603").limit(5));
    assert_eq!(
        a, b,
        "identical queries must return identical, ordered hits"
    );
}

#[test]
fn catalog_suggest_offers_closest_name_in_library() {
    let (_guard, root) = staged_pretty("Resistor_SMD", &["R_0603_1608Metric"]);
    let catalog = FootprintCatalog::from_root(&root).unwrap();
    let s: Vec<String> = catalog
        .suggest(&fid("Resistor_SMD:R_0603_1608Metrik"))
        .iter()
        .map(|i| i.to_string())
        .collect();
    assert_eq!(s, ["Resistor_SMD:R_0603_1608Metric"]);
}

#[test]
fn catalog_empty_query_returns_no_hits() {
    let (_guard, root) = staged_pretty("Resistor_SMD", &["R_0603_1608Metric"]);
    let catalog = FootprintCatalog::from_root(&root).unwrap();
    assert!(catalog.search(SearchQuery::new("@@@")).is_empty());
}

#[test]
fn catalog_propagates_parse_errors_distinctly() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("Fixtures.pretty");
    std::fs::create_dir(&dir).unwrap();
    std::fs::copy(
        fixtures().join("R_0603_1608Metric.kicad_mod"),
        dir.join("R_0603_1608Metric.kicad_mod"),
    )
    .unwrap();
    // A `.kicad_mod` whose root is not a footprint -> parse error, not NotFound.
    std::fs::write(dir.join("Broken.kicad_mod"), "(symbol \"NotAFootprint\")").unwrap();

    let catalog = FootprintCatalog::from_root(tmp.path()).unwrap();
    let good = catalog.footprint(&fid("Fixtures:R_0603_1608Metric"));
    assert!(good.is_ok(), "{good:?}");

    let bad = catalog.footprint(&fid("Fixtures:Broken")).unwrap_err();
    assert!(bad.is_parse(), "expected a parse error, got {bad}");
    assert_eq!(bad.path(), Some(dir.join("Broken.kicad_mod").as_path()));
}

#[test]
fn from_env_indexes_the_environment_footprint_dir() {
    // from_env must read `env.footprint_dir`, NOT a symbol-dir-derived sibling.
    let (guard, root) = staged_pretty("Resistor_SMD", &["R_0603_1608Metric"]);
    let env = KicadEnv::with_library_dirs(guard.path().join("symbols"), root);
    let catalog = FootprintCatalog::from_env(&env).unwrap();
    assert!(catalog.contains(&fid("Resistor_SMD:R_0603_1608Metric")));
}

// ── live environment (gated on KiCAD) ────────────────────────────────────────

#[test]
fn live_catalog_sees_many_libraries() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD installation detected");
        return;
    };
    let catalog = FootprintCatalog::from_env(&env).unwrap();
    eprintln!(
        "indexed {} footprints across {} libraries",
        catalog.len(),
        catalog.library_count()
    );
    assert!(catalog.library_count() > 50, "{}", catalog.library_count());
    assert!(catalog.len() > 1000, "{}", catalog.len());
}

#[test]
fn live_search_finds_r0603() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD installation detected");
        return;
    };
    let catalog = FootprintCatalog::from_env(&env).unwrap();
    let hits = catalog.search(SearchQuery::new("R_0603_1608Metric").limit(8));
    assert!(
        hits.iter()
            .any(|h| h.id.to_string() == "Resistor_SMD:R_0603_1608Metric"),
        "{hits:?}"
    );
    assert_eq!(hits[0].pad_count, Some(2), "{:?}", hits[0]);
}

#[test]
fn live_search_fuzzy_finds_sot23() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD installation detected");
        return;
    };
    let catalog = FootprintCatalog::from_env(&env).unwrap();
    let hits = catalog.search(SearchQuery::new("TO_SOT_SMD SOT-23").limit(10));
    assert!(
        hits.iter()
            .any(|h| h.id.to_string() == "Package_TO_SOT_SMD:SOT-23"),
        "{hits:?}"
    );
}
