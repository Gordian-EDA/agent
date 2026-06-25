//! Acceptance tests for footprint-library parsing, discovery, and search.
//!
//! Fixture-parse tests run **without** KiCAD installed against the vendored
//! `.kicad_mod` files under `tests/fixtures/footprints/` (see that dir's
//! `ATTRIBUTION.md`). Environment-dependent discovery/search tests gate on a
//! detected KiCAD install and SKIP visibly.

use std::path::{Path, PathBuf};

use kicad_cli::env::KicadEnv;
use kicad_footprint::{CourtyardSource, Footprint, FootprintIndex, PadTechnology};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/footprints")
}

fn load(name: &str) -> Footprint {
    Footprint::load(&fixtures().join(format!("{name}.kicad_mod"))).expect("parse fixture")
}

/// Approximate float equality for parsed millimetre coordinates.
fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

// ── fixture parse correctness (no KiCAD needed) ──────────────────────────────

#[test]
fn arc_courtyard_bulge_is_captured() {
    // kiutils 0.3 drops an fp_arc's (mid) apex; the courtyard parser must still
    // bound the bulge from start/end. This footprint's right-end arc bulges to
    // x=8.47 and the left to x=-3.59 → courtyard x must span [-3.59, 8.47].
    let fp = load("ArcCourtyard");
    assert_eq!(fp.courtyard_source, CourtyardSource::Crtyd);
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
    // A circular courtyard ((center 1.25 0) (end 5.25 0) → r=4) must bound the
    // whole disc x=[-2.75, 5.25], not just the center+edge sliver — else a radial
    // cap / round footprint is badly under-sized and overlaps its neighbours.
    let fp = load("CircleCourtyard");
    assert_eq!(fp.courtyard_source, CourtyardSource::Crtyd);
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
    assert!(fp.pads.iter().all(|p| p.drill.is_none()));

    // Pad numbers and the two symmetric offsets from the file.
    let mut nums: Vec<_> = fp.pads.iter().map(|p| p.number.as_str()).collect();
    nums.sort();
    assert_eq!(nums, ["1", "2"]);
    assert!(
        fp.pads
            .iter()
            .any(|p| close(p.at[0], -0.825) && close(p.at[1], 0.0))
    );
    assert!(
        fp.pads
            .iter()
            .any(|p| close(p.at[0], 0.825) && close(p.at[1], 0.0))
    );
    assert!(
        fp.pads
            .iter()
            .all(|p| close(p.size[0], 0.8) && close(p.size[1], 0.95))
    );
    assert!(fp.pads.iter().all(|p| p.shape == "roundrect"));

    // Single F.CrtYd fp_rect: (-1.48,-0.73)..(1.48,0.73) → 2.96 × 1.46 mm.
    assert_eq!(fp.courtyard_source, CourtyardSource::Crtyd);
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

    // Courtyard is drawn as many F.CrtYd fp_lines; the aggregated bbox spans
    // x[-1.93, 1.93] (3.86) and y[-1.7, 1.7] (3.4).
    assert_eq!(fp.courtyard_source, CourtyardSource::Crtyd);
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
    // 1.0 mm drill on both pads; both span the full copper stack (*.Cu).
    assert!(
        fp.pads
            .iter()
            .all(|p| p.drill.is_some_and(|d| close(d, 1.0)))
    );
    assert!(fp.pads.iter().all(|p| p.layers.iter().any(|l| l == "*.Cu")));
    // Pads on a 2.54 mm pitch along +y.
    assert!(fp.pads.iter().any(|p| close(p.at[1], 0.0)));
    assert!(fp.pads.iter().any(|p| close(p.at[1], 2.54)));
}

#[test]
fn overall_bbox_encloses_courtyard() {
    for name in [
        "R_0603_1608Metric",
        "SOT-23",
        "PinHeader_1x02_P2.54mm_Vertical",
    ] {
        let fp = load(name);
        assert!(fp.bbox.min_x <= fp.courtyard.min_x + 1e-9, "{name}");
        assert!(fp.bbox.max_x >= fp.courtyard.max_x - 1e-9, "{name}");
        assert!(fp.bbox.min_y <= fp.courtyard.min_y + 1e-9, "{name}");
        assert!(fp.bbox.max_y >= fp.courtyard.max_y - 1e-9, "{name}");
    }
}

// ── index over the fixture dir (no KiCAD needed) ─────────────────────────────
//
// The fixtures dir is a flat dir of `.kicad_mod` files, not `.pretty` dirs, so
// it exercises `Footprint::load` but not `.pretty` discovery — that is covered
// by the live-environment tests below. Here we build an index over a tiny
// synthetic `.pretty` layout to test discovery + search without KiCAD.

#[test]
fn build_indexes_pretty_dirs_and_searches() {
    let tmp = tempfile::tempdir().unwrap();
    let lib = tmp.path().join("Resistor_SMD.pretty");
    std::fs::create_dir(&lib).unwrap();
    std::fs::copy(
        fixtures().join("R_0603_1608Metric.kicad_mod"),
        lib.join("R_0603_1608Metric.kicad_mod"),
    )
    .unwrap();
    let sot = tmp.path().join("Package_TO_SOT_SMD.pretty");
    std::fs::create_dir(&sot).unwrap();
    std::fs::copy(
        fixtures().join("SOT-23.kicad_mod"),
        sot.join("SOT-23.kicad_mod"),
    )
    .unwrap();

    let idx = FootprintIndex::build_from_dir(tmp.path()).unwrap();
    assert_eq!(idx.library_count(), 2);
    assert_eq!(idx.len(), 2);
    assert_eq!(
        idx.libraries().collect::<Vec<_>>(),
        ["Package_TO_SOT_SMD", "Resistor_SMD"]
    );
    assert_eq!(
        idx.footprints_in("Resistor_SMD"),
        ["Resistor_SMD:R_0603_1608Metric"]
    );

    // Search resolves the hit and lazily parses its pad count.
    let hits = idx.search("R_0603_1608Metric", 5);
    assert_eq!(hits[0].lib_id, "Resistor_SMD:R_0603_1608Metric");
    assert_eq!(hits[0].pad_count, 2);

    // Lazy detail lookup works through the index too.
    let fp = idx.footprint("Package_TO_SOT_SMD:SOT-23").unwrap();
    assert_eq!(fp.pad_count(), 3);
    assert!(idx.footprint("Resistor_SMD:DoesNotExist").is_none());
}

#[test]
fn search_ordering_is_deterministic() {
    let tmp = tempfile::tempdir().unwrap();
    let lib = tmp.path().join("Resistor_SMD.pretty");
    std::fs::create_dir(&lib).unwrap();
    for n in ["R_0603_1608Metric", "SOT-23"] {
        std::fs::copy(
            fixtures().join(format!("{n}.kicad_mod")),
            lib.join(format!("{n}.kicad_mod")),
        )
        .unwrap();
    }
    let idx = FootprintIndex::build_from_dir(tmp.path()).unwrap();
    let a = idx.search("0603", 5);
    let b = idx.search("0603", 5);
    assert_eq!(
        a, b,
        "identical queries must return identical, ordered hits"
    );
}

#[test]
fn suggest_offers_closest_name_in_library() {
    let tmp = tempfile::tempdir().unwrap();
    let lib = tmp.path().join("Resistor_SMD.pretty");
    std::fs::create_dir(&lib).unwrap();
    std::fs::copy(
        fixtures().join("R_0603_1608Metric.kicad_mod"),
        lib.join("R_0603_1608Metric.kicad_mod"),
    )
    .unwrap();
    let idx = FootprintIndex::build_from_dir(tmp.path()).unwrap();
    // A near-miss name within an existing library yields a did-you-mean.
    let s = idx.suggest("Resistor_SMD:R_0603_1608Metrik");
    assert_eq!(s, ["Resistor_SMD:R_0603_1608Metric"]);
}

#[test]
fn empty_query_returns_no_hits() {
    let tmp = tempfile::tempdir().unwrap();
    let lib = tmp.path().join("Resistor_SMD.pretty");
    std::fs::create_dir(&lib).unwrap();
    std::fs::copy(
        fixtures().join("R_0603_1608Metric.kicad_mod"),
        lib.join("R_0603_1608Metric.kicad_mod"),
    )
    .unwrap();
    let idx = FootprintIndex::build_from_dir(tmp.path()).unwrap();
    assert!(idx.search("@@@", 5).is_empty());
}

// ── live environment (gated on KiCAD) ────────────────────────────────────────

#[test]
fn live_index_sees_many_libraries() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD installation detected");
        return;
    };
    let idx = FootprintIndex::build(&env).unwrap();
    eprintln!(
        "indexed {} footprints across {} libraries",
        idx.len(),
        idx.library_count()
    );
    assert!(
        idx.library_count() > 50,
        "{} libraries",
        idx.library_count()
    );
    assert!(idx.len() > 1000, "{} footprints", idx.len());
}

#[test]
fn live_search_finds_r0603() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD installation detected");
        return;
    };
    let idx = FootprintIndex::build(&env).unwrap();
    let hits = idx.search("R_0603_1608Metric", 8);
    assert!(
        hits.iter()
            .any(|h| h.lib_id == "Resistor_SMD:R_0603_1608Metric"),
        "{hits:?}"
    );
    assert_eq!(hits[0].pad_count, 2, "{:?}", hits[0]);
}

#[test]
fn live_search_fuzzy_finds_sot23() {
    let Some(env) = KicadEnv::detect() else {
        eprintln!("SKIP: no KiCAD installation detected");
        return;
    };
    let idx = FootprintIndex::build(&env).unwrap();
    // Fragment of the qualified id with separator noise, like the symbol
    // search's USB-C test — fuzzy matching must still surface the exact part.
    let hits = idx.search("TO_SOT_SMD SOT-23", 10);
    assert!(
        hits.iter().any(|h| h.lib_id == "Package_TO_SOT_SMD:SOT-23"),
        "{hits:?}"
    );
}
