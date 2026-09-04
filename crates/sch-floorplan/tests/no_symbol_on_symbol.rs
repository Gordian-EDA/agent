//! A symbol drawn on top of another symbol is never a legal sheet.
//!
//! The fixture is a real agent run's finished Blue Pill sheet: C1 and F1 (block POWER)
//! sit inside the body of U2 (block MCU), and every tool call that produced it reported
//! success. The two failures that let that stand are pinned here.
//!
//! SKIPs without a KiCad installation (the typesetter measures real symbols).

use std::path::Path;

use kicad::KicadInstallation;
use sch_doc::SchDoc;
use sch_floorplan::live::{self, Selection};
use sch_floorplan::visual;

const SHEET: &str = "tests/fixtures/validation/agent-run-bluepill.kicad_sch";

fn sheet() -> SchDoc {
    SchDoc::read(Path::new(SHEET)).unwrap()
}

/// The lint the fixture is here for. Everything below is about what the engine does
/// while this is true of the sheet it is editing.
#[test]
fn the_fixture_carries_the_overlap_the_run_produced() {
    assert_eq!(
        visual::body_overlaps(&sheet()),
        vec![
            ["C1".to_string(), "U2".to_string()],
            ["F1".to_string(), "U2".to_string()],
        ]
    );
}

/// `arrange` restores its snapshot when the wire redraw cannot keep the netlist — which
/// undoes the PLACEMENT too, not just the redraw. Reporting the selection as `moved`
/// anyway is how a caller comes to believe it has re-laid a block out, call after call,
/// while the drawing never changes: the run asked three times and was told "done" three
/// times with the two parts still on top of the MCU.
#[test]
fn an_arrange_that_rolls_back_reports_nothing_moved() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCad environment detected");
        return;
    };
    let mut doc = sheet();
    let power = ["F1", "C1", "U1", "C2", "C3"].map(String::from).to_vec();
    let report = live::arrange(&env, &mut doc, &Selection::Refs(power), None, None).unwrap();

    let unchanged = report
        .warnings
        .iter()
        .any(|warning| warning.contains("NOTHING WAS MOVED"));
    assert!(
        unchanged,
        "this fixture is here because its redraw falls back; warnings={:?}",
        report.warnings
    );
    assert!(
        report.moved.is_empty(),
        "the placement was rolled back, so nothing moved: {:?}",
        report.moved
    );
    assert!(report.left_bench.is_empty());
}

/// A sheet that already carries an overlap must stay editable: the gate refuses the
/// overlaps an edit CREATES, never the ones it inherits, or the only tool that could
/// repair the sheet would be the first one refused.
#[test]
fn an_inherited_overlap_does_not_refuse_the_next_edit() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCad environment detected");
        return;
    };
    let mut doc = sheet();
    let clock = ["C6", "Y1", "C7", "C8", "Y2", "C9"]
        .map(String::from)
        .to_vec();
    let report = live::arrange(&env, &mut doc, &Selection::Refs(clock), None, None)
        .expect("an inherited overlap is not this call's fault");
    assert!(report.committed);
}

/// The gate's contract, on every block of a real sheet: an `arrange` may repair an
/// overlap and may leave one it inherited, but it may never ADD a pair. Before the gate
/// this was only ever counted as a readability warning on the way to a commit.
#[test]
fn no_arrange_adds_an_overlapping_pair() {
    let Some(env) = KicadInstallation::detect() else {
        eprintln!("SKIP: no KiCad environment detected");
        return;
    };
    let blocks: [&[&str]; 4] = [
        &["F1", "C1", "U1", "C2", "C3"],
        &["U2", "C4", "C5", "R1", "SW1"],
        &["C6", "Y1", "C7", "C8", "Y2", "C9"],
        &["R3", "JP1", "R4", "JP2", "R5", "SW2"],
    ];
    for refs in blocks {
        let mut doc = sheet();
        let before = visual::body_overlaps(&doc);
        let refs: Vec<String> = refs.iter().map(|r| (*r).to_string()).collect();
        match live::arrange(&env, &mut doc, &Selection::Refs(refs.clone()), None, None) {
            Ok(_) => {
                for pair in visual::body_overlaps(&doc) {
                    assert!(before.contains(&pair), "{refs:?} left {pair:?} overlapping");
                }
            }
            Err(live::Error::BodyOverlap(overlaps)) => {
                // The refusal is the other legal answer, and it has to name the parts.
                assert!(!overlaps.0.is_empty());
                assert_eq!(visual::body_overlaps(&doc), before, "a refusal rolls back");
            }
            Err(other) => panic!("unexpected error: {other}"),
        }
    }
}

/// The refusal has to tell the model which two parts collided and what to do next.
#[test]
fn the_refusal_names_the_parts_and_the_move() {
    let overlaps = live::Overlaps(vec![["C1".to_string(), "U2".to_string()]]);
    let message = overlaps.to_string();
    assert!(message.contains("C1 on U2"), "{message}");
    assert!(message.contains("nothing was changed"), "{message}");
    assert!(message.contains("arrange"), "{message}");
}
