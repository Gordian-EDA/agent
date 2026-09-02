//! place — the IR → millimetre realization, split into cohesive submodules that share one
//! flat namespace (re-exported below so every `floorplan::place::…` path resolves
//! verbatim):
//!
//! - [`emit`] — gather + grid seed + engine orchestration + `SchematicWriter` assembly.
//! - [`score`] — the routed `count_*` crossing/merge/short terms + the geometry primitives
//!   the [`measure`] library reads off a built sheet.
//! - [`measure`] — the routed-sheet realiser and the [`sch_model::engine::CandidateEvaluator`]
//!   it implements: the oracle an engine crate asks what a candidate would cost. The
//!   OBJECTIVE (the weights) and the SEARCH live in the engine crates, not here.
//! - [`route`] — the orthogonal elbow router + power-rail riser planning.
//!
//! Realizing a sheet is heavy + non-algorithmic (how to draw and measure), so it lives
//! here as a shared library; the cost weights and the search are engine-owned method.

mod emit;
mod measure;
mod route;
mod score;

pub use emit::*;
pub use measure::*;
pub use score::*;
// `route` is the orthogonal router — every item is crate-internal (none was `pub`
// pre-split), so re-export it crate-visibly, not publicly.
pub(crate) use route::*;

#[cfg(test)]
mod grid_tests {
    use super::*;
    use sch_model::route::DrawnSegment;
    use geom::{Dir, Rect};
    use indexmap::IndexMap;
    use sch_check::model::{Block, Component, Design, LayoutGrid};
    use sch_model::ir::Side;

    fn cells(names: &[&str]) -> Vec<Option<String>> {
        names
            .iter()
            .map(|n| {
                if *n == "~" {
                    None
                } else {
                    Some((*n).to_string())
                }
            })
            .collect()
    }

    fn block(refs: &[&str], layout: LayoutGrid) -> Block {
        let mut components = IndexMap::new();
        for r in refs {
            components.insert((*r).to_string(), Component::default());
        }
        Block {
            note: None,
            components,
            layout,
        }
    }

    #[test]
    fn per_block_grids_compose_into_column_bands_with_spans_and_holes() {
        let mut design = Design::default();
        // block `usb`: J1 | hole | R1  -> width 3
        design.blocks.insert(
            "usb".into(),
            block(&["J1", "R1"], vec![cells(&["J1", "~", "R1"])]),
        );
        // block `mcu`: U1 spans its column across two rows (repeated) -> first occ.
        design.blocks.insert(
            "mcu".into(),
            block(&["U1"], vec![cells(&["U1"]), cells(&["U1"])]),
        );

        let g = grid_from_layout(&design);
        // usb band starts at col 0; the `~` hole reserves col 1. Boxes are
        // [col_min, row_min, col_max, row_max].
        assert_eq!(g["J1"], [0, 0, 0, 0]);
        assert_eq!(g["R1"], [2, 0, 2, 0]);
        // mcu band starts AFTER usb's 3 columns (no overlap); U1 SPANS rows 0..1 in
        // its column (repeated down it), so its box grows in the row axis.
        assert_eq!(g["U1"], [3, 0, 3, 1]);
        assert_eq!(g.len(), 3);

        let occurrences = grid_occurrences(&design);
        assert_eq!(occurrences["J1"], vec![(0, 0)]);
        assert_eq!(occurrences["R1"], vec![(2, 0)]);
        assert_eq!(occurrences["U1"], vec![(3, 0), (3, 1)]);
    }

    #[test]
    fn collinear_body_crossing_fires_on_passthrough_not_on_series() {
        // Vertical 2-pin part, pins at (10,0) (top) and (10,10) (bottom).
        let body = vec![([10.0, 0.0], [10.0, 10.0])];
        let w = |a: [f64; 2], b: [f64; 2]| DrawnSegment::new(a.into(), b.into(), None);
        // A wire on the SAME line (x=10) running from above the top pin to below the
        // bottom pin slices straight THROUGH the part — 1 crossing.
        assert_eq!(
            count_collinear_body_crossings(&body, &[w([10.0, -5.0], [10.0, 15.0])]),
            1
        );
        // A correctly-drawn series part: leads STOP at each pin (two segments, neither
        // spanning beyond both pins) — 0.
        assert_eq!(
            count_collinear_body_crossings(
                &body,
                &[w([10.0, -5.0], [10.0, 0.0]), w([10.0, 10.0], [10.0, 15.0])],
            ),
            0
        );
        // A parallel wire on a DIFFERENT line (x=20) is not collinear — 0.
        assert_eq!(
            count_collinear_body_crossings(&body, &[w([20.0, -5.0], [20.0, 15.0])]),
            0
        );
        // A PERPENDICULAR wire (handled by count_body_crossings, not this) — 0 here.
        assert_eq!(
            count_collinear_body_crossings(&body, &[w([0.0, 5.0], [20.0, 5.0])]),
            0
        );
        // A wire reaching one pin from outside but stopping inside the body — 0.
        assert_eq!(
            count_collinear_body_crossings(&body, &[w([10.0, -5.0], [10.0, 5.0])]),
            0
        );
    }

    #[test]
    fn block_without_layout_contributes_nothing() {
        let mut design = Design::default();
        design
            .blocks
            .insert("main".into(), block(&["U1", "R1"], Vec::new()));
        assert!(grid_from_layout(&design).is_empty());
    }

    #[test]
    fn dir_to_side_inverts_side_dir() {
        for s in [Side::Left, Side::Right, Side::Top, Side::Bottom] {
            assert_eq!(dir_to_side(side_dir(s)), s);
        }
    }

    #[test]
    fn single_pin_port_exit_follows_pin_with_short_reach() {
        // The h-bridge regression: a lone WEST-facing gate pin must seat its port
        // pennant on a SHORT stub to its own side (clear of its body and of the
        // next symbol in a packed row), NOT the long multi-pin reach.
        let west = [([20.0, 0.0], Dir::West)];
        assert_eq!(
            port_exit_point(&west, Side::Left),
            [geom::GRID_50_MIL.snap(20.0 - 2.54), 0.0]
        );
        let east = [([20.0, 0.0], Dir::East)];
        assert_eq!(
            port_exit_point(&east, Side::Right),
            [geom::GRID_50_MIL.snap(20.0 + 2.54), 0.0]
        );
        // A multi-pin port keeps the longer reach so it clears the last pin.
        let two = [([20.0, 0.0], Dir::East), ([24.0, 0.0], Dir::East)];
        assert_eq!(
            port_exit_point(&two, Side::Right),
            [geom::GRID_50_MIL.snap(24.0 + 7.62), 0.0]
        );
    }

    #[test]
    fn forced_single_port_wire_requires_the_original_clear_short_stub() {
        let clear = sch_model::route::RouteScene {
            solids: Vec::new(),
            points: Vec::new(),
            segments: Vec::new(),
            label_solids: Vec::new(),
        };
        assert!(safe_forced_single_port_stub(
            [10.0, 10.0].into(),
            [12.54, 10.0].into(),
            "OUT1",
            &clear,
        ));
        assert!(!safe_forced_single_port_stub(
            [10.0, 10.0].into(),
            [35.4, 10.0].into(),
            "OUT1",
            &clear,
        ));

        let blocked = sch_model::route::RouteScene {
            solids: vec![Rect::new(10.5, 9.0, 12.0, 11.0)],
            points: Vec::new(),
            segments: Vec::new(),
            label_solids: Vec::new(),
        };
        assert!(!safe_forced_single_port_stub(
            [10.0, 10.0].into(),
            [12.54, 10.0].into(),
            "OUT1",
            &blocked,
        ));
    }

    #[test]
    fn vdd_side_power_glyph_points_away_from_served_body() {
        // A west-facing VDD pin sits on the left edge of its symbol body. The VDD
        // arrow must extend west into open space, not east back through the body.
        let body = Rect::new(0.0, -2.0, 8.0, 2.0);
        let angle = choose_power_angle("VDD", Dir::West, [0.0, 0.0], &[body]);
        let glyph = Rect::new(-3.0, -1.5, 0.0, 1.5);
        assert_eq!(angle, 90.0);
        assert!(!glyph.overlaps(&body));
    }

    #[test]
    fn gnd_side_power_glyph_points_away_from_served_body() {
        // GND's triangle extends the opposite way from a VDD arrow at the same
        // angle, so west-facing ground keeps the old 270-degree orientation.
        let body = Rect::new(0.0, -2.0, 8.0, 2.0);
        assert_eq!(
            choose_power_angle("GND", Dir::West, [0.0, 0.0], &[body]),
            270.0
        );
    }

    #[test]
    fn power_angle_chooser_uses_clear_local_side_when_conventional_side_is_blocked() {
        // If the conventional outward side is occupied by a neighbouring body,
        // rotate the glyph to any clear side instead of creating a visual overlap.
        let west_neighbor = Rect::new(-8.0, -2.0, 0.0, 2.0);
        let angle = choose_power_angle("VDD", Dir::West, [0.0, 0.0], &[west_neighbor]);
        let glyph = power_glyph_box("VDD", [0.0, 0.0], angle);
        assert_ne!(angle, 90.0);
        assert!(!glyph.overlaps(&west_neighbor));
    }

    #[test]
    fn power_flag_direction_follows_collision_avoiding_glyph() {
        let west_neighbor = Rect::new(-8.0, -2.0, 0.0, 2.0);
        let angle = choose_power_angle("VDD", Dir::West, [0.0, 0.0], &[west_neighbor]);
        let glyph_dir = power_glyph_dir("VDD", angle);
        assert_eq!(glyph_dir, Dir::East);
        assert_eq!(flag_angle(glyph_dir), 270.0);
    }

    #[test]
    fn pwr_flag_splits_from_power_marker_on_adjacent_taps() {
        let eps = [
            ([10.0, 0.0], Dir::North),
            ([12.54, 0.0], Dir::North),
            ([30.0, 0.0], Dir::North),
        ];
        assert_eq!(split_flag_power_pair(&eps, 5.08), Some((0, 1)));
    }

    #[test]
    fn pwr_flag_split_requires_nearby_collinear_taps() {
        let diagonal = [([10.0, 0.0], Dir::North), ([12.54, 2.54], Dir::North)];
        let far = [([10.0, 0.0], Dir::North), ([20.32, 0.0], Dir::North)];
        assert_eq!(split_flag_power_pair(&diagonal, 5.08), None);
        assert_eq!(split_flag_power_pair(&far, 5.08), None);
    }
}
