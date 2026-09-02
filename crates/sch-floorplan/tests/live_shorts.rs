//! Two intended nets must never land on one extracted net — the `shorted` half of
//! [`sch_floorplan::live::verify`].
//!
//! Both cases here are campaign refusals reduced to fixtures. Each is a SECOND
//! `place_parts` onto a sheet the first call already filled, which is the shape the
//! blank-sheet corpus in `net_clearance.rs` cannot reach: the incremental path draws
//! its labels beside content it did not place.
//!
//! SKIPs cleanly without a KiCAD installation.

use std::path::{Path, PathBuf};

use kicad::KicadInstallation;
use sch_check::place_parts::PlacePartsInput;
use sch_doc::SchDoc;
use sch_floorplan::live::{self, PlaceReport};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(format!("tests/fixtures/validation/{name}.place-parts.json"))
}

fn payload(name: &str) -> PlacePartsInput {
    let src = std::fs::read_to_string(fixture(name)).expect("fixture");
    serde_json::from_str(&src).expect("payload parses")
}

/// Place `blocks` in order onto one sheet, as the tool's engine ladder does: every
/// block is gated, and a refusal restores the sheet before the next one starts.
fn place_blocks(env: &KicadInstallation, blocks: &[&str]) -> (SchDoc, Vec<(String, PlaceReport)>) {
    let mut doc = live::blank_sheet().expect("blank sheet");
    let mut reports = Vec::new();
    for name in blocks {
        let input = payload(name);
        // The tool budgets by what the call LEAVES on the sheet, and a truncated search
        // is part of the shape being reproduced — an unbounded run places differently.
        let sheet_parts = doc.symbols().count() + input.parts.len();
        let report = live::place_parts(
            env,
            &mut doc,
            &input,
            Box::new(spine_place::SpinePlace),
            Some(live::PlacementBudget::new(sheet_parts)),
        )
        .unwrap_or_else(|e| panic!("{name}: {e}"));
        reports.push(((*name).to_string(), report));
    }
    (doc, reports)
}

fn assert_no_shorts(blocks: &[&str]) {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCAD installation");
        return;
    };
    if !fixture(blocks[0]).is_file() {
        eprintln!("SKIP: validation corpus not present");
        return;
    }
    let (_, reports) = place_blocks(&env, blocks);
    let mut failures = Vec::new();
    for (name, report) in &reports {
        for (a, b) in &report.mismatch.shorted {
            failures.push(format!("{name}: shorted {a}+{b}"));
        }
        for net in &report.mismatch.scattered {
            failures.push(format!("{name}: scattered {net}"));
        }
        if !report.committed && report.mismatch.is_empty() {
            failures.push(format!("{name}: refused with no mismatch"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// `campaign-stm32-buck`: the 26-part MCU core placed after the buck stage refused with
/// `shorted 3V3+NRST` — a label seated onto the 3V3 rail's own geometry.
#[test]
fn mcu_core_after_the_buck_stage_is_truthful() {
    assert_no_shorts(&["campaign-stm32-power", "campaign-stm32-mcu-core"]);
}

/// `campaign-bms-10s`: the pull-up network reduced to its cause. `Device:R_Network04`
/// puts four pins at half-grid pitch, so `I2C_SCL`'s pennant — nudged 2.54 mm clear of
/// the body — landed exactly on `I2C_SDA`'s, and two global labels at one coordinate are
/// one net (`shorted I2C_SCL+I2C_SDA`, under BOTH spine and cluster).
#[test]
fn a_pullup_network_seats_one_pennant_per_anchor() {
    assert_no_shorts(&["i2c-pullup-network"]);
}

/// The same network placed BESIDE existing content, where nothing is reframed: the
/// incremental path must seat its pennants clear of the sheet it did not draw.
#[test]
fn a_pullup_network_beside_existing_content_is_truthful() {
    assert_no_shorts(&["campaign-stm32-power", "i2c-pullup-network"]);
}

/// A multi-unit 100-pin MCU whose ports are its GPIO bank, added to a populated sheet —
/// the shape `campaign-stm32-buck` refused as `shorted GND+PA7`, where the SAME net was
/// labelled twice and one of the two anchors landed on a ground pin.
#[test]
fn a_multiunit_mcu_beside_existing_content_is_truthful() {
    assert_no_shorts(&["campaign-stm32-power", "stm32-multiunit-core"]);
}
