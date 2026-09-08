//! Placement invariants on the staged Blue Pill board: legal by construction, connectors on the
//! edges the caller asked for, decoupling caps beside their hub.

use std::collections::BTreeMap;
use std::path::Path;

use pcb_auto::geom::BBox;
use pcb_auto::model::Board;
use pcb_auto::place::{PlanOptions, apply, plan_placement, suggest_outline};

fn staged() -> (Board, f64, f64) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bluepill_staging.kicad_pcb");
    let mut board = Board::load(&path).expect("fixture loads");
    let (w, h) = suggest_outline(&board, 1.5, 0.6, 1.0);
    board.set_outline_rect(10.0, 10.0, w, h, 0.0);
    (board, w, h)
}

fn options() -> PlanOptions {
    let mut opts = PlanOptions { seat_connectors: true, seed: 1, ..Default::default() };
    for (r, e) in [("J2", "left"), ("J3", "right"), ("J1", "top")] {
        opts.edge_for.insert(r.into(), e.into());
    }
    opts
}

/// Gap between two boxes, 0 when they touch or overlap.
fn box_gap(a: &BBox, b: &BBox) -> f64 {
    let dx = (a.x0 - b.x1).max(b.x0 - a.x1).max(0.0);
    let dy = (a.y0 - b.y1).max(b.y0 - a.y1).max(0.0);
    dx.hypot(dy)
}

#[test]
fn outline_matches_the_reference() {
    let (_board, w, h) = staged();
    // the Python `suggest_outline` on the same board
    assert!((w - 55.8).abs() <= 0.1, "width {w}");
    assert!((h - 55.8).abs() <= 0.1, "height {h}");
}

#[test]
fn plan_is_legal_and_shorter() {
    let (mut board, _, _) = staged();
    let plan = plan_placement(&board, &options());
    assert!(plan.unplaced.is_empty(), "unplaced: {:?}", plan.unplaced);
    assert_eq!(plan.overlaps_after, 0, "overlaps: {:?}", plan.notes);
    assert!(
        plan.wirelength_after < plan.wirelength_before,
        "{} -> {}",
        plan.wirelength_before,
        plan.wirelength_after
    );

    apply(&mut board, &plan);
    let outline = board.outline_bbox().expect("outline");
    for f in board.footprints() {
        let c = f.courtyard_bbox();
        assert!(
            c.x0 >= outline.x0 - 1e-6
                && c.y0 >= outline.y0 - 1e-6
                && c.x1 <= outline.x1 + 1e-6
                && c.y1 <= outline.y1 + 1e-6,
            "{} is outside the board: {c:?} vs {outline:?}",
            f.ref_
        );
    }
}

#[test]
fn connectors_take_the_edges_they_asked_for() {
    let (board, _, _) = staged();
    let plan = plan_placement(&board, &options());
    for (r, e) in [("J2", "left"), ("J3", "right"), ("J1", "top")] {
        let seat = plan.seated.get(r).unwrap_or_else(|| panic!("{r} not seated"));
        assert_eq!(seat.side_used, e, "{r} seated {}", seat.side_used);
        assert!(seat.gap_mm >= 0.0 && seat.gap_mm < 2.0, "{r} gap {}", seat.gap_mm);
    }
}

/// The +3V3/GND caps are satellites: each one sits against the multi-pad part on its rail it was
/// slotted beside (the LDO, the MCU or a header), not out in the cloud.
#[test]
fn decoupling_caps_land_beside_their_hub() {
    let (mut board, _, _) = staged();
    let plan = plan_placement(&board, &options());
    apply(&mut board, &plan);
    let boxes: BTreeMap<String, BBox> = board
        .footprints()
        .into_iter()
        .map(|f| (f.ref_.clone(), f.courtyard_bbox()))
        .collect();
    for c in ["C1", "C2", "C3", "C4", "C5"] {
        let cap = &boxes[c];
        let d = ["U1", "U2", "J2", "J4", "JP1", "JP2"]
            .iter()
            .map(|h| box_gap(cap, &boxes[*h]))
            .fold(f64::INFINITY, f64::min);
        // a decoupler is a satellite of whichever +3V3 part it was slotted beside; the bar is
        // "hugging a courtyard", not a fixed millimetre
        assert!(d <= 8.0, "{c} is {d:.2} mm from the nearest +3V3 hub");
    }
}

/// The whole point of the seeded restarts: one board and one seed give one plan.
#[test]
fn a_seed_gives_one_plan() {
    let (board, _, _) = staged();
    let a = plan_placement(&board, &options());
    let b = plan_placement(&board, &options());
    assert_eq!(a.moves, b.moves);
    assert_eq!(a.wirelength_after, b.wirelength_after);
}

/// Within a quarter of the Python reference planner on the same fixture (it gets 1528 mm).
#[test]
fn wirelength_tracks_the_reference() {
    let (board, _, _) = staged();
    let plan = plan_placement(&board, &options());
    assert!(
        plan.wirelength_after < 1528.1 * 1.25,
        "wirelength {} mm",
        plan.wirelength_after
    );
}
